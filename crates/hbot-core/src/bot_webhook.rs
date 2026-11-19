//! 外部系统打进来的入口（`C1.webhook`）：契约、参数闸与唯一一份鉴权判据。
//!
//! # 它和 `message` 不是同一条管道
//!
//! `ctx.message` 那一侧的身份来自平台：sender、conversation、message_id 都由适配器
//! 规范化后落 `inbound` 表，去重键是 `UNIQUE (bot_id, external_id)`。webhook 的调用方
//! 是一个**外部系统**（工单、告警、支付回调），它没有平台身份，也没有会话概念 ——
//! 所以它不进 `inbound`：塞进去会让去重键与回复目标都失去定义（`AGENTS.md` §4.1）。
//! 一次请求就是一次事件，事件的结果就是 HTTP 应答。
//!
//! # 应答为什么是 handler 的返回值
//!
//! 调用方在等 HTTP 响应。若做成「注册 + 后续投递」（像 `ctx.session.prompt`），
//! 调用方会被晾在那儿，而且没有任何地方能保证它等得到。所以形状是**请求即事件**：
//! `onWebhook` 跑完，它返回的对象就是应答体（`AGENTS.md` 坑 74 的同一条理由）。
//!
//! # 鉴权不是可选项
//!
//! 服务只绑 `127.0.0.1`，但**本机任意进程都能发 POST**，所以「是本机」不构成鉴权。
//! 每条路由声明一个环境变量名（`secretEnv`），请求必须带 `x-hbot-secret`，值与那个
//! 环境变量**常数时间**比较。密钥不写进清单：清单是项目文件，会入库、会被分享。
//! 环境变量没设时**一律拒绝**（fail-closed），不退化成「没配密钥就放行」。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 一个项目最多声明多少条 webhook 路由。
pub const MAX_WEBHOOK_ROUTES: usize = 8;

/// 路由路径的字节上限（`/hook/<path>` 里的 `<path>`）。
pub const MAX_WEBHOOK_PATH_BYTES: usize = 64;

/// 密钥环境变量名的字节上限。
pub const MAX_WEBHOOK_SECRET_ENV_BYTES: usize = 64;

/// 请求体上限：64 KiB。
///
/// 故意比 `MAX_JOB_PAYLOAD_BYTES`（8 KiB）宽：webhook 常被当「一整个对象推过来」用
/// （工单快照、支付通知），但再大就该走文件而不是 HTTP 头里塞 JSON。
pub const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;

/// 应答体上限：256 KiB。
///
/// 比请求宽：handler 可能把知识库检索结果整段返回。调用方拿到的响应仍受传输层的
/// 写超时约束，所以这个上限只防「一次应答把内存吃光」。
pub const MAX_WEBHOOK_RESPONSE_BYTES: usize = 256 * 1024;

/// 请求头里携带密钥的那个名字（小写，传输层按小写查）。
pub const WEBHOOK_SECRET_HEADER: &str = "x-hbot-secret";

/// 应答的默认内容类型。
pub const DEFAULT_WEBHOOK_CONTENT_TYPE: &str = "application/json";

/// 一条路由的声明。
///
/// `secretEnv` 是**环境变量名**，不是密钥本身（见模块文档「鉴权不是可选项」）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookRoute {
    /// 路径片段，例如 `tickets` → 完整入口 `POST /hook/<bot_id>/tickets`。
    pub path: String,
    /// 从哪个环境变量读这条路由的密钥。
    pub secret_env: String,
}

impl WebhookRoute {
    /// 构造一条路由（不做校验；校验走 [`validate_routes`]）。
    #[must_use]
    pub fn new(path: impl Into<String>, secret_env: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            secret_env: secret_env.into(),
        }
    }
}

/// 一次 webhook 请求，**已经过传输层解析与鉴权**。
///
/// 传输层（`hbot-rpc` 的 `WebhookServer`）负责 HTTP 细节：只收 `POST`、只认
/// `Content-Length`、拒绝 `Transfer-Encoding`、体积上限、密钥比较。到这里时它已经是
/// 「哪条路由 + 一段字节」，所以 isolate 侧看不到 socket、也看不到别的路由的密钥。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookRequest {
    /// 命中的路由路径（已校验过存在于清单）。
    pub route: String,
    /// HTTP 方法（v1 只可能是 `POST`，保留字段是为了将来 `GET` 探活）。
    pub method: String,
    /// 调用方给的 `Content-Type`；没给就是 `None`。
    pub content_type: Option<String>,
    /// 原始请求体。
    pub body: Vec<u8>,
}

