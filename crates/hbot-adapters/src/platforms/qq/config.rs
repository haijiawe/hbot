//! QQ 官方机器人（QQ 开放平台）适配器的配置。
//!
//! # 默认值从哪来
//!
//! 标注「实测」的项是 2026-11-16 用真凭据握手一次得到的观测值；标「未核实」的项
//! 来自公开协议文档/社区实现，**没有**在真网关上逐条验证过。
//!
//! | 项 | 默认值 | 来源 |
//! |---|---|---|
//! | 取 token | `POST https://bots.qq.com/app/getAppAccessToken` | 实测：200 |
//! | Token 请求体 | `{"appId":…,"clientSecret":…}` | 实测：被接受 |
//! | Token 响应 | `{"access_token":…,"expires_in":…}` | 实测：`access_token` 长 78、`expires_in` 6627 |
//! | OpenAPI 基址 | `https://api.sgroup.qq.com` | 实测 |
//! | 网关发现 | `GET {api_base}/gateway`（`Authorization: QQBot <token>`） | 实测：200，`url` = `wss://api.sgroup.qq.com/websocket/` |
//! | 鉴权头 | `Authorization: QQBot <access_token>` | 实测：被接受 |
//! | `intents` | `1 << 25`（GROUP_AND_C2C_EVENT） | 实测：网关接受并回 `READY` |
//! | `shard` | `[0, 1]` | 实测：网关接受 |
//! | 心跳间隔 | **不写死**，取 Hello 帧的 `d.heartbeat_interval` | 实测：本次 41250ms（不是 45000） |
//! | `expires_in` 语义 | **剩余**有效期（平台复用未过期 token 时不是 7200） | 实测：6627 |
//!
//! # 一个刻意的默认：`intents` 只有 `1 << 25`
//!
//! 多声明没有权限的位会被平台拒绝（社区报告是 close code `4914` / `4915`，**未核实**），
//! 所以默认只声明「群 @ 消息 + C2C 消息」这一位，其余一律由调用方显式
//! [`QqConfig::with_intents`] 打开。频道（`AT_MESSAGE_CREATE`）需要额外的公域消息位
//! （社区文档写 `1 << 30`，**未核实**），默认不开。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use hbot_core::Clock;

use crate::error::AdapterError;
use crate::platforms::onebot::backoff::{Backoff, Jitter, NoJitter};

/// 取 access token 的默认端点（实测可用）。
pub const DEFAULT_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// QQ OpenAPI 的默认基址（实测可用）。
pub const DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";

/// 网关发现端点在 [`DEFAULT_API_BASE`] 下的路径（实测：`GET /gateway` → `{"url":"wss://…"}`）。
pub const DEFAULT_GATEWAY_PATH: &str = "/gateway";

/// 默认 intents：`1 << 25` = `GROUP_AND_C2C_EVENT`（群 @ 消息与 C2C 消息）。
///
/// 实测该值被网关接受并收到 `READY`。其余位（频道公域消息、成员变更等）默认不开，
/// 因为多声明没有权限的位会被平台拒绝，见模块文档。
pub const DEFAULT_INTENTS: u64 = 1 << 25;

/// 默认分片：[`0, 1`]（单分片）。实测被网关接受。
pub const DEFAULT_SHARD: [u32; 2] = [0, 1];

/// 默认事件通道容量（与 OneBot 适配器同量级，`docs/02-architecture.md` §4）。
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// 默认 `msg_seq` 表容量：按 `msg_id` 记住「这个事件已经回复到第几条」。
///
/// 有界是为了不随会话长度无限增长；被动回复只在事件后几分钟内有意义（**未核实**
/// 具体有效期），所以几百条足够。
pub const DEFAULT_MSG_SEQ_CAPACITY: usize = 512;

/// 默认提前续期窗口：剩这么多就换新 token。
///
/// 平台给的 `expires_in` 是**剩余**有效期（实测 6627s），所以窗口要按剩余值取
/// `min(本值, expires_in / 2)`，不能假设 7200。
pub const DEFAULT_TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// 默认连接超时：TCP 连接、（`wss://` 时）TLS 握手、WS 升级、等 Hello 帧都算在里面。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 默认单次 HTTPS 请求超时（取 token / 网关发现 / 发消息）。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 环境变量名：AppID。
pub const ENV_APP_ID: &str = "HBOT_QQ_APP_ID";

