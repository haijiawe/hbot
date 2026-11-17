//! JSON-RPC 2.0 over WebSocket：后台模式（`hbot serve`）的传输层。
//!
//! 桌面模式在同一进程里直接调 [`Dispatcher::dispatch`]；后台模式没有 Tauri，
//! 唯一的入口就是这里。两条路径进的是同一个 `dispatch`，不存在"桌面能做的事
//! 后台做不了"（`docs/02-architecture.md` §3、§9）。
//!
//! # 协议（`docs/02-architecture.md` §9.3）
//!
//! 1. 只绑 `127.0.0.1`；握手时按 `allowed_origins` 校验 `Origin`。白名单为空表示
//!    **只接受无 `Origin` 的本地客户端**（浏览器必带 `Origin`，因此这条规则把
//!    网页拦在外面）；无论哪种情况都要过 token。
//! 2. 握手后第一帧**必须**是
//!    `{"jsonrpc":"2.0","id":1,"method":"auth","params":{"token":"…"}}`，
//!    token 用常数时间比较；[`WsConfig::auth_timeout`] 内未认证即关闭（1008）。
//! 3. 认证后：
//!    - `dispatch`：`params` 是 [`Command`] 的 JSON（或 `{"command": …}` 包装）；
//!    - `app_info`：`Command::AppInfo` 的便捷方法（UI 启动第一条，少一层包装）。
//!
//!    未知方法 → `-32601`；`params` 反序列化失败 → `-32602`（message 里带 serde
//!    线索与 `params` 片段）；在飞请求超过 [`WsConfig::max_in_flight`] → `-32003`
//!    （`too many in flight`）；单帧超过 [`WsConfig::max_message_bytes`] → 关闭 1009。
//!
//! # 响应形状
//!
//! 成功 `{"jsonrpc":"2.0","id":…,"result":…}`，失败
//! `{"jsonrpc":"2.0","id":…,"error":{"code":…,"message":…,"data":…}}`。拆 `result` /
//! `error` 用的是 [`crate::Response::into_result`]，**不另造一套 DTO**：`ApiError`
//! 就是 wire 上的错误对象，`Response` 就是 dispatch 的结果类型。
//!
//! # 事件广播
//!
//! 本模块自己实现 [`EventSink<Event>`]（[`WsServer::event_sink`]），装配方把它交给
//! `Dispatcher::with_events`；事件只发给**已认证**的连接，帧形状
//! `{"jsonrpc":"2.0","method":"event","params":<Event>}`。
//!
//! 每连接两条有界队列（`AGENTS.md` §5 通道一律有界）：
//!
//! - **关键队列**：`approval.request` / `agent.diff` 走它，容量固定 64，状态类事件
//!   的洪水不可能把它挤满；
//! - **普通队列**：其余事件，容量 [`WsConfig::event_queue_capacity`]（默认 1024，
//!   `docs/02-architecture.md` §9.2）。
//!
//! 队列满时：关键事件记 `events_overflowed` 并 `error!`（同步 `emit` 不能阻塞，
//! 只能显式降级并记录）；普通事件记 `events_dropped` 并 `warn!`。绝不静默丢弃
//! （`AGENTS.md` §0.8）。
//!
//! # 使用
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use hbot_rpc::{Dispatcher, ws::{WsConfig, WsServer}};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let server = WsServer::new(WsConfig::new("token"));
//! let dispatcher = Arc::new(Dispatcher::new().with_events(server.event_sink()));
//! let handle = server.bind(dispatcher).await?;
//! println!("listening on {}", handle.addr());
//! handle.shutdown().await;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context as _;
use futures_util::{SinkExt as _, StreamExt as _};
use hbot_core::EventSink;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tracing::{debug, error, warn};

use crate::Dispatcher;
use crate::dto::{ApiError, Command, Event, Response};

/// 默认单帧上限：1 MiB。
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// 默认单连接同时在飞请求数。
pub const DEFAULT_MAX_IN_FLIGHT: usize = 32;

/// 默认认证超时：5 秒。
pub const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// 默认每连接普通事件队列容量（`docs/02-architecture.md` §9.2）。
pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 1024;

/// 关键事件（`approval.request` / `agent.diff`）的独立队列容量。
///
/// 小而有界：关键事件数量天然少，给它一条不被状态洪水挤占的通道即可。
const CRITICAL_QUEUE_CAPACITY: usize = 64;

/// 关闭时等待单个任务收尾的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// JSON-RPC 解析失败。
const PARSE_ERROR: i32 = -32700;
/// JSON-RPC 请求形状非法。
const INVALID_REQUEST: i32 = -32600;
/// 未知方法。
const METHOD_NOT_FOUND: i32 = -32601;
/// `params` 不合法。
const INVALID_PARAMS: i32 = -32602;
/// 在飞请求超过上限；数值与 [`hbot_core::HbotError::Conflict`]（`-32003`）一致。
const TOO_MANY_IN_FLIGHT: i32 = -32003;