impl WebhookRequest {
    /// 一条 `POST` 请求。
    #[must_use]
    pub fn post(route: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            route: route.into(),
            method: "POST".to_owned(),
            content_type: None,
            body: body.into(),
        }
    }

    /// 带上 `Content-Type`。
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// 请求体按 UTF-8 解释；不是合法 UTF-8 时给出错误而不是替换字符。
    ///
    /// 替换字符（U+FFFD）会让「看起来收到了」和「真的收到了」分不清 —— 二进制
    /// 载荷应当走文件通道，而不是被悄悄改写成一段乱码再喂给 handler。
    pub fn text(&self) -> Result<&str, WebhookError> {
        std::str::from_utf8(&self.body).map_err(|error| WebhookError::Invalid {
            message: format!(
                "请求体不是合法 UTF-8（第 {} 字节起）：{error}",
                error.valid_up_to()
            ),
        })
    }
}

/// 一次 webhook 的应答：`onWebhook` 的返回值，原样成为 HTTP 响应。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookResponse {
    /// HTTP 状态码（200–599；调用方给了非法值就按 `HBOT_WEBHOOK_INVALID` 拒绝）。
    pub status: u16,
    /// 内容类型。
    pub content_type: String,
    /// 应答体（UTF-8 文本）。
    pub body: String,
}

impl WebhookResponse {
    /// 一段 JSON 文本。
    #[must_use]
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: DEFAULT_WEBHOOK_CONTENT_TYPE.to_owned(),
            body: body.into(),
        }
    }

    /// 一段纯文本。
    #[must_use]
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/plain; charset=utf-8".to_owned(),
            body: body.into(),
        }
    }

    /// 无应答体（`204`）。
    #[must_use]
    pub fn no_content() -> Self {
        Self {
            status: 204,
            content_type: DEFAULT_WEBHOOK_CONTENT_TYPE.to_owned(),
            body: String::new(),
        }
    }

    /// 改状态码。
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

/// webhook 的失败分类。
///
/// 码的分工与其余能力一致：`NOT_DECLARED` = 清单没写（改 `hbot.json`）、
/// `NO_ROUTE` = 写了但路径对不上（改调用方）、`UNAUTHORIZED` = 密钥不对（查环境变量）、
/// `UNAVAILABLE` = 宿主没装配（bot 没起来）、`TIMEOUT` / `FAILED` = 执行侧。
#[derive(Debug, Error)]
pub enum WebhookError {
    /// 项目没声明 `capabilities.webhook`。
    #[error("HBOT_WEBHOOK_NOT_DECLARED: 项目清单没有声明 capabilities.webhook")]
    NotDeclared,
    /// 密钥缺失或不匹配。
    #[error("HBOT_WEBHOOK_UNAUTHORIZED: {reason}")]
    Unauthorized {
        /// 具体原因（不包含密钥本身，也不回显调用方给的值）。
        reason: String,
    },
    /// 路径不在清单声明的路由里。
    #[error("HBOT_WEBHOOK_NO_ROUTE: 路由 {route} 未在 capabilities.webhook.routes 里声明")]
    NoRoute {
        /// 调用方请求的路径。
        route: String,
    },
    /// 参数闸拒绝（体积、状态码、编码）。
    #[error("HBOT_WEBHOOK_INVALID: {message}")]
    Invalid {
        /// 人读的原因。
        message: String,
    },
    /// 宿主没有可用的 isolate（bot 没启动、或 worker 槽位已清空）。
    #[error("HBOT_WEBHOOK_UNAVAILABLE: {reason}")]
    Unavailable {
        /// 为什么没有。
        reason: String,
    },
    /// handler 超时（预算用尽）。
    #[error("HBOT_WEBHOOK_TIMEOUT: 处理这条请求超过了 handler 预算")]
    Timeout,
    /// handler 抛错，或应答形状不可用。
    #[error("HBOT_WEBHOOK_FAILED: {message}")]
    Failed {
        /// 人读的原因（handler 的异常文本原样带出）。
        message: String,
    },
}

