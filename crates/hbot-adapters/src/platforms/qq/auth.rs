//! 取 access token 与缓存（单飞 + 提前续期）。
//!
//! # 协议（实测 2026-11-16）
//!
//! ```text
//! POST https://bots.qq.com/app/getAppAccessToken
//! content-type: application/json
//!
//! {"appId":"<app_id>","clientSecret":"<client_secret>"}
//!
//! 200 {"access_token":"<78 字符>","expires_in":6627}
//! ```
//!
//! 两个容易踩的点：
//!
//! 1. `expires_in` 是**剩余**有效期，不是「新 token 能活 7200 秒」。平台会复用还没
//!    过期的 token，此时返回的就是剩下的秒数（实测 6627）。所以续期窗口一律按
//!    `expires_in` 算，代码里不出现 `7200` 这个数字。
//! 2. 并发取 token 会互相浪费配额，也可能把旧 token 顶掉。这里用一把
//!    [`tokio::sync::Mutex`] 把「检查 + 取 + 写回」做成**单飞**：拿锁期间别的调用者
//!    在等，醒来时缓存已经有效，直接复用。
//!
//! # 续期失败时保留旧 token
//!
//! 如果新 token 取不到（网络抖动、平台 5xx）但手里这个还没到 `expires_at`，就继续用
//! 旧的并 `warn!`。否则一次抖动会让整个适配器停摆几分钟。
//! 但为了避免每来一条消息都去打平台，失败后 5 秒内不再尝试续期。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use hbot_core::{Clock, Timestamp};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::AdapterError;
use crate::platforms::qq::config::QqConfig;
use crate::platforms::qq::http::{self, HttpRequest};

/// 续期失败后的重试冷却：这段时间内不再打平台，继续用旧 token。
const REFRESH_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// 一个 access token 及其时长。
///
/// `Debug` 只打长度，不打值（token 是凭据，`AGENTS.md` §4.4）。
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    value: String,
    expires_in: Duration,
}

impl AccessToken {
    /// 用原始值构造。
    fn new(value: String, expires_in: Duration) -> Self {
        Self { value, expires_in }
    }

    /// 平台声明的有效期（**剩余**秒数，不是固定 7200）。
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// token 的字符长度（用于日志与探测，不泄漏值）。
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// token 是否为空（正常不会）。
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// 原始 token 值，用于拼 `Authorization: QQBot <token>`。
    ///
    /// 调用方**不要**把它写进日志或错误串。
    pub(crate) fn access_token(&self) -> &str {
        &self.value
    }

