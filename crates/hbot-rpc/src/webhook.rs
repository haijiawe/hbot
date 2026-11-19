//! webhook 的 HTTP 传输层（`C1.webhook`）：外部系统 POST 进来，宿主把它交给 `onWebhook`。
//!
//! # 为什么手写 HTTP/1.1 而不是引入框架
//!
//! 服务只做一件事：收一条 `POST`、读 `Content-Length` 字节、回一条响应。为此引入
//! `axum` + `hyper` 会往依赖图里塞进一整套异步生态，而要复用的判断（方法、体积、
//! 密钥、路由）一条都不会因此变少。这里的取舍与 `ws.rs` 一致：传输层自己刻，
//! 判据全部落在 `hbot-core`（`bot_webhook`）。
//!
//! # 形状
//!
//! ```text
//! POST /hook/<bot_id>/<path>
//! x-hbot-secret: <secret>
//! Content-Length: <n>
//!
//! <body>
//! ```
//!
//! * `<bot_id>` 是 UUID；不在运行中的 bot 表里 → 404；
//! * `<path>` 必须在该 bot 清单的 `capabilities.webhook[].path` 里，否则 404；
//! * 密钥与那个路由声明的环境变量**常数时间**比较，不匹配 → 401；环境变量没设 → 401
//!   （fail-closed，不退化成「没配就放行」）；
//! * 请求体上限 `MAX_WEBHOOK_BODY_BYTES` → 413；`Transfer-Encoding` / 其它方法 → 405；
//! * 应答直接来自 handler 的返回值（见 `bot_webhook::response_from_handler`）。
//!
//! # 单连接一问一答
//!
//! 一条 TCP 连接只处理一个请求就关（`Connection: close`）。理由：webhook 调用方通常是
//! 「发一条就走」的脚本/服务，keep-alive 在这里省不下可观的握手，却要引入连接生命周期、
//! 空闲超时与流水线顺序三条额外的状态机。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use hbot_core::{BotId, WebhookError, WebhookResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::app::AppState;

/// 读取请求头与体的总超时。
///
/// 外部系统可能连上不发（扫描器、健康检查误配），不设上限就是一条连接占一个任务。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求头字节上限：8 KiB（`Content-Length` 本身加上几个头远不到）。
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

/// 路由前缀：所有 webhook 都在它下面。
pub const ROUTE_PREFIX: &str = "/hook/";

/// 服务配置。
#[derive(Clone, Debug)]
pub struct WebhookConfig {
    /// 绑哪个地址。**默认 127.0.0.1**：webhook 的对外暴露应当由反向代理负责，
    /// 而不是让宿主自己监听 0.0.0.0（那等于把「谁都能打」当成默认）。
    pub bind: SocketAddr,
    /// 单次请求的超时。
    pub request_timeout: Duration,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

impl WebhookConfig {
    /// 绑一个固定端口（测试与自托管用；0 = 让系统挑）。
    #[must_use]
    pub fn on_port(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

/// 一次请求的解析结果（已经过形状校验，还没有过路由/密钥）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRequest {
    /// 方法（大写）。
    pub method: String,
    /// 路径（不含 query）。
    pub path: String,
    /// 头（名字全部小写）。
    pub headers: HashMap<String, String>,
    /// 体。
    pub body: Vec<u8>,
}

impl ParsedRequest {
    /// 某个头（名字大小写不敏感）。
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// 解析失败：状态码 + 一句人话（不回显请求内容）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseRejection {
    /// 要回的 HTTP 状态码。
    pub status: u16,
    /// 一句话原因。
    pub message: String,
}

impl ParseRejection {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// 从一段字节里解析一个请求（`Content-Length` 必须已经把体读全）。
///
/// 只认三种形状：请求行 + 头 + 空行 + 体。不支持 `Transfer-Encoding`（`chunked` 需要
/// 一整套分块状态机，而 webhook 的调用方给 `Content-Length` 是常态）、不支持
/// `Expect: 100-continue`、不支持流水线。
pub fn parse_request(bytes: &[u8]) -> Result<ParsedRequest, ParseRejection> {
    // 头与体以第一个 CRLFCRLF 分界。
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| ParseRejection::new(400, "请求头没有以空行结束"))?;
    let head = std::str::from_utf8(&bytes[..split])
        .map_err(|_| ParseRejection::new(400, "请求头不是合法 UTF-8"))?;
    let body = bytes[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ParseRejection::new(400, "缺少请求行"))?;
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ParseRejection::new(400, "请求行缺少方法"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| ParseRejection::new(400, "请求行缺少路径"))?;
    // 只取路径：query 留给调用方自己（webhook 的路由判据只看路径）。
    let path = target.split('?').next().unwrap_or(target).to_owned();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ParseRejection::new(400, "请求头缺少冒号"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }

    if let Some(value) = headers.get("transfer-encoding") {
        return Err(ParseRejection::new(
            501,
            format!("不支持 Transfer-Encoding（{value}）：请用 Content-Length"),
        ));
    }

    Ok(ParsedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// 把 `/hook/<bot_id>/<path>` 拆成两段。
#[must_use]
pub fn route_of(path: &str) -> Option<(BotId, String)> {
    let rest = path.strip_prefix(ROUTE_PREFIX)?;
    let (bot, tail) = rest.split_once('/')?;
    let bot: BotId = bot.parse().ok()?;
    if tail.is_empty() {
        return None;
    }
    Some((bot, tail.to_owned()))
}

/// 一条要回的响应（已经含状态码与体）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireResponse {
    /// 状态码。
    pub status: u16,
    /// 内容类型。
    pub content_type: String,
    /// 体（UTF-8 文本；错误响应也走它）。
    pub body: String,
}

impl WireResponse {
    /// 一段 JSON 错误体。
    #[must_use]
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json".to_owned(),
            body: serde_json::json!({ "error": message }).to_string(),
        }
    }

    /// 序列化成 HTTP/1.1 响应字节。
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = reason_phrase(self.status);
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(self.body.as_bytes());
        bytes
    }
}

/// 一小撮状态码的原因短语（回给调用方看的，不需要全集）。
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// 把 `WebhookError` 翻成 HTTP 状态码与错误体。
///
/// 状态码的选择是**给调用方看的语义**：401 让他去查密钥、404 让他去查路径、503 让他
/// 稍后重试、500/504 让他把请求报文与宿主日志一起交给维护者。全部自带错误码前缀。
#[must_use]
pub fn response_for_error(error: &WebhookError) -> WireResponse {
    let status = match error {
        WebhookError::NotDeclared => 404,
        WebhookError::Unauthorized { .. } => 401,
        WebhookError::NoRoute { .. } => 404,
        WebhookError::Invalid { .. } => 400,
        WebhookError::Unavailable { .. } => 503,
        WebhookError::Timeout => 504,
        WebhookError::Failed { .. } => 500,
    };
    WireResponse::error(status, &error.to_string())
}

/// 把 handler 的应答翻成 wire 响应。
#[must_use]
pub fn wire_response(response: WebhookResponse) -> WireResponse {
    WireResponse {
        status: response.status,
        content_type: response.content_type,
        body: response.body,
    }
}

/// 正在跑的 webhook 服务。
pub struct WebhookServer {
    /// 实际绑定的地址（接受循环拿走了 listener，这里只留地址）。
    address: SocketAddr,
    /// 停服信号。
    shutdown: tokio::sync::watch::Sender<bool>,
    /// 接受循环的句柄。
    accept: JoinHandle<()>,
}

impl WebhookServer {
    /// 绑地址并开始接受连接。
    ///
    /// # Errors
    ///
    /// 端口占用、权限不足都会在这里失败（而不是让服务"看起来起来了"）。
    pub async fn bind(
        config: WebhookConfig,
        state: Arc<AppState>,
    ) -> anyhow::Result<WebhookServer> {
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("绑定 webhook 端口 {} 失败", config.bind))?;
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        // `addr()` 要能问实际端口（测试用 0 让系统挑），所以接受循环拿走的是克隆。
        let address = listener.local_addr().ok();
        if let Some(addr) = address {
            info!(%addr, "webhook 服务已启动");
        }
        let accepting = listener;
        let accept = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // 停服信号优先：先关接受，再让在飞请求自己跑完（各自有超时）。
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = accepting.accept() => {
                        let Ok((stream, peer)) = accepted else {
                            if shutdown_rx.has_changed().unwrap_or(false) {
                                break;
                            }
                            warn!("webhook accept 失败，继续接受下一连接");
                            continue;
                        };
                        let state = Arc::clone(&state);
                        let timeout = config.request_timeout;
                        tokio::spawn(async move {
                            if let Err(error) = handle_connection(stream, peer, state, timeout).await {
                                debug!(%peer, error = %error, "webhook 连接处理结束");
                            }
                        });
                    }
                }
            }
        });
        Ok(Self {
            address: address.context("读取 webhook 监听地址失败")?,
            shutdown,
            accept,
        })
    }

    /// 实际绑定的地址（测试用它拿系统分配的端口）。
    ///
    /// # Errors
    ///
    /// 只有拿不到本地地址时失败（正常不会发生）。
    pub fn addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.address)
    }

    /// 停服：拒绝新连接，等在飞请求自然结束。
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        // 先让接受循环退出，再 join：否则 accept 会一直等着下一个连接。
        self.accept.abort();
        let _ = self.accept.await;
    }
}