/// 后台 WebSocket 服务的配置。
#[derive(Clone, Debug)]
pub struct WsConfig {
    /// 监听地址；`127.0.0.1:0` 表示由内核分配端口（测试用）。
    pub bind: SocketAddr,
    /// 认证 token（非空）。**任何日志都不打印它**。
    pub token: String,
    /// 允许的 `Origin` 白名单；为空表示只接受无 `Origin` 的本地客户端。
    pub allowed_origins: Vec<String>,
    /// 单帧上限（字节），默认 1 MiB。
    pub max_message_bytes: usize,
    /// 单连接同时在飞的请求上限，默认 32。
    pub max_in_flight: usize,
    /// 认证超时，默认 5 秒。
    pub auth_timeout: Duration,
    /// 每连接普通事件队列容量，默认 1024。
    pub event_queue_capacity: usize,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: String::new(),
            allowed_origins: Vec::new(),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            auth_timeout: DEFAULT_AUTH_TIMEOUT,
            event_queue_capacity: DEFAULT_EVENT_QUEUE_CAPACITY,
        }
    }
}

impl WsConfig {
    /// 以 token 建默认配置（`127.0.0.1:0`，其余取默认值）。
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            ..Self::default()
        }
    }

    /// 设置监听地址。
    pub fn with_bind(mut self, bind: SocketAddr) -> Self {
        self.bind = bind;
        self
    }

    /// 追加一个允许的 `Origin`。
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.push(origin.into());
        self
    }

    /// 设置单帧上限。
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    /// 设置单连接在飞请求上限。
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max;
        self
    }

    /// 设置认证超时。
    pub fn with_auth_timeout(mut self, auth_timeout: Duration) -> Self {
        self.auth_timeout = auth_timeout;
        self
    }

    /// 设置每连接普通事件队列容量。
    pub fn with_event_queue_capacity(mut self, capacity: usize) -> Self {
        self.event_queue_capacity = capacity;
        self
    }

    /// 校验配置；不合法时返回带上下文的错误（不 panic）。
    fn validate(&self) -> anyhow::Result<()> {
        if self.token.is_empty() {
            anyhow::bail!("token 不能为空：后台 RPC 必须带认证 token（`--token` 或 `HBOT_TOKEN`）");
        }
        if self.max_message_bytes == 0 {
            anyhow::bail!("max_message_bytes 必须大于 0");
        }
        if self.max_in_flight == 0 {
            anyhow::bail!("max_in_flight 必须大于 0");
        }
        if self.event_queue_capacity == 0 {
            anyhow::bail!("event_queue_capacity 必须大于 0");
        }
        if self.auth_timeout.is_zero() {
            anyhow::bail!("auth_timeout 必须大于 0");
        }
        Ok(())
    }
}

/// 服务端运行期计数快照（[`WsHandle::metrics`]）。
///
/// 只增不减，便于运维侧做速率计算；`active_connections` 是瞬时值。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WsMetrics {
    /// 当前已认证并登记在册的连接数。
    pub active_connections: u64,
    /// 累计接受的 TCP 连接数。
    pub connections: u64,
    /// 累计认证成功的连接数。
    pub authenticated: u64,
    /// 累计认证失败（token 错、首帧不是 auth、超时）的连接数。
    pub auth_rejected: u64,
    /// 累计因 `Origin` 不在白名单被拒的握手数。
    pub handshake_rejected: u64,
    /// 累计收到的请求数（含 parse / invalid 请求）。
    pub requests: u64,
    /// 累计因在飞请求超限被拒的请求数（`-32003`）。
    pub in_flight_rejected: u64,
    /// 累计丢弃的普通事件数（队列满）。
    pub events_dropped: u64,
    /// 累计丢弃的关键事件数（队列满；正常情况下应恒为 0）。
    pub events_overflowed: u64,
    /// 累计因超过单帧上限被关闭的连接数。
    pub oversized_messages: u64,
}

/// 运行期计数（原子）。
#[derive(Debug, Default)]
struct Counters {
    connections: AtomicU64,
    authenticated: AtomicU64,
    auth_rejected: AtomicU64,
    handshake_rejected: AtomicU64,
    requests: AtomicU64,
    in_flight_rejected: AtomicU64,
    events_dropped: AtomicU64,
    events_overflowed: AtomicU64,
    oversized_messages: AtomicU64,
}

impl Counters {
    /// 加一（`Relaxed` 足够：这些计数只要求最终一致，不参与同步）。
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// 一条连接的发件箱。
///
/// 主循环（唯一持有 `WebSocketStream` 的任务）从这两条队列里取帧写出去；
/// 请求任务与事件广播只往队列里放，不直接碰 socket。
struct ConnHandle {
    critical: mpsc::Sender<String>,
    bulk: mpsc::Sender<String>,
}

/// 事件通道分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventClass {
    /// 不可丢：`approval.request` / `agent.diff` / `debug.paused` / `debug.resumed` /
    /// `device.status` / `device.request`。
    Critical,
    /// 可丢弃（记录并计数）：状态类与可合并增量。
    Bulk,
}

impl EventClass {
    /// 按 `docs/02-architecture.md` §9.2 分类。
    ///
    /// `debug.paused` / `debug.resumed` 必须走 critical：暂停事件丢了，UI 就再也
    /// 不知道 bot 正卡在断点上（表现为「点了运行但没反应」），而它本身是低频事件，
    /// 占不住 64 格 critical 队列。
    ///
    /// `device.status` / `device.request` 同理且更严重：设备状态丢了，UI 会显示
    /// "没有会话在跑"，而鼠标/键盘可能正被生成代码控制（`docs/09` §4.1）。这两条
    /// 也是低频（只在开/关/到期/请求时发），不会挤占 critical 队列。
    fn classify(event: &Event) -> Self {
        match event {
            Event::ApprovalRequest { .. }
            | Event::AgentDiff { .. }
            | Event::DebugPaused { .. }
            | Event::DebugResumed { .. }
            | Event::DeviceStatus { .. }
            | Event::DeviceRequest { .. } => Self::Critical,
            _ => Self::Bulk,
        }
    }
}

