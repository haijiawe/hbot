//! QQ OpenAPI 的调用层：一个地方管「带 token 的请求 + 401 续期重试一次 + 状态码映射」。
//!
//! # 为什么值得单独一层
//!
//! 取 token、查网关地址、发被动回复三个调用都要带同一个头
//! （`Authorization: QQBot <token>`），也都会遇到「token 被平台提前判过期」——
//! 那时要**续一次期并重试一次**。这套逻辑只写在这里一份；分散到调用方就会变成
//! 有的地方无限重试、有的地方干脆不重试。
//!
//! # 重试上限（刻意）
//!
//! - 401/403：**最多**续期 1 次 + 重试 1 次。还 401 就直接报错，绝不循环 —— 循环会
//!   把「凭据被撤权」变成打平台的死循环，也会让测试里的请求计数不可断言。
//! - 429/5xx：**不自动重试**。被动回复是「用户点一下、机器人答一句」，重试要由上层
//!   决定（[`crate::platforms::qq::send`] 有策略）；这一层只如实报错。
//!
//! # 错误信息里不许有凭据
//!
//! 平台错误 body 可能回显请求的一部分（社区报告过），所以这里只保留**状态码**和
//! 平台给的**数字 `code`**，绝不贴 `message`/`body`/token。

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::AdapterError;
use crate::platforms::qq::auth::{AccessToken, TokenProvider};
use crate::platforms::qq::config::QqConfig;
use crate::platforms::qq::http::{self, HttpRequest};

/// 一个带鉴权的 QQ OpenAPI 客户端（可跨任务共享，`Arc` 包一层）。
pub(crate) struct ApiClient {
    /// token 来源（单飞缓存）；网关 WS 也要用它，所以外面持有同一个 `Arc`。
    auth: Arc<TokenProvider>,
    /// OpenAPI 基址，例如 `https://api.sgroup.qq.com`。
    api_base: String,
    /// 单次请求超时。
    request_timeout: Duration,
    /// 追加信任根（测试的自签证书）。
    extra_roots: Vec<Vec<u8>>,
}

/// 一次尝试的结果。
enum Attempt {
    /// 拿到 2xx 与 JSON 正文。
    Ok(Value),
    /// 401/403：token 不被接受（可能只是过期，也可能被撤权）。
    AuthRejected,
}

impl ApiClient {
    /// 从配置与共享的 token 提供者构造。
    pub(crate) fn new(config: &QqConfig, auth: Arc<TokenProvider>) -> Self {
        Self {
            auth,
            api_base: config.api_base().trim_end_matches('/').to_owned(),
            request_timeout: config.request_timeout(),
            extra_roots: config.extra_root_certificates().to_vec(),
        }
    }

    /// 共享的 token 提供者（网关 WS 握手要用）。
    pub(crate) fn auth(&self) -> &Arc<TokenProvider> {
        &self.auth
    }

    /// `GET` 一个绝对 URL（网关发现用）。
    pub(crate) async fn get_json(&self, url: &str) -> Result<Value, AdapterError> {
        self.send_json("GET", url, None).await
    }

    /// `POST` 一个路径（相对 [`Self::api_base`]）或绝对 URL。
    pub(crate) async fn post_json(
        &self,
        path_or_url: &str,
        body: &Value,
    ) -> Result<Value, AdapterError> {
        let url = self.resolve(path_or_url);
        self.send_json("POST", &url, Some(body)).await
    }