/// 处理一条连接：读一个请求 → 派发 → 回一个响应 → 关。
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: Arc<AppState>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let request = match tokio::time::timeout(timeout, read_request(&mut stream)).await {
        Err(_) => {
            let response = WireResponse::error(408, "HBOT_WEBHOOK_INVALID: 读请求超时");
            let _ = stream.write_all(&response.to_bytes()).await;
            return Ok(());
        }
        Ok(Err(rejection)) => {
            let response = WireResponse::error(rejection.status, &rejection.message);
            let _ = stream.write_all(&response.to_bytes()).await;
            return Ok(());
        }
        Ok(Ok(request)) => request,
    };

    if request.method != "POST" {
        let response = WireResponse::error(405, "HBOT_WEBHOOK_INVALID: 只接受 POST");
        let _ = stream.write_all(&response.to_bytes()).await;
        return Ok(());
    }

    let response = dispatch(&state, &request).await;
    stream
        .write_all(&response.to_bytes())
        .await
        .with_context(|| format!("回写 webhook 响应失败（peer {peer}）"))?;
    Ok(())
}

/// 读一个请求：先读到 `\r\n\r\n`，再按 `Content-Length` 读体。
async fn read_request(stream: &mut TcpStream) -> Result<ParsedRequest, ParseRejection> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| ParseRejection::new(400, format!("读请求失败：{error}")))?;
        if read == 0 {
            return Err(ParseRejection::new(400, "连接在读请求头时关闭"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(ParseRejection::new(413, "请求头过大"));
        }
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };

    // 先解析头部拿到长度，再确认体是否已经读全。
    let head_only = parse_request(&buffer[..head_end])?;
    let declared = match head_only.header("content-length") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| ParseRejection::new(400, "Content-Length 不是合法整数"))?,
        None => 0,
    };
    if declared > hbot_core::MAX_WEBHOOK_BODY_BYTES {
        return Err(ParseRejection::new(
            413,
            format!(
                "请求体 {declared} 字节，超过上限 {}",
                hbot_core::MAX_WEBHOOK_BODY_BYTES
            ),
        ));
    }
    while buffer.len() < head_end + declared {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| ParseRejection::new(400, format!("读请求体失败：{error}")))?;
        if read == 0 {
            return Err(ParseRejection::new(400, "连接在读请求体时关闭"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    parse_request(&buffer[..head_end + declared])
}

/// 路由 + 鉴权 + 派发。
async fn dispatch(state: &Arc<AppState>, request: &ParsedRequest) -> WireResponse {
    let Some((bot_id, route)) = route_of(&request.path) else {
        return WireResponse::error(404, "HBOT_WEBHOOK_NO_ROUTE: 路径不是 /hook/<botId>/<path>");
    };
    let Some(handler) = handler_of(state, &bot_id) else {
        return WireResponse::error(404, "HBOT_WEBHOOK_NO_ROUTE: 这个 bot 没有在运行");
    };
    if let Err(error) = authorize(state, &bot_id, &route, request) {
        warn!(bot = %bot_id, route = %route, error = %error, "webhook 鉴权失败");
        return response_for_error(&error);
    }
    let content_type = request.header("content-type").map(str::to_owned);
    let body = match std::str::from_utf8(&request.body) {
        Ok(text) => text.to_owned(),
        Err(error) => {
            return WireResponse::error(
                400,
                &format!(
                    "HBOT_WEBHOOK_INVALID: 请求体不是合法 UTF-8（第 {} 字节起）",
                    error.valid_up_to()
                ),
            );
        }
    };
    match handler
        .dispatch_webhook(&route, &request.method, content_type.as_deref(), &body)
        .await
    {
        Ok(response) => wire_response(response),
        Err(error) => response_for_error(&error),
    }
}

/// 运行中的 bot → 它的执行器；没在跑就是 `None`。
fn handler_of(
    state: &Arc<AppState>,
    bot_id: &BotId,
) -> Option<Arc<crate::app::runtime::sandbox_handler::SandboxHandler>> {
    state.webhooks().get(bot_id)
}
/// 运行中的 bot → 它的 sandbox 执行器。
///
/// 为什么不是从 `BotRegistry` 拿：登记表存的是 supervisor（消息侧要它），webhook 要的是
/// 执行器本身 —— 它不走 supervisor，因为没有会话、没有队列、没有去重。给 `RunningBot` 加
/// 一个执行器字段会让「登记表存什么」的边界变糊；单独一张小表更清楚，而且 `bot_stop`
/// 时清掉它正好等于「这个 bot 的 webhook 立刻 404」。
#[derive(Default)]
pub struct WebhookRegistry {
    handlers: std::sync::Mutex<
        std::collections::HashMap<BotId, Arc<crate::app::runtime::sandbox_handler::SandboxHandler>>,
    >,
}

impl WebhookRegistry {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个刚启动的 bot。
    pub fn register(
        &self,
        bot_id: BotId,
        handler: Arc<crate::app::runtime::sandbox_handler::SandboxHandler>,
    ) {
        self.lock().insert(bot_id, handler);
    }

    /// 注销（`bot_stop` 调）：之后这个 bot 的 webhook 一律 404。
    pub fn unregister(&self, bot_id: &BotId) {
        self.lock().remove(bot_id);
    }

    /// 取一个在跑的执行器。
    #[must_use]
    pub fn get(
        &self,
        bot_id: &BotId,
    ) -> Option<Arc<crate::app::runtime::sandbox_handler::SandboxHandler>> {
        self.lock().get(bot_id).map(Arc::clone)
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::HashMap<BotId, Arc<crate::app::runtime::sandbox_handler::SandboxHandler>>,
    > {
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for WebhookRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookRegistry")
            .field("running", &self.lock().len())
            .finish_non_exhaustive()
    }
}

/// 鉴权：路由必须声明过，密钥必须与环境变量**常数时间**相等。
///
/// 三条 fail-closed：路由没声明 → 401（不是 404：路径形状对、只是这个项目没开这条），
/// 环境变量没设 → 401（**不**退化成「没配就放行」），密钥不匹配 → 401。
/// 三种都回 401 而不是分别回码：对调用方来说补救动作是同一个（去问部署方要正确的密钥），
/// 而把「这个路由存在但你没配密钥」与「密钥错了」区分开，等于给探测者一个存在性探针。
fn authorize(
    state: &Arc<AppState>,
    bot_id: &BotId,
    route: &str,
    request: &ParsedRequest,
) -> Result<(), WebhookError> {
    let Some(root) = state.project_root() else {
        return Err(WebhookError::NotDeclared);
    };
    let declared = crate::app::runtime::webhook::declared_webhook(&root);
    let Some(spec) = declared.iter().find(|spec| spec.path == route) else {
        return Err(WebhookError::Unauthorized {
            reason: format!("路由 `{route}` 没有在 capabilities.webhook 里声明"),
        });
    };
    let expected = std::env::var(&spec.secret_env).map_err(|_| WebhookError::Unauthorized {
        reason: format!(
            "环境变量 {} 没有设置：这条路由的密钥读不到，一律拒绝（不会因为没配就放行）",
            spec.secret_env
        ),
    })?;
    let given = request
        .header(hbot_core::WEBHOOK_SECRET_HEADER)
        .unwrap_or_default();
    if !hbot_core::secrets_match(&expected, given) {
        return Err(WebhookError::Unauthorized {
            reason: format!(
                "请求头 {} 与 {} 不匹配",
                hbot_core::WEBHOOK_SECRET_HEADER,
                spec.secret_env
            ),
        });
    }
    let _ = bot_id;
    Ok(())
}