/// 投递结果。
enum Offer {
    Sent,
    Full,
    Closed,
}

impl ConnHandle {
    /// 尝试投递一帧事件（不阻塞）。
    fn offer(&self, frame: String, class: EventClass) -> Offer {
        let target = match class {
            EventClass::Critical => &self.critical,
            EventClass::Bulk => &self.bulk,
        };

        match target.try_send(frame) {
            Ok(()) => Offer::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => Offer::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => Offer::Closed,
        }
    }
}

/// 服务端共享状态：连接表、任务表、关闭信号、计数。
struct Hub {
    conns: Mutex<HashMap<u64, ConnHandle>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    next_id: AtomicU64,
    shutdown: watch::Sender<bool>,
    counters: Counters,
}

impl Hub {
    fn new() -> Self {
        let (shutdown, _receiver) = watch::channel(false);
        Self {
            conns: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            shutdown,
            counters: Counters::default(),
        }
    }

    /// 登记一条已认证连接；返回连接 ID。
    fn register(&self, handle: ConnHandle) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.conns).insert(id, handle);
        id
    }

    /// 注销连接（发件箱随之关闭，挂住的请求任务会被唤醒并退出）。
    fn unregister(&self, id: u64) {
        let _removed = lock(&self.conns).remove(&id);
    }

    /// 记下一个连接任务，同时清理已结束的句柄，避免任务表无限增长。
    fn push_task(&self, task: JoinHandle<()>) {
        let mut tasks = lock(&self.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// 取走全部连接任务（关闭时逐个等待）。
    fn take_tasks(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *lock(&self.tasks))
    }

    /// 发出关闭信号（幂等）。
    fn signal_shutdown(&self) {
        if self.shutdown.send(true).is_err() {
            // 没有接收者说明所有连接与 accept 循环都已退出：这就是目标状态。
            debug!("关闭信号无人接收：服务已经停止");
        }
    }

    /// 快照计数。
    fn metrics(&self) -> WsMetrics {
        WsMetrics {
            active_connections: lock(&self.conns).len() as u64,
            connections: self.counters.connections.load(Ordering::Relaxed),
            authenticated: self.counters.authenticated.load(Ordering::Relaxed),
            auth_rejected: self.counters.auth_rejected.load(Ordering::Relaxed),
            handshake_rejected: self.counters.handshake_rejected.load(Ordering::Relaxed),
            requests: self.counters.requests.load(Ordering::Relaxed),
            in_flight_rejected: self.counters.in_flight_rejected.load(Ordering::Relaxed),
            events_dropped: self.counters.events_dropped.load(Ordering::Relaxed),
            events_overflowed: self.counters.events_overflowed.load(Ordering::Relaxed),
            oversized_messages: self.counters.oversized_messages.load(Ordering::Relaxed),
        }
    }

    /// 把事件广播给所有**已认证**连接。
    fn broadcast(&self, event: &Event) {
        let envelope = json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": event,
        });
        // 事件的线上 tag（`bot.status` 等）用于日志字段；不重复维护一张枚举表。
        let event_type = envelope
            .get("params")
            .and_then(|params| params.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("event")
            .to_owned();

        let frame = match serde_json::to_string(&envelope) {
            Ok(frame) => frame,
            Err(error) => {
                error!(%error, "序列化事件失败，事件未广播");
                return;
            }
        };

        let class = EventClass::classify(event);
        let conns = lock(&self.conns);
        for handle in conns.values() {
            match handle.offer(frame.clone(), class) {
                Offer::Sent | Offer::Closed => {}
                Offer::Full if class == EventClass::Critical => {
                    Counters::bump(&self.counters.events_overflowed);
                    error!(
                        event = event_type.as_str(),
                        "关键事件队列已满，事件被迫丢弃：客户端长时间不消费"
                    );
                }
                Offer::Full => {
                    Counters::bump(&self.counters.events_dropped);
                    warn!(
                        event = event_type.as_str(),
                        "事件队列已满，状态类事件被丢弃（客户端读得太慢）"
                    );
                }
            }
        }
    }
}

impl EventSink<Event> for Hub {
    fn emit(&self, event: Event) {
        self.broadcast(&event);
    }
}

/// 后台 WebSocket 服务。
///
/// 生命周期：`WsServer::new` → [`WsServer::event_sink`]（交给 `Dispatcher`）→
/// [`WsServer::bind`] 拿到 [`WsHandle`] → `shutdown`。
pub struct WsServer {
    cfg: WsConfig,
    hub: Arc<Hub>,
}

impl WsServer {
    /// 建服务（尚未绑定端口）。
    pub fn new(cfg: WsConfig) -> Self {
        Self {
            cfg,
            hub: Arc::new(Hub::new()),
        }
    }

    /// 配置快照。
    pub fn config(&self) -> &WsConfig {
        &self.cfg
    }

    /// 事件出口：交给 [`Dispatcher::with_events`]。
    pub fn event_sink(&self) -> Arc<dyn EventSink<Event>> {
        self.hub.clone()
    }