/// 环境变量名：AppSecret。**只读进内存**，不要把它写进日志/仓库。
pub const ENV_APP_SECRET: &str = "HBOT_QQ_APP_SECRET";

/// QQ 官方机器人适配器的配置。
///
/// 构造后交给 [`crate::platforms::qq::QqClient::start`]。默认端点指向 QQ 开放平台的
/// 真实地址；测试用 [`QqConfig::with_token_url`] / [`QqConfig::with_api_base`] /
/// [`QqConfig::with_gateway_url`] 把它们指到进程内假网关。
///
/// `Debug` 会把 `client_secret` 打成 `<redacted>`（`AGENTS.md` §4.4「密钥统一脱敏」）。
#[derive(Clone)]
pub struct QqConfig {
    app_id: String,
    client_secret: String,
    intents: u64,
    shard: [u32; 2],
    token_url: String,
    api_base: String,
    gateway_url: Option<String>,
    connect_timeout: Duration,
    request_timeout: Duration,
    event_capacity: usize,
    msg_seq_capacity: usize,
    token_refresh_margin: Duration,
    heartbeat_timeout: Option<Duration>,
    extra_root_certificates: Vec<Vec<u8>>,
    backoff: Backoff,
    jitter: Arc<dyn Jitter>,
    clock: Arc<dyn Clock>,
}