    /// `Authorization` 头的完整值（`QQBot <token>`）。
    pub(crate) fn authorization_header(&self) -> String {
        format!("QQBot {}", self.value)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field(
                "value",
                &format_args!("<redacted:{} chars>", self.value.len()),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// 缓存里的一条 token。
#[derive(Clone, Debug)]
struct CachedToken {
    token: AccessToken,
    /// 从这一刻起可以续期。
    refresh_at: Timestamp,
    /// 到这一刻 token 一定失效。
    expires_at: Timestamp,
}

/// 取 token 的提供者（单飞 + 缓存）。
///
/// 由 [`crate::platforms::qq::api::ApiClient`] 与网关发现共享；`Clone` 不需要，
/// 外面包一层 [`Arc`]。
pub(crate) struct TokenProvider {
    app_id: String,
    client_secret: String,
    token_url: String,
    request_timeout: Duration,
    extra_roots: Vec<Vec<u8>>,
    refresh_margin: Duration,
    clock: Arc<dyn Clock>,
    state: Mutex<TokenState>,
}

/// 受锁保护的那部分状态。
#[derive(Default)]
struct TokenState {
    cached: Option<CachedToken>,
    /// 上一次续期失败的时刻；冷却期内不再尝试。
    next_refresh_attempt: Option<Timestamp>,
}

impl TokenProvider {
    /// 从配置构造（每个客户端一份）。
    pub(crate) fn new(config: &QqConfig) -> Self {
        Self {
            app_id: config.app_id().to_owned(),
            client_secret: config.client_secret().to_owned(),
            token_url: config.token_url().to_owned(),
            request_timeout: config.request_timeout(),
            extra_roots: config.extra_root_certificates().to_vec(),
            refresh_margin: config.token_refresh_margin(),
            clock: config.clock_arc(),
            state: Mutex::new(TokenState::default()),
        }
    }

    /// 拿一个可用的 token（必要时取新的）。
    ///
    /// 单飞：整个「检查 → 取 → 写回」都在锁里，并发调用只会发一次请求。
    pub(crate) async fn access_token(&self) -> Result<AccessToken, AdapterError> {
        let mut state = self.state.lock().await;
        let now = self.clock.now();
        if let Some(cached) = state.cached.as_ref() {
            if before(now, cached.refresh_at) {
                return Ok(cached.token.clone());
            }
            // 还在冷却期内：不打平台，继续用旧 token（它可能仍然有效）。
            if let Some(next) = state.next_refresh_attempt {
                if before(now, next) && before(now, cached.expires_at) {
                    return Ok(cached.token.clone());
                }
            }
        }
        self.fetch_into(&mut state, now).await
    }

    /// 强制换一个新 token（用于「平台说 401/403」之后的唯一一次重试）。
    ///
    /// `stale` 是被平台拒绝的那个 token：如果缓存已经被别人换过了，直接用新的，
    /// 不再多打一次平台。
    pub(crate) async fn renew(&self, stale: &AccessToken) -> Result<AccessToken, AdapterError> {
        let mut state = self.state.lock().await;
        if let Some(cached) = state.cached.as_ref() {
            if cached.token != *stale {
                return Ok(cached.token.clone());
            }
        }
        let now = self.clock.now();
        self.fetch_into(&mut state, now).await
    }

    /// 真的去平台取一次并写回缓存。
    ///
    /// 失败时：旧 token 还没硬过期就保留它（只 `warn!`），否则把错误往上抛。
    async fn fetch_into(
        &self,
        state: &mut TokenState,
        now: Timestamp,
    ) -> Result<AccessToken, AdapterError> {
        match self.fetch().await {
            Ok(token) => {
                let (refresh_after, expires_after) =
                    refresh_windows(token.expires_in(), self.refresh_margin);
                let cached = CachedToken {
                    refresh_at: add(now, refresh_after),
                    expires_at: add(now, expires_after),
                    token: token.clone(),
                };
                state.cached = Some(cached);
                state.next_refresh_attempt = None;
                tracing::debug!(
                    token_len = token.len(),
                    expires_in_secs = token.expires_in().as_secs(),
                    "qq access token refreshed"
                );
                Ok(token)
            }
            Err(error) => {
                state.next_refresh_attempt = Some(add(now, REFRESH_RETRY_COOLDOWN));
                match state.cached.as_ref() {
                    Some(cached) if before(now, cached.expires_at) => {
                        tracing::warn!(
                            error = %error,
                            "qq access token refresh failed; keeping the cached token until it expires"
                        );
                        Ok(cached.token.clone())
                    }
                    _ => Err(error),
                }
            }
        }
    }

    /// 打一次取 token 端点。
    async fn fetch(&self) -> Result<AccessToken, AdapterError> {
        let body = serde_json::json!({
            "appId": self.app_id,
            "clientSecret": self.client_secret,
        });
        let mut request = HttpRequest::new("POST", self.token_url.clone(), self.request_timeout)
            .header("accept", "application/json")
            .json_body(&body)?;
        request.extra_roots = self.extra_roots.clone();
        let response = http::request(request).await?;
        if !response.is_success() {
            return Err(token_status_error(response.status, &response.body));
        }
        let payload = response.json()?;
        let value = payload
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AdapterError::protocol("qq access token response is missing `access_token`")
            })?;
        let expires_in = payload
            .get("expires_in")
            .and_then(parse_expires_in)
            .ok_or_else(|| {
                AdapterError::protocol("qq access token response is missing a usable `expires_in`")
            })?;
        Ok(AccessToken::new(
            value.to_owned(),
            Duration::from_secs(expires_in),
        ))
    }
}

/// 把 `expires_in` 解析成正数秒（容忍字符串形式的数字）。
fn parse_expires_in(value: &Value) -> Option<u64> {
    let seconds = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    (seconds > 0).then_some(seconds)
}

/// 计算 `(refresh_after, expires_after)`。
///
/// 提前量取 `min(margin, expires_in / 2)`：平台可能返回一个很小的 `expires_in`
/// （实测见过 6627，但复用中的 token 会一直变小），提前量超过一半就永远在续期了。
fn refresh_windows(expires_in: Duration, margin: Duration) -> (Duration, Duration) {
    let expires = expires_in.max(Duration::from_millis(1));
    let margin = margin.min(expires / 2);
    (expires.saturating_sub(margin), expires)
}

/// `base + delta`（毫秒精度，溢出饱和）。
fn add(base: Timestamp, delta: Duration) -> Timestamp {
    let millis = i64::try_from(delta.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_unix_millis(base.unix_millis().saturating_add(millis))
}

/// `a` 是否严格早于 `b`。
fn before(a: Timestamp, b: Timestamp) -> bool {
    a.unix_millis() < b.unix_millis()
}

/// 取 token 失败时的状态码映射。
///
/// 错误串里**只有状态码与平台数字 `code`**：不贴响应正文，也不贴凭据 —— 平台有时会
/// 在错误 body 里回显请求的一部分。
fn token_status_error(status: u16, body: &[u8]) -> AdapterError {
    let code = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("code").and_then(Value::as_i64));
    let suffix = match code {
        Some(code) => format!(" (platform code {code})"),
        None => String::new(),
    };
    match status {
        401 | 403 => AdapterError::protocol(format!(
            "qq access token request was rejected with status {status}: app id or client secret is not accepted{suffix}"
        )),
        429 => AdapterError::limit(format!(
            "qq access token request was rate limited with status {status}{suffix}"
        )),
        500..=599 => AdapterError::transport(format!(
            "qq access token endpoint failed with status {status}{suffix}"
        )),
        _ => AdapterError::protocol(format!(
            "qq access token request failed with status {status}{suffix}"
        )),
    }
}

/// 一次 token 探测的结果：**不含 token 值**。
///
/// 给验收/自检用：只报告「拿到了、多长、还剩多久」，避免把凭据打出来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenProbe {
    /// token 的字符长度。
    pub token_len: usize,
    /// 平台声明的剩余有效期。
    pub expires_in: Duration,
}