    /// 绑定端口并启动 accept 循环。
    ///
    /// 返回的 [`WsHandle::addr`] 是真实地址；`bind` 用端口 0 时由内核分配。
    pub async fn bind(self, dispatcher: Arc<Dispatcher>) -> anyhow::Result<WsHandle> {
        self.cfg.validate()?;

        let listener = TcpListener::bind(self.cfg.bind)
            .await
            .with_context(|| format!("绑定 {} 失败", self.cfg.bind))?;
        let addr = listener
            .local_addr()
            .context("读取监听地址失败（端口 0 应能拿到内核分配的端口）")?;

        let cfg = Arc::new(self.cfg);
        let hub = self.hub.clone();
        let accept_task = tokio::spawn(accept_loop(listener, hub.clone(), cfg, dispatcher));

        Ok(WsHandle {
            addr,
            hub,
            accept_task: Some(accept_task),
        })
    }

    /// [`WsServer::new`] + [`WsServer::bind`] 的简写。
    pub async fn serve(dispatcher: Arc<Dispatcher>, cfg: WsConfig) -> anyhow::Result<WsHandle> {
        Self::new(cfg).bind(dispatcher).await
    }
}

/// [`WsServer::serve`] 的自由函数形式。
pub async fn serve(dispatcher: Arc<Dispatcher>, cfg: WsConfig) -> anyhow::Result<WsHandle> {
    WsServer::serve(dispatcher, cfg).await
}

/// 运行中的服务句柄。
///
/// `Drop` 会尽量收尾（发关闭信号并中止 accept 循环）；要确定所有连接都已关闭，
/// 用 [`WsHandle::shutdown`]。
pub struct WsHandle {
    addr: SocketAddr,
    hub: Arc<Hub>,
    accept_task: Option<JoinHandle<()>>,
}

impl WsHandle {
    /// 真实监听地址（端口 0 时用它拿内核分配的端口）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 事件出口（与 [`WsServer::event_sink`] 同一个）。
    pub fn event_sink(&self) -> Arc<dyn EventSink<Event>> {
        self.hub.clone()
    }

    /// 运行期计数快照。
    pub fn metrics(&self) -> WsMetrics {
        self.hub.metrics()
    }

    /// 优雅关闭：通知所有连接（1001 GoingAway）、等 accept 循环与连接任务收尾。
    pub async fn shutdown(mut self) {
        self.hub.signal_shutdown();

        if let Some(mut task) = self.accept_task.take() {
            await_task(&mut task, "accept").await;
        }

        for mut task in self.hub.take_tasks() {
            await_task(&mut task, "connection").await;
        }
    }
}

impl Drop for WsHandle {
    fn drop(&mut self) {
        self.hub.signal_shutdown();
        if let Some(task) = self.accept_task.take() {
            // 已发关闭信号；这里只是保证 accept 循环不残留。
            task.abort();
        }
    }
}

impl std::fmt::Debug for WsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsHandle")
            .field("addr", &self.addr)
            .field("active_connections", &self.hub.metrics().active_connections)
            .finish()
    }
}

/// 等待任务收尾；超时就中止并记录（不 panic、不无限等）。
async fn await_task(task: &mut JoinHandle<()>, label: &str) {
    if timeout(SHUTDOWN_TIMEOUT, &mut *task).await.is_err() {
        warn!(task = label, "关闭超时，强制中止任务");
        task.abort();
    }
}

/// 接受循环：直到关闭信号为止，每个 TCP 连接一个任务。
async fn accept_loop(
    listener: TcpListener,
    hub: Arc<Hub>,
    cfg: Arc<WsConfig>,
    dispatcher: Arc<Dispatcher>,
) {
    let mut shutdown = hub.shutdown.subscribe();

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                // 发送端被丢弃（Err）或值变成 true：都该退出。
                debug!(changed = changed.is_ok(), "accept 循环收到关闭信号");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        Counters::bump(&hub.counters.connections);
                        let task = tokio::spawn(handle_connection(
                            stream,
                            Some(peer),
                            hub.clone(),
                            cfg.clone(),
                            dispatcher.clone(),
                        ));
                        hub.push_task(task);
                    }
                    Err(error) => {
                        warn!(%error, "accept 失败，短暂退避后重试");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }
    }
}

/// 单连接入口：握手 → 认证 → 主循环。
async fn handle_connection(
    stream: TcpStream,
    peer: Option<SocketAddr>,
    hub: Arc<Hub>,
    cfg: Arc<WsConfig>,
    dispatcher: Arc<Dispatcher>,
) {
    if let Err(error) = run_connection(stream, peer, &hub, &cfg, &dispatcher).await {
        // 连接级错误是"客户端走了/网络断了"这一类：不要当内部错误刷屏。
        debug!(?peer, %error, "连接结束");
    }
}