impl QqConfig {
    /// 用 AppID 与 AppSecret 构造（其余取默认值）。
    ///
    /// 这里不做校验（与 `OneBotConfig::new` 一致）：空串/非法端点由
    /// [`QqConfig::validate`] 在 `QqClient::start` 里一次性报出来。
    pub fn new(app_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            client_secret: client_secret.into(),
            intents: DEFAULT_INTENTS,
            shard: DEFAULT_SHARD,
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            api_base: DEFAULT_API_BASE.to_owned(),
            gateway_url: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            msg_seq_capacity: DEFAULT_MSG_SEQ_CAPACITY,
            token_refresh_margin: DEFAULT_TOKEN_REFRESH_MARGIN,
            heartbeat_timeout: None,
            extra_root_certificates: Vec::new(),
            backoff: Backoff::reconnect_default(),
            jitter: Arc::new(NoJitter),
            clock: Arc::new(hbot_core::SystemClock),
        }
    }

    /// 从环境变量 [`ENV_APP_ID`] / [`ENV_APP_SECRET`] 构造。
    ///
    /// 只读环境变量；错误信息里**不带**变量的值（`AGENTS.md` §4.4）。
    pub fn from_env() -> Result<Self, AdapterError> {
        let app_id = std::env::var(ENV_APP_ID).map_err(|_| {
            AdapterError::protocol(format!("environment variable {ENV_APP_ID} is not set"))
        })?;
        let client_secret = std::env::var(ENV_APP_SECRET).map_err(|_| {
            AdapterError::protocol(format!("environment variable {ENV_APP_SECRET} is not set"))
        })?;
        Ok(Self::new(app_id.trim(), client_secret.trim()))
    }

    /// AppID（不是 QQ 号）。
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// AppSecret。
    ///
    /// 只为同 crate 的取 token 逻辑暴露（`pub(crate)`）：调用方构造时就知道它，
    /// 不需要再读回来，读回来只会增加泄漏面。
    pub(crate) fn client_secret(&self) -> &str {
        &self.client_secret
    }

    /// 声明的 intents 位（默认 `1 << 25`）。
    pub fn intents(&self) -> u64 {
        self.intents
    }

    /// 分片 `[shard_id, shard_count]`（默认 `[0, 1]`）。
    pub fn shard(&self) -> [u32; 2] {
        self.shard
    }

    /// 取 token 的完整 URL。
    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    /// OpenAPI 基址（不含结尾 `/`）。
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// 网关发现 URL 的显式覆盖值；`None` 表示用 `{api_base}/gateway`。
    pub fn gateway_url(&self) -> String {
        match self.gateway_url.as_deref() {
            Some(url) => url.to_owned(),
            None => format!(
                "{}{}",
                self.api_base.trim_end_matches('/'),
                DEFAULT_GATEWAY_PATH
            ),
        }
    }

    /// 连接超时（TCP / TLS / WS 升级 / 等 Hello 帧）。
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// 单次 HTTPS 请求超时。
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// 事件通道容量（有界，满了让读取暂停 = 背压，不丢消息）。
    pub fn event_capacity(&self) -> usize {
        self.event_capacity
    }

    /// `msg_seq` 表容量（有界 LRU）。
    pub fn msg_seq_capacity(&self) -> usize {
        self.msg_seq_capacity
    }

    /// 提前续期窗口。
    pub fn token_refresh_margin(&self) -> Duration {
        self.token_refresh_margin
    }

    /// 心跳看门狗的上限覆盖值。
    ///
    /// `None`（默认）= 服务器 Hello 给的 `heartbeat_interval` 的 2 倍，与协议要求一致。
    /// 测试要压到毫秒级时用 [`QqConfig::with_heartbeat_timeout`] 显式覆盖。
    pub fn heartbeat_timeout(&self) -> Option<Duration> {
        self.heartbeat_timeout
    }

    /// 追加的信任根（私有 CA / 自签部署），DER 编码。
    pub fn extra_root_certificates(&self) -> &[Vec<u8>] {
        &self.extra_root_certificates
    }

    /// 重连退避（复用 OneBot 那套口径：1/2/4…30s ±20%）。
    pub fn backoff(&self) -> Backoff {
        self.backoff
    }

    /// 抖动源。
    pub fn jitter(&self) -> &dyn Jitter {
        self.jitter.as_ref()
    }

    /// 时间源（归一化的 `received_at` 用它）。
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// 时间源的 `Arc` 句柄（crate 内部要长期持有时用）。
    pub(crate) fn clock_arc(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// 打开额外的 intents 位。
    ///
    /// 注意：多声明没有权限的位会被平台关闭连接（社区报告 close `4914`/`4915`，
    /// **未核实**），所以宁可少开也不要一次全开。
    pub fn with_intents(mut self, intents: u64) -> Self {
        self.intents = intents;
        self
    }

    /// 设置分片。
    pub fn with_shard(mut self, shard: [u32; 2]) -> Self {
        self.shard = shard;
        self
    }

    /// 覆盖取 token 的端点（测试指向本地假服务器）。
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// 覆盖 OpenAPI 基址（测试指向本地假服务器）。
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// 覆盖网关发现 URL（整体覆盖，测试指向本地假服务器）。
    pub fn with_gateway_url(mut self, url: impl Into<String>) -> Self {
        self.gateway_url = Some(url.into());
        self
    }

    /// 设置连接超时。
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// 设置单次请求超时。
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// 设置事件通道容量（0 会被 [`QqConfig::validate`] 拒绝 —— 无界/零容量都是 bug）。
    pub fn with_event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity;
        self
    }

    /// 设置 `msg_seq` 表容量（0 会被 [`QqConfig::validate`] 拒绝）。
    pub fn with_msg_seq_capacity(mut self, capacity: usize) -> Self {
        self.msg_seq_capacity = capacity;
        self
    }

    /// 设置提前续期窗口。
    pub fn with_token_refresh_margin(mut self, margin: Duration) -> Self {
        self.token_refresh_margin = margin;
        self
    }

    /// 覆盖心跳看门狗（`None` = 2 × 服务器心跳间隔）。
    pub fn with_heartbeat_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.heartbeat_timeout = timeout;
        self
    }

    /// 追加信任根（DER 证书）。校验始终开启，这里只增加信任，不减少。
    pub fn with_extra_root_certificates(mut self, certificates: Vec<Vec<u8>>) -> Self {
        self.extra_root_certificates = certificates;
        self
    }

    /// 设置重连退避。
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// 设置抖动源。
    pub fn with_jitter(mut self, jitter: Arc<dyn Jitter>) -> Self {
        self.jitter = jitter;
        self
    }

    /// 设置时间源。
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 校验配置能否启动。
    ///
    /// 只做本地检查（不联网）：凭据非空、端点 scheme 合法、容量非 0。
    /// 错误信息里不含 AppSecret。
    pub fn validate(&self) -> Result<(), AdapterError> {
        if self.app_id.trim().is_empty() {
            return Err(AdapterError::protocol("qq app_id must not be empty"));
        }
        if self.client_secret.trim().is_empty() {
            return Err(AdapterError::protocol("qq client_secret must not be empty"));
        }
        if self.event_capacity == 0 {
            return Err(AdapterError::protocol(
                "qq event channel capacity must be greater than 0",
            ));
        }
        if self.msg_seq_capacity == 0 {
            return Err(AdapterError::protocol(
                "qq msg_seq table capacity must be greater than 0",
            ));
        }
        check_endpoint("token_url", &self.token_url)?;
        check_endpoint("api_base", &self.api_base)?;
        if self.gateway_url.is_some() {
            check_endpoint("gateway_url", &self.gateway_url())?;
        }
        if self.shard[1] == 0 {
            return Err(AdapterError::protocol(
                "qq shard count must be greater than 0",
            ));
        }
        if self.shard[0] >= self.shard[1] {
            return Err(AdapterError::protocol(
                "qq shard id must be smaller than the shard count",
            ));
        }
        Ok(())
    }
}