impl WebhookError {
    /// 稳定的错误码前缀（`Display` 也以它开头）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared => "HBOT_WEBHOOK_NOT_DECLARED",
            Self::Unauthorized { .. } => "HBOT_WEBHOOK_UNAUTHORIZED",
            Self::NoRoute { .. } => "HBOT_WEBHOOK_NO_ROUTE",
            Self::Invalid { .. } => "HBOT_WEBHOOK_INVALID",
            Self::Unavailable { .. } => "HBOT_WEBHOOK_UNAVAILABLE",
            Self::Timeout => "HBOT_WEBHOOK_TIMEOUT",
            Self::Failed { .. } => "HBOT_WEBHOOK_FAILED",
        }
    }
}

/// 校验一条路由的路径片段。
///
/// 允许 `[a-z0-9/-_]`，不许空、不许前导或尾随 `/`、不许 `//`、不许 `..`：路径要能
/// 直接拼进 URL 且不能靠它做目录穿越（传输层虽然不碰文件系统，但「路径里出现 `..`」
/// 是调用方在探路，早点拒比晚点拒好）。
pub fn validate_route_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("路由路径不能为空".to_owned());
    }
    if path.len() > MAX_WEBHOOK_PATH_BYTES {
        return Err(format!(
            "路由路径 {} 字节，超过上限 {MAX_WEBHOOK_PATH_BYTES}",
            path.len()
        ));
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(format!("路由路径 `{path}` 不能以 `/` 开头或结尾"));
    }
    if path.contains("//") {
        return Err(format!("路由路径 `{path}` 不能含连续 `/`"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("路由路径 `{path}` 出现了空段、`.` 或 `..`"));
        }
    }
    if let Some(bad) = path
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '/')))
    {
        return Err(format!(
            "路由路径 `{path}` 含非法字符 `{bad}`（只允许 a-z 0-9 - _ /）"
        ));
    }
    Ok(())
}

/// 校验密钥环境变量名。
///
/// 只接受 `[A-Z0-9_]`、不以数字开头：环境变量名写错时（小写、带 `-`、带空格）应当
/// **在加载清单时**就失败，而不是等第一次请求才发现「读不到密钥」→ 全部 401。
pub fn validate_secret_env(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("secretEnv 不能为空".to_owned());
    }
    if name.len() > MAX_WEBHOOK_SECRET_ENV_BYTES {
        return Err(format!(
            "secretEnv {name} 长度 {} 字节，超过上限 {MAX_WEBHOOK_SECRET_ENV_BYTES}",
            name.len()
        ));
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() || first == '_' => {}
        _ => {
            return Err(format!(
                "secretEnv `{name}` 必须以大写字母或 `_` 开头（环境变量名惯例）"
            ));
        }
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_'))
    {
        return Err(format!(
            "secretEnv `{name}` 含非法字符 `{bad}`（只允许 A-Z 0-9 _）"
        ));
    }
    Ok(())
}

/// 校验一整组路由：数量上限、逐条参数、路径不重复。
pub fn validate_routes(routes: &[WebhookRoute]) -> Result<(), String> {
    if routes.len() > MAX_WEBHOOK_ROUTES {
        return Err(format!(
            "声明了 {} 条 webhook 路由，超过上限 {MAX_WEBHOOK_ROUTES}",
            routes.len()
        ));
    }
    for (index, route) in routes.iter().enumerate() {
        validate_route_path(&route.path)
            .map_err(|error| format!("routes[{index}].path 不合法：{error}"))?;
        validate_secret_env(&route.secret_env)
            .map_err(|error| format!("routes[{index}].secretEnv 不合法：{error}"))?;
        if routes[..index].iter().any(|seen| seen.path == route.path) {
            return Err(format!("路由 `{}` 重复声明", route.path));
        }
    }
    Ok(())
}

/// 校验一条请求（传输层在派发前调）。
pub fn validate_request(request: &WebhookRequest) -> Result<(), WebhookError> {
    validate_route_path(&request.route).map_err(|message| WebhookError::Invalid { message })?;
    if request.method != "POST" {
        return Err(WebhookError::Invalid {
            message: format!("方法 `{}` 不受支持（v1 只收 POST）", request.method),
        });
    }
    if request.body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(WebhookError::Invalid {
            message: format!(
                "请求体 {} 字节，超过上限 {MAX_WEBHOOK_BODY_BYTES}",
                request.body.len()
            ),
        });
    }
    Ok(())
}

