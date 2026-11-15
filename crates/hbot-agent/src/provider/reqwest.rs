//! 真实 HTTP 传输：把 `reqwest` 的流式响应接到 [`HttpTransport`] 上。
//!
//! 契约（见 [`HttpTransport`]）：把响应体**分块**交给 [`ResponseSink`]，
//! 不在内存里攒完整响应；取消后立刻返回且不再继续读。
//!
//! 三条安全约束，改这个文件前先读：
//!
//! 1. **API key 绝不进错误信息**。`reqwest::Error` 的 `Display` 会带 URL，这里一律
//!    `without_url()`；服务端返回的错误体在拼进消息前，先把请求头里的凭据抹掉
//!    （见 [`redact_secrets`]）。
//! 2. **总期限**用 [`HttpRequest::timeout`] 包住整次请求（连接 + 读流），
//!    不依赖 `Client` 的 per-request timeout（那个只覆盖到响应头）。
//! 3. **状态码走 [`ProviderError::from_status`]**，不在这里重写一套映射——
//!    "4xx 不可重试 / 429·5xx 可重试"只有一处真源。

use std::time::Duration;

use async_trait::async_trait;
use hbot_core::HbotError;

use super::ProviderError;
use super::openai::{HttpRequest, HttpTransport, ResponseSink};
use crate::cancel::CancellationToken;

/// 连接超时。总期限仍由 [`HttpRequest::timeout`] 决定。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 读服务端错误体时的上限（错误体只用于诊断，不该把整页 HTML 读进内存）。
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

/// 短于这个长度的"凭据"不做替换：避免把 `abc` 这类普通子串抹得满屏 `<redacted>`。
const MIN_SECRET_LEN: usize = 8;

/// 基于 `reqwest` 的 [`HttpTransport`]。
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// 新建传输（内部只建一个连接池，可复用）。
    pub fn new() -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| {
                ProviderError::fatal(HbotError::internal(format!(
                    "无法初始化 HTTP 客户端：{error}"
                )))
            })?;
        Ok(Self { client })
    }

    /// 用现成的 client 构造（测试或需要自定义连接池时用）。
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// 一次请求的实体：把响应体分块喂给 `sink`。
    async fn stream(
        &self,
        request: HttpRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ResponseSink,
        secrets: &[String],
    ) -> Result<(), ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }

        let mut builder = self.client.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let mut response = builder
            .body(request.body)
            .send()
            .await
            .map_err(|error| map_transport_error(error, request.timeout))?;

        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(&mut response).await;
            let redacted = redact_secrets(&body, secrets);
            return Err(ProviderError::from_status(status.as_u16(), &redacted));
        }

        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| map_transport_error(error, request.timeout))?
        {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            sink.on_chunk(&chunk)?;
        }
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        Ok(())
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
        sink: &mut dyn ResponseSink,
    ) -> Result<(), ProviderError> {
        let deadline = request.timeout;
        let timeout_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        let secrets = secrets_of(&request.headers);
        let stream = self.stream(request, &cancel, sink, &secrets);
        match tokio::time::timeout(deadline, stream).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::timeout("provider 请求", timeout_ms)),
        }
    }
}

/// 从请求头里取凭据，供错误体脱敏用。
///
/// 只认 `Authorization` / `X-Api-Key` / `Api-Key` 三种（本 crate 只会发第一种）；
/// `Authorization: Bearer x` 取 `x`，其它形式取整值。
fn secrets_of(headers: &[(String, String)]) -> Vec<String> {
    let mut secrets = Vec::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower != "authorization" && lower != "x-api-key" && lower != "api-key" {
            continue;
        }
        let token = value
            .split_once(' ')
            .map_or_else(|| value.trim(), |(_, rest)| rest.trim());
        if token.chars().count() >= MIN_SECRET_LEN {
            secrets.push(token.to_string());
        }
    }
    secrets
}

/// 把已知凭据从文本里抹掉。不改变其余内容，方便照常定位问题。
fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// 读错误体，最多 [`MAX_ERROR_BODY_BYTES`]；读失败就返回已读到的部分。
async fn read_error_body(response: &mut reqwest::Response) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ERROR_BODY_BYTES.saturating_sub(buffer.len());
                if room == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) => break,
            // 错误体只是诊断信息：读不出来不算新的失败原因，用已读到的部分。
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// `reqwest` 错误 → [`ProviderError`]。
///
/// 去掉 URL 再拼消息（查询串可能带临时凭证）；`is_connect` / `is_timeout`
/// 视为可重试，其余（协议错误、TLS 配置错误）不可重试。
fn map_transport_error(error: reqwest::Error, timeout: Duration) -> ProviderError {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let timed_out = error.is_timeout();
    let connect = error.is_connect();
    let error = error.without_url();
    if timed_out {
        return ProviderError::timeout("provider 请求", timeout_ms);
    }
    let message = format!("provider 传输失败：{error}");
    if connect {
        ProviderError::retryable(HbotError::internal(message))
    } else {
        ProviderError::fatal(HbotError::internal(message))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct NullSink;

    impl ResponseSink for NullSink {
        fn on_chunk(&mut self, _chunk: &[u8]) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn authorization(key: &str) -> (String, String) {
        ("Authorization".to_string(), format!("Bearer {key}"))
    }

    #[test]
    fn extracts_bearer_token_from_authorization() {
        let headers = vec![authorization("sk-test-1234567890")];
        let secrets = secrets_of(&headers);
        assert_eq!(secrets, vec!["sk-test-1234567890".to_string()]);
    }

    #[test]
    fn ignores_headers_that_are_not_credentials() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        assert!(secrets_of(&headers).is_empty());
    }

    #[test]
    fn redacts_every_occurrence_of_the_key() {
        let body = "Incorrect API key provided: sk-test-1234567890 (again sk-test-1234567890)";
        let redacted = redact_secrets(body, &["sk-test-1234567890".to_string()]);
        assert!(!redacted.contains("sk-test-1234567890"));
        assert!(redacted.contains("<redacted>"));
        assert!(redacted.contains("Incorrect API key provided"));
    }

    #[test]
    fn short_values_are_not_treated_as_secrets() {
        let headers = vec![authorization("abc")];
        assert!(secrets_of(&headers).is_empty());
    }

    /// 传输失败的消息里不得出现 API key（回环地址，不触外网）。
    #[tokio::test]
    async fn transport_failure_does_not_leak_the_api_key() {
        let transport = ReqwestTransport::new().expect("client");
        let key = "sk-test-secret-0123456789";
        let request = HttpRequest {
            url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                authorization(key),
            ],
            body: "{}".to_string(),
            timeout: Duration::from_secs(5),
        };
        let cancel = CancellationToken::new();
        let mut sink = NullSink;
        let error = transport
            .post(request, cancel, &mut sink)
            .await
            .expect_err("回环地址上不可能有服务");
        let message = error.error.to_string();
        assert!(!message.contains(key), "错误信息泄露了 API key：{message}");
        assert!(message.contains("provider 传输失败") || message.contains("provider 请求"));
    }

    /// 预取消的请求一个字节都不发。
    #[tokio::test]
    async fn pre_cancelled_request_returns_cancelled() {
        let transport = ReqwestTransport::new().expect("client");
        let request = HttpRequest {
            url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
            headers: Vec::new(),
            body: "{}".to_string(),
            timeout: Duration::from_secs(5),
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut sink = NullSink;
        let error = transport
            .post(request, cancel, &mut sink)
            .await
            .expect_err("已取消");
        assert_eq!(error.error.code(), -32005);
    }
}