/// 单连接的完整流程。
async fn run_connection(
    stream: TcpStream,
    peer: Option<SocketAddr>,
    hub: &Arc<Hub>,
    cfg: &Arc<WsConfig>,
    dispatcher: &Arc<Dispatcher>,
) -> anyhow::Result<()> {
    let mut websocket = accept(stream, hub, cfg).await?;
    let mut shutdown = hub.shutdown.subscribe();

    if *shutdown.borrow() {
        close_ws(&mut websocket, CloseCode::Away, "server shutting down").await;
        return Ok(());
    }

    // ── 认证阶段 ────────────────────────────────────────────────────────────
    let auth = tokio::select! {
        biased;
        _ = shutdown.changed() => {
            close_ws(&mut websocket, CloseCode::Away, "server shutting down").await;
            return Ok(());
        }
        result = timeout(cfg.auth_timeout, read_auth(&mut websocket, cfg)) => result,
    };

    let auth = match auth {
        Ok(Ok(auth)) => auth,
        Ok(Err(failure)) => {
            Counters::bump(&hub.counters.auth_rejected);
            match failure {
                AuthFailure::Closed => debug!(?peer, "认证阶段连接已断开"),
                AuthFailure::Close { code, reason } => {
                    warn!(?peer, reason, "认证失败，关闭连接");
                    close_ws(&mut websocket, code, reason).await;
                }
            }
            return Ok(());
        }
        Err(_elapsed) => {
            Counters::bump(&hub.counters.auth_rejected);
            warn!(?peer, timeout_ms = cfg.auth_timeout.as_millis(), "认证超时");
            close_ws(&mut websocket, CloseCode::Policy, "auth timeout").await;
            return Ok(());
        }
    };

    Counters::bump(&hub.counters.authenticated);
    let ack = json!({
        "jsonrpc": "2.0",
        "id": auth.id,
        "result": { "ok": true },
    });
    websocket
        .send(Message::text(ack.to_string()))
        .await
        .context("发送 auth 结果失败")?;
    debug!(?peer, "认证通过");

    // ── 登记连接与发件箱 ────────────────────────────────────────────────────
    let (critical_tx, mut critical_rx) = mpsc::channel::<String>(CRITICAL_QUEUE_CAPACITY);
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<String>(cfg.event_queue_capacity);
    let conn_id = hub.register(ConnHandle {
        critical: critical_tx.clone(),
        bulk: bulk_tx.clone(),
    });
    let _registration = Registration {
        hub: hub.clone(),
        id: conn_id,
    };

    let semaphore = Arc::new(Semaphore::new(cfg.max_in_flight));
    let context = ConnContext {
        hub: hub.clone(),
        cfg: cfg.clone(),
        dispatcher: dispatcher.clone(),
        semaphore,
        critical: critical_tx,
        bulk: bulk_tx,
        peer,
    };

    // ── 主循环 ──────────────────────────────────────────────────────────────
    //
    // `select!` 的每个分支只产出 [`Step`]，不碰 `websocket`：读写复用同一个
    // `WebSocketStream`，若在分支体里写就会和 `websocket.next()` 的借用打起来。
    loop {
        let step = tokio::select! {
            biased;
            outgoing = critical_rx.recv() => Step::Outgoing(outgoing),
            outgoing = bulk_rx.recv() => Step::Outgoing(outgoing),
            _ = shutdown.changed() => Step::Shutdown,
            incoming = websocket.next() => match incoming {
                None => Step::End,
                Some(Err(error)) => {
                    debug!(?peer, %error, "读取失败");
                    Step::End
                }
                Some(Ok(message)) => Step::Incoming(message),
            },
        };

        match step {
            Step::Outgoing(outgoing) => {
                if !write_out(&mut websocket, outgoing).await? {
                    break;
                }
            }
            Step::Incoming(message) => match context.on_message(message).await {
                Incoming::Continue => {}
                Incoming::Close { code, reason } => {
                    close_ws(&mut websocket, code, reason).await;
                    break;
                }
            },
            Step::Shutdown => {
                close_ws(&mut websocket, CloseCode::Away, "server shutting down").await;
                break;
            }
            Step::End => break,
        }
    }

    Ok(())
}

/// 主循环一轮的结果：分支里只决定"做什么"，真正的读写留在 `select!` 外面。
enum Step {
    /// 发件箱里有一条待写出的帧（`None` = 通道关闭）。
    Outgoing(Option<String>),
    /// 读到一个入站消息。
    Incoming(Message),
    /// 服务端开始关闭。
    Shutdown,
    /// 读到流结束 / 读错误。
    End,
}

/// 注销连接的 guard：任何提前返回（`?`、`break`）都会把连接从表里摘掉。
struct Registration {
    hub: Arc<Hub>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.unregister(self.id);
    }
}

/// 握手：校验 `Origin` 并带上帧上限。
///
/// `tungstenite::handshake::server::Callback` 的签名固定是
/// `Result<Response, ErrorResponse>`，`ErrorResponse`（`http::Response`）体积由上游
/// 决定、无法装箱，因此这里按 crate 惯例就地放行 `result_large_err`。
#[allow(clippy::result_large_err)]
async fn accept(
    stream: TcpStream,
    hub: &Arc<Hub>,
    cfg: &Arc<WsConfig>,
) -> anyhow::Result<WsStream> {
    let allowed = cfg.allowed_origins.clone();
    let hub = hub.clone();
    let callback = move |request: &Request,
                         response: HandshakeResponse|
          -> Result<HandshakeResponse, ErrorResponse> {
        match request.headers().get("origin") {
            None => Ok(response),
            Some(value) => match value.to_str() {
                Ok(origin) if allowed.iter().any(|allowed| allowed == origin) => Ok(response),
                Ok(origin) => {
                    Counters::bump(&hub.counters.handshake_rejected);
                    warn!(origin, "拒绝 Origin 不在白名单的握手");
                    Err(forbidden("origin not allowed"))
                }
                Err(_) => {
                    Counters::bump(&hub.counters.handshake_rejected);
                    warn!("拒绝 Origin 不是合法 UTF-8 的握手");
                    Err(forbidden("invalid origin header"))
                }
            },
        }
    };

    // 协议层上限放宽到两倍：超限时先让我们自己的检查给出 1009 与明确日志，
    // 而不是让底层直接按协议错误断开（快照见 `WsConfig::max_message_bytes`）。
    let protocol = WebSocketConfig {
        max_message_size: Some(cfg.max_message_bytes.saturating_mul(2)),
        max_frame_size: Some(cfg.max_message_bytes.saturating_mul(2)),
        ..WebSocketConfig::default()
    };

    accept_hdr_async_with_config(stream, callback, Some(protocol))
        .await
        .context("WebSocket 握手失败")
}