/// 端点必须是 `http://` 或 `https://`（大小写不敏感），且 `://` 后面还有内容。
///
/// 这里**不做**内网拦截：端点来自运维配置（`QqConfig` 是构造时显式传入的），
/// 而测试必须能把它们指向 `127.0.0.1`。面向外部输入的抓取走 [`crate::http`]，
/// 那里才有 SSRF 拦截。
fn check_endpoint(field: &str, url: &str) -> Result<(), AdapterError> {
    let trimmed = url.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let has_scheme = lowered.starts_with("https://") || lowered.starts_with("http://");
    let has_body = trimmed
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.trim().is_empty());
    if has_scheme && has_body {
        Ok(())
    } else {
        Err(AdapterError::protocol(format!(
            "qq {field} must be an absolute http:// or https:// url"
        )))
    }
}

impl fmt::Debug for QqConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QqConfig")
            .field("app_id", &self.app_id)
            .field("client_secret", &"<redacted>")
            .field("intents", &self.intents)
            .field("shard", &self.shard)
            .field("token_url", &self.token_url)
            .field("api_base", &self.api_base)
            .field("gateway_url", &self.gateway_url)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("event_capacity", &self.event_capacity)
            .field("msg_seq_capacity", &self.msg_seq_capacity)
            .field("token_refresh_margin", &self.token_refresh_margin)
            .field("heartbeat_timeout", &self.heartbeat_timeout)
            .field(
                "extra_root_certificates",
                &format!("{} certificate(s)", self.extra_root_certificates.len()),
            )
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_client_secret() {
        let config = QqConfig::new("102000000", "sk-test-DEADBEEF");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("sk-test-DEADBEEF"),
            "client_secret leaked: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "expected a redaction marker"
        );
        assert!(rendered.contains("102000000"));
    }

    #[test]
    fn defaults_match_the_measured_gateway() {
        let config = QqConfig::new("app", "secret");
        assert_eq!(config.intents(), 1 << 25);
        assert_eq!(config.shard(), [0, 1]);
        assert_eq!(config.token_url(), DEFAULT_TOKEN_URL);
        assert_eq!(config.api_base(), DEFAULT_API_BASE);
        assert_eq!(
            config.gateway_url(),
            "https://api.sgroup.qq.com/gateway".to_owned()
        );
        assert_eq!(config.heartbeat_timeout(), None);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn validate_rejects_bad_inputs_without_leaking_the_secret() {
        let err = QqConfig::new("  ", "sk-test-DEADBEEF")
            .validate()
            .expect_err("empty app id must be rejected");
        assert!(!err.to_string().contains("sk-test-DEADBEEF"));

        let err = QqConfig::new("app", "sk-test-DEADBEEF")
            .with_token_url("ftp://example.invalid/token")
            .validate()
            .expect_err("ftp endpoint must be rejected");
        assert!(!err.to_string().contains("sk-test-DEADBEEF"));

        assert!(
            QqConfig::new("app", "secret")
                .with_event_capacity(0)
                .validate()
                .is_err()
        );
        assert!(
            QqConfig::new("app", "secret")
                .with_shard([2, 2])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn gateway_url_follows_api_base() {
        let config = QqConfig::new("app", "secret").with_api_base("http://127.0.0.1:9/");
        assert_eq!(config.gateway_url(), "http://127.0.0.1:9/gateway");
        let config = config.with_gateway_url("http://127.0.0.1:9/custom/gateway");
        assert_eq!(config.gateway_url(), "http://127.0.0.1:9/custom/gateway");
    }
}