/// 只取一次 access token 并立刻丢弃，用于验证凭据与端点是否可用。
///
/// 这个函数**不保存** token，也不把它放进返回值 —— 返回值只有长度与有效期。
/// 供 `tests/qq_real.rs`（`#[ignore]`，需要真凭据）和运维自检使用。
pub async fn probe_access_token(config: &QqConfig) -> Result<TokenProbe, AdapterError> {
    config.validate()?;
    let provider = TokenProvider::new(config);
    let token = provider.access_token().await?;
    Ok(TokenProbe {
        token_len: token.len(),
        expires_in: token.expires_in(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_debug_redacts_the_value() {
        let token = AccessToken::new("sk-test-DEADBEEF".to_owned(), Duration::from_secs(60));
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains("sk-test-DEADBEEF"),
            "token leaked: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        assert_eq!(token.len(), "sk-test-DEADBEEF".len());
        assert!(!token.is_empty());
        assert_eq!(token.authorization_header(), "QQBot sk-test-DEADBEEF");
    }

    #[test]
    fn refresh_window_leaves_the_documented_margin() {
        // 实测值：expires_in = 6627，margin = 300 → 第 6327 秒起开始续期。
        let (refresh, expires) =
            refresh_windows(Duration::from_secs(6627), Duration::from_secs(300));
        assert_eq!(expires, Duration::from_secs(6627));
        assert_eq!(refresh, Duration::from_secs(6327));
    }

    #[test]
    fn refresh_window_never_exceeds_half_of_a_short_lifetime() {
        let (refresh, expires) = refresh_windows(Duration::from_secs(4), Duration::from_secs(300));
        assert_eq!(expires, Duration::from_secs(4));
        assert_eq!(refresh, Duration::from_secs(2));

        // 0 秒按 1ms 处理，避免零长度的续期窗口；提前量仍是一半（500µs），
        // 所以 refresh_after 落在 500µs 而不是 0 —— 只要严格小于 expires 就不会退化。
        let (refresh, expires) = refresh_windows(Duration::ZERO, Duration::from_secs(300));
        assert_eq!(expires, Duration::from_millis(1));
        assert_eq!(refresh, Duration::from_micros(500));
        assert!(refresh < expires, "a refresh window must stay non-empty");
    }

    #[test]
    fn parse_expires_in_accepts_numbers_and_numeric_strings() {
        assert_eq!(parse_expires_in(&serde_json::json!(6627)), Some(6627));
        assert_eq!(parse_expires_in(&serde_json::json!("6627")), Some(6627));
        assert_eq!(parse_expires_in(&serde_json::json!(0)), None);
        assert_eq!(parse_expires_in(&serde_json::json!("abc")), None);
        assert_eq!(parse_expires_in(&serde_json::json!(null)), None);
    }

    #[test]
    fn token_status_error_never_echoes_the_response_body() {
        let body = br#"{"code":100016,"message":"sk-test-DEADBEEF"}"#;
        let error = token_status_error(401, body);
        let text = error.to_string();
        assert!(!text.contains("sk-test-DEADBEEF"), "body leaked: {text}");
        assert!(text.contains("401"));
        assert!(text.contains("100016"));
    }
}