/// 403 响应：`Origin` 被拒。
fn forbidden(reason: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(reason.to_owned()));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// 认证结果。
struct AuthOk {
    id: Value,
}

/// 认证失败。
enum AuthFailure {
    /// 连接已断开（对端关闭 / 读错误），不需要再发关闭帧。
    Closed,
    /// 需要以指定 close code 关闭。
    Close {
        code: CloseCode,
        reason: &'static str,
    },
}

impl AuthFailure {
    /// 认证不通过：一律 1008（policy violation）。
    fn unauthorized(reason: &'static str) -> Self {
        Self::Close {
            code: CloseCode::Policy,
            reason,
        }
    }
}

/// 读第一帧并校验 token。
async fn read_auth(websocket: &mut WsStream, cfg: &WsConfig) -> Result<AuthOk, AuthFailure> {
    loop {
        let message = match websocket.next().await {
            None => return Err(AuthFailure::Closed),
            Some(Err(error)) => {
                debug!(%error, "认证阶段读取失败");
                return Err(AuthFailure::Closed);
            }
            Some(Ok(message)) => message,
        };

        let text = match message {
            Message::Text(text) => text,
            Message::Binary(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return Err(AuthFailure::unauthorized("auth 帧必须是 UTF-8")),
            },
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => return Err(AuthFailure::Closed),
        };

        if text.len() > cfg.max_message_bytes {
            return Err(AuthFailure::Close {
                code: CloseCode::Size,
                reason: "message too big",
            });
        }

        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return Err(AuthFailure::unauthorized("第一帧必须是 auth 请求")),
        };

        if value.get("method").and_then(Value::as_str) != Some("auth") {
            return Err(AuthFailure::unauthorized("第一帧必须是 auth 请求"));
        }

        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let offered = value
            .get("params")
            .and_then(|params| params.get("token"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if !constant_time_eq(offered.as_bytes(), cfg.token.as_bytes()) {
            return Err(AuthFailure::unauthorized("token 不匹配"));
        }

        return Ok(AuthOk { id });
    }
}

/// 常数时间比较：长度不同直接判否，内容逐字节异或累加。
///
/// 不引第三方库（`subtle`）也不自己写 `==`：token 比较是安全边界，必须避免
/// 早退泄露前缀信息。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let length = left.len().max(right.len());

    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }

    std::hint::black_box(diff) == 0
}

/// 单连接的处理上下文（避免把一长串参数传来传去）。
struct ConnContext {
    hub: Arc<Hub>,
    cfg: Arc<WsConfig>,
    dispatcher: Arc<Dispatcher>,
    semaphore: Arc<Semaphore>,
    critical: mpsc::Sender<String>,
    bulk: mpsc::Sender<String>,
    peer: Option<SocketAddr>,
}

impl ConnContext {
    /// 处理一帧入站消息。
    async fn on_message(&self, message: Message) -> Incoming {
        let text = match message {
            Message::Text(text) => text,
            Message::Binary(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.error(Value::Null, INVALID_REQUEST, "帧必须是 UTF-8 文本");
                    return Incoming::Continue;
                }
            },
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Incoming::Continue,
            Message::Close(_) => {
                return Incoming::Close {
                    code: CloseCode::Normal,
                    reason: "client closed",
                };
            }
        };