/// 校验一条应答（handler 返回之后、写回 socket 之前调）。
pub fn validate_response(response: &WebhookResponse) -> Result<(), WebhookError> {
    if !(200..=599).contains(&response.status) {
        return Err(WebhookError::Invalid {
            message: format!("应答状态码 {} 不在 200–599 之内", response.status),
        });
    }
    if response.body.len() > MAX_WEBHOOK_RESPONSE_BYTES {
        return Err(WebhookError::Invalid {
            message: format!(
                "应答体 {} 字节，超过上限 {MAX_WEBHOOK_RESPONSE_BYTES}",
                response.body.len()
            ),
        });
    }
    if response.content_type.trim().is_empty() {
        return Err(WebhookError::Invalid {
            message: "应答的 content-type 不能为空".to_owned(),
        });
    }
    if let Some(bad) = response
        .content_type
        .chars()
        .find(|c| c.is_control() || *c == '\r' || *c == '\n')
    {
        return Err(WebhookError::Invalid {
            message: format!("应答的 content-type 含控制字符 `{bad}`（会撕开响应头）"),
        });
    }
    Ok(())
}

/// 常数时间比较两段密钥。
///
/// 长度不同时也走完整轮比较：只比较到较短那段会让「前缀对不对」从耗时上泄漏出来。
/// 返回值只说明「等不等」，不说明「差在第几位」。
#[must_use]
pub fn secrets_match(expected: &str, given: &str) -> bool {
    let expected = expected.as_bytes();
    let given = given.as_bytes();
    let mut diff = (expected.len() ^ given.len()) as u8;
    let longest = expected.len().max(given.len());
    for index in 0..longest {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = given.get(index).copied().unwrap_or(0);
        diff |= left ^ right;
    }
    diff == 0
}