    /// 拼出绝对 URL（已经是 `http(s)://` 的原样返回）。
    fn resolve(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_owned()
        } else if path_or_url.starts_with('/') {
            format!("{}{path_or_url}", self.api_base)
        } else {
            format!("{}/{}", self.api_base, path_or_url)
        }
    }

    /// 「取 token → 发 → 401 时续一次并重试一次」。
    async fn send_json(
        &self,
        method: &'static str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, AdapterError> {
        let token = self.auth.access_token().await?;
        match self.attempt(method, url, body, &token).await? {
            Attempt::Ok(value) => return Ok(value),
            Attempt::AuthRejected => {
                tracing::debug!(
                    method,
                    path = url,
                    "qq api rejected the access token; refreshing once and retrying"
                );
            }
        }
        // 唯一的续期重试：新 token 还不被接受就报错（不再循环）。
        let token = self.auth.renew(&token).await?;
        match self.attempt(method, url, body, &token).await? {
            Attempt::Ok(value) => Ok(value),
            Attempt::AuthRejected => Err(AdapterError::protocol(format!(
                "qq api rejected the access token even after refreshing it ({method} {url}): \
                 the bot may have been removed from the app, or the credentials were revoked"
            ))),
        }
    }

    /// 发一次请求并分类状态码。
    async fn attempt(
        &self,
        method: &'static str,
        url: &str,
        body: Option<&Value>,
        token: &AccessToken,
    ) -> Result<Attempt, AdapterError> {
        let mut request = HttpRequest::new(method, url.to_owned(), self.request_timeout)
            .header("authorization", token.authorization_header())
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json_body(body)?;
        }
        request.extra_roots = self.extra_roots.clone();
        let response = http::request(request).await?;
        if response.is_success() {
            return Ok(Attempt::Ok(response.json()?));
        }
        if matches!(response.status, 401 | 403) {
            return Ok(Attempt::AuthRejected);
        }
        Err(status_error(method, url, response.status, &response.body))
    }
}

/// 非 2xx → [`AdapterError`]。
///
/// 401/403 由调用方走续期重试，这里只兜底（正常到不了）。错误串里只有方法、路径、
/// 状态码与平台数字 `code`。
pub(crate) fn status_error(method: &str, url: &str, status: u16, body: &[u8]) -> AdapterError {
    let code = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("code").and_then(Value::as_i64));
    let suffix = match code {
        Some(code) => format!(" (platform code {code})"),
        None => String::new(),
    };
    match status {
        401 | 403 => AdapterError::protocol(format!(
            "qq api rejected the credentials for {method} {url} (status {status}){suffix}"
        )),
        429 => AdapterError::limit(format!(
            "qq api rate limited {method} {url} (status {status}){suffix}"
        )),
        500..=599 => AdapterError::transport(format!(
            "qq api failed on {method} {url} with status {status}{suffix}"
        )),
        _ => AdapterError::protocol(format!(
            "qq api rejected {method} {url} with status {status}{suffix}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_error_maps_statuses_and_never_echoes_the_body() {
        let body = br#"{"code":40054005,"message":"sk-test-DEADBEEF"}"#;

        let error = status_error("POST", "/v2/groups/g/messages", 429, body);
        assert!(matches!(error, AdapterError::Limit { .. }), "{error:?}");
        assert!(error.to_string().contains("429"));
        assert!(error.to_string().contains("40054005"));

        let error = status_error("POST", "/x", 502, body);
        assert!(matches!(error, AdapterError::Transport { .. }), "{error:?}");

        let error = status_error("POST", "/x", 400, body);
        assert!(matches!(error, AdapterError::Protocol { .. }), "{error:?}");

        for status in [400u16, 403, 429, 502] {
            let text = status_error("POST", "/x", status, body).to_string();
            assert!(!text.contains("sk-test-DEADBEEF"), "body leaked: {text}");
        }
    }

    #[test]
    fn resolve_keeps_absolute_urls_and_joins_paths() {
        let config = QqConfig::new("app", "secret").with_api_base("http://127.0.0.1:9/");
        let client = ApiClient::new(&config, Arc::new(TokenProvider::new(&config)));
        assert_eq!(
            client.resolve("/v2/groups/g/messages"),
            "http://127.0.0.1:9/v2/groups/g/messages"
        );
        assert_eq!(
            client.resolve("v2/users/u/messages"),
            "http://127.0.0.1:9/v2/users/u/messages"
        );
        assert_eq!(
            client.resolve("https://bots.qq.com/app/getAppAccessToken"),
            "https://bots.qq.com/app/getAppAccessToken"
        );
    }
}