        if text.len() > self.cfg.max_message_bytes {
            Counters::bump(&self.hub.counters.oversized_messages);
            warn!(
                ?self.peer,
                bytes = text.len(),
                limit = self.cfg.max_message_bytes,
                "单帧超过上限，关闭连接"
            );
            return Incoming::Close {
                code: CloseCode::Size,
                reason: "message too big",
            };
        }

        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                Counters::bump(&self.hub.counters.requests);
                self.error(Value::Null, PARSE_ERROR, &format!("JSON 解析失败：{error}"));
                return Incoming::Continue;
            }
        };

        let Some(object) = value.as_object() else {
            Counters::bump(&self.hub.counters.requests);
            self.error(Value::Null, INVALID_REQUEST, "请求必须是 JSON 对象");
            return Incoming::Continue;
        };

        let id = object.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            Counters::bump(&self.hub.counters.requests);
            self.error(id, INVALID_REQUEST, "缺少 method 字符串");
            return Incoming::Continue;
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let method = method.to_owned();

        match method.as_str() {
            "auth" => {
                Counters::bump(&self.hub.counters.requests);
                self.error(id, INVALID_REQUEST, "已认证：auth 只能是握手后的第一帧");
                Incoming::Continue
            }
            "dispatch" => self.on_dispatch(id, params),
            "app_info" => self.spawn_request(id, Command::AppInfo),
            other => {
                Counters::bump(&self.hub.counters.requests);
                self.error(id, METHOD_NOT_FOUND, &format!("未知方法：{other}"));
                Incoming::Continue
            }
        }
    }

    /// `dispatch`：`params` 是 `Command`，或 `{"command": <Command>}`。
    fn on_dispatch(&self, id: Value, params: Value) -> Incoming {
        let command_value = match &params {
            Value::Object(map) => map
                .get("command")
                .cloned()
                .unwrap_or_else(|| params.clone()),
            _ => params.clone(),
        };

        if command_value.is_null() {
            Counters::bump(&self.hub.counters.requests);
            self.error(id, INVALID_PARAMS, "缺少 params（应为 Command 对象）");
            return Incoming::Continue;
        }

        match serde_json::from_value::<Command>(command_value) {
            Ok(command) => self.spawn_request(id, command),
            Err(error) => {
                Counters::bump(&self.hub.counters.requests);
                let message = format!("params 不是合法的 Command（{error}）：{}", preview(&params));
                self.error(id, INVALID_PARAMS, &message);
                Incoming::Continue
            }
        }
    }

    /// 占一个在飞名额并 spawn 一次 dispatch；名额在响应写进队列后才释放。
    fn spawn_request(&self, id: Value, command: Command) -> Incoming {
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                Counters::bump(&self.hub.counters.in_flight_rejected);
                warn!(
                    ?self.peer,
                    max = self.cfg.max_in_flight,
                    "在飞请求超过上限，拒绝该请求"
                );
                self.error(id, TOO_MANY_IN_FLIGHT, "too many in flight");
                return Incoming::Continue;
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Incoming::Close {
                    code: CloseCode::Error,
                    reason: "in-flight limiter closed",
                };
            }
        };

        Counters::bump(&self.hub.counters.requests);

        let dispatcher = self.dispatcher.clone();
        let sink = self.bulk.clone();
        tokio::spawn(async move {
            let response = dispatcher.dispatch(command).await;
            let frame = encode_response(&id, response);

            if sink.send(frame).await.is_err() {
                debug!("连接已关闭，响应丢弃");
            }

            drop(permit);
        });

        Incoming::Continue
    }

    /// 尽力投递一条错误响应。
    ///
    /// 用 `try_send` 而不是 `await`：本函数由主循环调用，而主循环正是唯一的
    /// 消费者，`await` 会自锁。队列满时记 warn 并放弃（连接随后多半会被关闭）。
    fn error(&self, id: Value, code: i32, message: &str) {
        let frame = error_frame(&id, code, message);

        if self.critical.try_send(frame).is_err() {
            warn!(?self.peer, code, "错误响应未能投递：发件箱已满");
        }
    }
}

/// 主循环对一帧的处理结果。
enum Incoming {
    /// 已处理，继续读下一帧。
    Continue,
    /// 关闭连接。
    Close {
        code: CloseCode,
        reason: &'static str,
    },
}

/// 把一帧写出去；返回 `false` 表示连接应结束（发件箱关闭）。
async fn write_out(websocket: &mut WsStream, outgoing: Option<String>) -> anyhow::Result<bool> {
    let Some(frame) = outgoing else {
        return Ok(false);
    };

    websocket
        .send(Message::text(frame))
        .await
        .context("发送帧失败")?;
    Ok(true)
}

/// 发送关闭帧（尽力而为；对端已断开时不当作错误）。
async fn close_ws(websocket: &mut WsStream, code: CloseCode, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };

    if let Err(error) = websocket.close(Some(frame)).await {
        debug!(%error, "发送关闭帧失败（对端可能已断开）");
    }
}

/// dispatch 结果 → JSON-RPC 响应帧。
///
/// 复用 [`Response::into_result`]：成功走 `result`，失败走 `error`，`ApiError`
/// 原样序列化（`code` / `message` / `data`），不另造错误类型。
fn encode_response(id: &Value, response: Response) -> String {
    match response.into_result() {
        Ok(data) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": data,
        }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": error,
        }),
    }
    .to_string()
}