/// 在声明的路由里找一条（`path` 精确匹配）。
#[must_use]
pub fn route_for<'a>(routes: &'a [WebhookRoute], path: &str) -> Option<&'a WebhookRoute> {
    routes.iter().find(|route| route.path == path)
}
/// 把 `onWebhook` 的返回值翻译成一条应答。
///
/// 规则只有一份，就写在这里：`返回什么等于什么`是契约，不是 rpc 的实现细节。
///
/// - `undefined` / `null` → `204 No Content`（handler 只想确认收到）；
/// - 字符串 → `text/plain; charset=utf-8`、200（人读的应答不必包一层 JSON）；
/// - 数组 / 数字 / 布尔 → 序列化成 JSON、200；
/// - 对象里出现 `status` / `body` / `contentType` 任何一个 → 视为应答信封
///   （`status` 缺省 200，`contentType` 缺省 `application/json`，`body` 是字符串就照发、
///   否则序列化成 JSON）；
/// - 其余对象 → 整个对象当 JSON 应答体、200。
///
/// 信封里的 `status` 必须是 200–599 的整数：`"200"` 这种字符串一律拒绝，不做隐式转换 ——
/// 悄悄转一次，用户就会在 `201abc` 上看到 201 而以为自己写对了。
pub fn response_from_handler(value: &serde_json::Value) -> Result<WebhookResponse, WebhookError> {
    let response = match value {
        serde_json::Value::Null => WebhookResponse::no_content(),
        serde_json::Value::String(text) => WebhookResponse::text(text.clone()),
        serde_json::Value::Object(map)
            if map.contains_key("status")
                || map.contains_key("body")
                || map.contains_key("contentType") =>
        {
            let status = match map.get("status") {
                None => 200,
                Some(raw) => {
                    let code = raw.as_u64().ok_or_else(|| WebhookError::Invalid {
                        message: format!("应答的 status 必须是整数，收到 {raw}"),
                    })?;
                    u16::try_from(code).map_err(|_| WebhookError::Invalid {
                        message: format!("应答的 status {code} 超出 u16 范围"),
                    })?
                }
            };
            let content_type = match map.get("contentType") {
                None => DEFAULT_WEBHOOK_CONTENT_TYPE.to_owned(),
                Some(raw) => raw
                    .as_str()
                    .ok_or_else(|| WebhookError::Invalid {
                        message: format!("应答的 contentType 必须是字符串，收到 {raw}"),
                    })?
                    .to_owned(),
            };
            let body = match map.get("body") {
                None => String::new(),
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
            };
            WebhookResponse {
                status,
                content_type,
                body,
            }
        }
        other => WebhookResponse::json(other.to_string()),
    };
    validate_response(&response)?;
    Ok(response)
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[test]
    fn a_handler_that_returns_nothing_means_no_content() {
        let response = response_from_handler(&serde_json::Value::Null).unwrap();
        assert_eq!(response.status, 204);
        assert!(response.body.is_empty());
    }

    #[test]
    fn a_string_becomes_a_plain_text_answer() {
        let response = response_from_handler(&serde_json::json!("ok")).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "ok");
        assert!(response.content_type.starts_with("text/plain"));
    }

    #[test]
    fn an_unknown_object_becomes_a_json_body() {
        let value = serde_json::json!({"ticket": 42});
        let response = response_from_handler(&value).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, DEFAULT_WEBHOOK_CONTENT_TYPE);
        assert_eq!(response.body, value.to_string());
    }

    #[test]
    fn an_envelope_carries_the_status_and_content_type() {
        let value = serde_json::json!({
            "status": 418,
            "contentType": "text/plain",
            "body": "teapot"
        });
        let response = response_from_handler(&value).unwrap();
        assert_eq!(response.status, 418);
        assert_eq!(response.content_type, "text/plain");
        assert_eq!(response.body, "teapot");
    }

    #[test]
    fn an_envelope_body_can_be_an_object() {
        let value = serde_json::json!({ "body": { "a": 1 } });
        let response = response_from_handler(&value).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"a\":1}");
    }

    #[test]
    fn a_stringified_status_is_refused_instead_of_coerced() {
        let value = serde_json::json!({ "status": "201" });
        let error = response_from_handler(&value).unwrap_err();
        assert!(error.to_string().contains("必须是整数"), "{error}");
    }

    #[test]
    fn a_status_outside_the_range_is_refused_at_translation_time() {
        let value = serde_json::json!({ "status": 99 });
        let error = response_from_handler(&value).unwrap_err();
        assert!(error.to_string().contains("200–599"), "{error}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn route(path: &str, secret_env: &str) -> WebhookRoute {
        WebhookRoute::new(path, secret_env)
    }

    #[test]
    fn a_plain_path_is_accepted() {
        assert!(validate_route_path("tickets").is_ok());
        assert!(validate_route_path("billing/pay").is_ok());
        assert!(validate_route_path("a-1_b").is_ok());
    }

    #[test]
    fn hostile_paths_are_refused() {
        assert!(validate_route_path("").is_err());
        assert!(validate_route_path("/lead").is_err());
        assert!(validate_route_path("trail/").is_err());
        assert!(validate_route_path("a//b").is_err());
        assert!(validate_route_path("../etc").is_err());
        assert!(validate_route_path("a/./b").is_err());
        assert!(validate_route_path("Upper").is_err());
        assert!(validate_route_path("with space").is_err());
    }

    #[test]
    fn a_path_longer_than_the_limit_is_refused() {
        let long = "a".repeat(MAX_WEBHOOK_PATH_BYTES + 1);
        let error = validate_route_path(&long).unwrap_err();
        assert!(error.contains("超过上限"), "{error}");
    }

    #[test]
    fn a_screaming_snake_env_name_is_accepted() {
        assert!(validate_secret_env("HBOT_HOOK_SECRET").is_ok());
        assert!(validate_secret_env("_PRIVATE").is_ok());
        assert!(validate_secret_env("A1").is_ok());
    }

    #[test]
    fn a_bad_env_name_is_refused_before_the_first_request() {
        assert!(validate_secret_env("").is_err());
        assert!(validate_secret_env("hbot_secret").is_err());
        assert!(validate_secret_env("1BAD").is_err());
        assert!(validate_secret_env("BAD-NAME").is_err());
        assert!(validate_secret_env("BAD NAME").is_err());
    }

    #[test]
    fn duplicate_paths_are_refused() {
        let routes = [route("tickets", "A"), route("tickets", "B")];
        let error = validate_routes(&routes).unwrap_err();
        assert!(error.contains("重复声明"), "{error}");
    }

    #[test]
    fn too_many_routes_are_refused() {
        let routes: Vec<WebhookRoute> = (0..=MAX_WEBHOOK_ROUTES)
            .map(|index| route(&format!("p{index}"), "SECRET"))
            .collect();
        let error = validate_routes(&routes).unwrap_err();
        assert!(error.contains("超过上限"), "{error}");
    }

    #[test]
    fn the_route_error_names_the_index() {
        let routes = [route("ok", "SECRET"), route("Bad", "SECRET")];
        let error = validate_routes(&routes).unwrap_err();
        assert!(error.contains("routes[1].path"), "{error}");
    }

    #[test]
    fn only_post_passes_the_request_gate() {
        let mut request = WebhookRequest::post("tickets", b"{}".to_vec());
        assert!(validate_request(&request).is_ok());
        request.method = "GET".to_owned();
        let error = validate_request(&request).unwrap_err();
        assert_eq!(error.code(), "HBOT_WEBHOOK_INVALID");
        assert!(error.to_string().contains("只收 POST"), "{error}");
    }

    #[test]
    fn an_oversized_body_is_refused() {
        let body = vec![b'x'; MAX_WEBHOOK_BODY_BYTES + 1];
        let request = WebhookRequest::post("tickets", body);
        let error = validate_request(&request).unwrap_err();
        assert!(error.to_string().contains("超过上限"), "{error}");
    }

    #[test]
    fn a_plain_response_passes_the_response_gate() {
        assert!(validate_response(&WebhookResponse::json("{\"ok\":true}")).is_ok());
        assert!(validate_response(&WebhookResponse::no_content()).is_ok());
    }

    #[test]
    fn a_status_outside_two_hundred_to_five_ninety_nine_is_refused() {
        let low = WebhookResponse::json("{}").with_status(199);
        assert!(validate_response(&low).is_err());
        let high = WebhookResponse::json("{}").with_status(600);
        assert!(validate_response(&high).is_err());
    }

    #[test]
    fn an_oversized_response_is_refused() {
        let response = WebhookResponse::text("x".repeat(MAX_WEBHOOK_RESPONSE_BYTES + 1));
        let error = validate_response(&response).unwrap_err();
        assert!(error.to_string().contains("超过上限"), "{error}");
    }

    #[test]
    fn a_content_type_that_would_split_the_headers_is_refused() {
        let mut response = WebhookResponse::json("{}");
        response.content_type = "application/json\r\nX-Injected: 1".to_owned();
        let error = validate_response(&response).unwrap_err();
        assert!(error.to_string().contains("控制字符"), "{error}");
        response.content_type = "   ".to_owned();
        assert!(validate_response(&response).is_err());
    }

    #[test]
    fn secrets_are_compared_without_short_circuiting_on_length() {
        assert!(secrets_match("s3cret", "s3cret"));
        assert!(!secrets_match("s3cret", "s3cre"));
        assert!(!secrets_match("s3cret", "s3cret2"));
        assert!(!secrets_match("s3cret", "S3CRET"));
        assert!(secrets_match("", ""));
        assert!(!secrets_match("", "x"));
    }

    #[test]
    fn route_lookup_is_an_exact_match() {
        let routes = [route("tickets", "A"), route("billing/pay", "B")];
        assert_eq!(
            route_for(&routes, "tickets").map(|r| r.secret_env.as_str()),
            Some("A")
        );
        assert!(route_for(&routes, "ticket").is_none());
        assert!(route_for(&routes, "tickets/").is_none());
    }

    #[test]
    fn a_body_that_is_not_utf8_is_refused_instead_of_replaced() {
        let request = WebhookRequest::post("tickets", vec![0xff, 0xfe]);
        let error = request.text().unwrap_err();
        assert_eq!(error.code(), "HBOT_WEBHOOK_INVALID");
        let ok = WebhookRequest::post("tickets", b"{\"a\":1}".to_vec());
        assert_eq!(ok.text().unwrap(), "{\"a\":1}");
    }

    #[test]
    fn every_error_code_is_the_prefix_of_its_display() {
        let errors = [
            WebhookError::NotDeclared,
            WebhookError::Unauthorized {
                reason: "no secret".to_owned(),
            },
            WebhookError::NoRoute {
                route: "x".to_owned(),
            },
            WebhookError::Invalid {
                message: "bad".to_owned(),
            },
            WebhookError::Unavailable {
                reason: "no isolate".to_owned(),
            },
            WebhookError::Timeout,
            WebhookError::Failed {
                message: "boom".to_owned(),
            },
        ];
        for error in errors {
            assert!(
                error.to_string().starts_with(error.code()),
                "{} 不以码开头：{error}",
                error.code()
            );
        }
    }
}