/// 错误响应帧。
fn error_frame(id: &Value, code: i32, message: &str) -> String {
    let error = ApiError {
        code,
        message: message.to_owned(),
        data: None,
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
    .to_string()
}

/// 截断后的 JSON 片段：错误信息里带字段线索，又不会把 1 MiB 原样回传。
fn preview(value: &Value) -> String {
    const LIMIT: usize = 200;

    let text = value.to_string();
    if text.chars().count() <= LIMIT {
        return text;
    }

    let truncated: String = text.chars().take(LIMIT).collect();
    format!("{truncated}…（已截断）")
}

/// 取锁：被毒化时也继续用内部值（这里没有"半更新"的不变量，恢复比崩掉合适）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 服务端手里的 WebSocket 流类型。
type WsStream = WebSocketStream<TcpStream>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_the_documented_limits() {
        let cfg = WsConfig::default();
        assert_eq!(cfg.max_message_bytes, 1024 * 1024);
        assert_eq!(cfg.max_in_flight, 32);
        assert_eq!(cfg.auth_timeout, Duration::from_secs(5));
        assert_eq!(cfg.event_queue_capacity, 1024);
        assert!(cfg.allowed_origins.is_empty());
        assert_eq!(cfg.bind.port(), 0);
        assert!(cfg.token.is_empty());
    }

    #[test]
    fn validate_rejects_empty_token_and_zero_limits() {
        assert!(WsConfig::default().validate().is_err(), "空 token 必须拒绝");

        let valid = WsConfig::new("t");
        assert!(valid.validate().is_ok(), "默认值必须合法");

        assert!(valid.clone().with_max_in_flight(0).validate().is_err());
        assert!(valid.clone().with_max_message_bytes(0).validate().is_err());
        assert!(
            valid
                .clone()
                .with_event_queue_capacity(0)
                .validate()
                .is_err()
        );
        assert!(valid.with_auth_timeout(Duration::ZERO).validate().is_err());
    }

    #[test]
    fn constant_time_comparison_behaves_like_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"token "));
        assert!(!constant_time_eq(b"token ", b"token"));
        assert!(!constant_time_eq(b"", b"t"));
    }

    #[test]
    fn critical_events_are_classified_as_critical() {
        let approval = Event::ApprovalRequest {
            approval_id: "88888888-8888-4888-8888-888888888888"
                .parse()
                .expect("测试 UUID"),
            tool: "file_write".to_owned(),
            args: json!({ "path": "a.txt" }),
            risk: crate::dto::RiskLevel::High,
        };
        assert_eq!(
            EventClass::classify(&approval),
            EventClass::Critical,
            "approval.request 不可丢"
        );

        let status = Event::BotStatus {
            bot_id: "22222222-2222-4222-8222-222222222222"
                .parse()
                .expect("测试 UUID"),
            status: hbot_core::BotStatus::Running,
            error: None,
        };
        assert_eq!(
            EventClass::classify(&status),
            EventClass::Bulk,
            "bot.status 属于状态类"
        );

        // 调试暂停丢了 UI 就永远停在「点了运行但没反应」，而它本身低频，
        // 所以两条调试事件都归 critical。
        let paused = Event::DebugPaused {
            bot_id: "22222222-2222-4222-8222-222222222222"
                .parse()
                .expect("测试 UUID"),
            file: "src/main.ts".to_owned(),
            line: 12,
            condition: None,
            scopes: Vec::new(),
            call_stack: Vec::new(),
        };
        assert_eq!(
            EventClass::classify(&paused),
            EventClass::Critical,
            "debug.paused 不可丢：丢了 UI 不知道 bot 正卡在断点上"
        );

        let resumed = Event::DebugResumed {
            bot_id: "22222222-2222-4222-8222-222222222222"
                .parse()
                .expect("测试 UUID"),
        };
        assert_eq!(
            EventClass::classify(&resumed),
            EventClass::Critical,
            "debug.resumed 不可丢：丢了 UI 会一直显示暂停中"
        );

        // 设备事件同理：状态条丢了，用户会以为鼠标没被控制。
        let device_status = Event::DeviceStatus {
            status: crate::dto::DeviceStatus {
                bot_id: "22222222-2222-4222-8222-222222222222"
                    .parse()
                    .expect("测试 UUID"),
                session_id: Some("session".to_owned()),
                state: "active".to_owned(),
                sub: vec!["screen".to_owned()],
                allowed_apps: vec!["game.exe".to_owned()],
                granted_at: Some(hbot_core::Timestamp::from_unix_millis(1_700_000_000_000)),
                deadline: Some(hbot_core::Timestamp::from_unix_millis(1_700_000_300_000)),
                remaining_ms: Some(300_000),
            },
        };
        assert_eq!(
            EventClass::classify(&device_status),
            EventClass::Critical,
            "device.status 不可丢：丢了 UI 不显示会话，而鼠标可能正被控制"
        );

        let device_request = Event::DeviceRequest {
            bot_id: "22222222-2222-4222-8222-222222222222"
                .parse()
                .expect("测试 UUID"),
            sub: vec!["screen".to_owned(), "input".to_owned()],
            reason: "想看看你的游戏画面".to_owned(),
            request_id: "req-1".to_owned(),
        };
        assert_eq!(
            EventClass::classify(&device_request),
            EventClass::Critical,
            "device.request 不可丢：丢了用户永远等不到授权弹窗"
        );
    }

    #[test]
    fn preview_truncates_long_params() {
        let short = json!({ "type": "file_read", "path": "a.rs" });
        assert_eq!(preview(&short), short.to_string());

        let long = json!({ "type": "file_write", "content": "x".repeat(500) });
        let text = preview(&long);
        assert!(text.ends_with("（已截断）"), "超长必须截断：{text}");
        assert!(text.chars().count() < 240, "截断后仍然太长：{}", text.len());
    }

    #[test]
    fn encode_response_splits_result_and_error() {
        let ok = encode_response(&json!(7), Response::with_data(json!({ "name": "hbot" })));
        let ok: Value = serde_json::from_str(&ok).expect("必须是 JSON");
        assert_eq!(ok["jsonrpc"], "2.0");
        assert_eq!(ok["id"], 7);
        assert_eq!(ok["result"]["name"], "hbot");
        assert!(ok.get("error").is_none());

        let error = encode_response(
            &json!("abc"),
            Response::error(ApiError {
                code: -32006,
                message: "尚未实现：agent_send".to_owned(),
                data: None,
            }),
        );
        let error: Value = serde_json::from_str(&error).expect("必须是 JSON");
        assert_eq!(error["id"], "abc");
        assert_eq!(error["error"]["code"], -32006);
        assert_eq!(error["error"]["message"], "尚未实现：agent_send");
        assert!(error.get("result").is_none());
    }
}
