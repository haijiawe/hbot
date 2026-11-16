//! 手搓的 OneBot 11 **正向 WebSocket 网关**（测试专用，零第三方 OneBot 库）。
//!
//! 为什么要有它：本套件其它用例把平台适配器换成 [`super::FakeAdapter`]，只验证
//! supervisor / 队列 / outbox。`parity_14` 要验证的是**真协议**——真的
//! `hbot_adapters::onebot::OneBotClient` 连上来、真的 `hbot_rpc` 适配器把 WS 帧
//! 归一化成 `PlatformEvent`、真的 supervisor 落库入队、真的 QuickJS handler 回复、
//! 最后真的 `send_group_msg` 打到这儿。所以这里只把「QQ 那一端」换成进程内的假
//! 网关，协议那一层（握手鉴权 / echo 配对 / 消息段渲染 / 心跳）全部是真的。
//!
//! 协议覆盖面（照 OneBot 11 正向 WS 的定义，不是自定义格式）：
//!
//! * 握手：路径任意；配置了 token 时要求 `Authorization: Bearer <token>`，
//!   不匹配就**拒绝握手**并回 HTTP 401（不是接受连接后再关）。
//! * 连上后推 `meta_event/lifecycle`（`sub_type=connect`）；之后按固定节拍推
//!   `meta_event/heartbeat`。adapter 侧只有 `heartbeat_timeout`（默认 45s）没有
//!   `heartbeat_interval` 配置，所以节拍由测试给定（默认 1s），adapter 用
//!   「任意入站帧刷新看门狗」的方式判定连接还活着。
//! * action 请求：`send_group_msg` / `send_private_msg` / `get_status` 回
//!   `{"status":"ok","retcode":0,"data":{…},"echo":<原样回显>}`；未知 action 回
//!   `{"status":"failed","retcode":1404,"echo":<原样回显>}`。
//! * 出站 action 全部记进共享 `Vec`（[`Gateway::actions`]），供测试断言 payload。
//!
//! 四个可配置开关（对应验收要求里的 ①②③④）：
//!
//! * ① 同一条消息投两次：由测试调 [`Gateway::push`] 两次（网关不替测试去重）。
//! * ② 事件不带 `message_id`：[`group_message`] / [`private_message`] 的
//!   `message_id` 传 `None` 就不写这个字段。
//! * ③ 推完一条就断开：[`Gateway::push_then_disconnect`]，adapter 会自己重连
//!   （[`Gateway::generation`] 涨一次）。
//! * ④ 出站记录：[`Gateway::actions`]，`params` 原样保留。
//!
//! ## 连接可能有多条，但一次 `bot_start` 只开一条
//!
//! 这里刻意维护「所有活着的连接」而不是一个 `active` 槽位：适配器启动时的那条连接
//! 是 `OneBotAdapter::subscribe()` 里 probe → handoff 交接给常驻客户端的（同一条连接，
//! 探测成功后**不关闭**），断线重连还会再开新的；测试自己也可能先用
//! `probe_connection` 手探一次。也就是说同一时刻可能只有一条，但生命周期里会出现多条。
//!
//! 曾经用单槽位时，先关闭的那条连接会把还活着的那条的登记清掉 —— 于是
//! 「真连接明明活着，`live()` 却是 false」，用例卡在推送事件那一步（现象是
//! `gen=3 active=None`）。现在每条连接只登记/注销**自己**那一代，互不影响；
//! 推送目标取「最新一条、且全部连接都已稳定 [`CONNECTION_SETTLE`] 以上」的那条，
//! 这样不会被还在收尾的旧连接吃掉事件。
//!
//! 所有跨任务状态都在 `Mutex` 里、锁只包住一瞬间的读写，不跨 `.await` 持有。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};

/// 服务端方向的发送半边（`ws.split()` 之后由写任务独占）。
type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;

/// 默认心跳节拍：adapter 没有 `heartbeat_interval` 配置项，只能由网关决定。
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(1);

/// 未知 action 的 retcode（OneBot 11 里 1404 = 不支持的 action）。
pub const RETCODE_UNSUPPORTED_ACTION: i64 = 1404;

/// 判定「这条连接已经稳定」所需的存活时长。
///
/// 一条连接可能是刚交接完的探测连接，也可能是测试先手探的那条（`probe_connection`,
/// 握手完就发 `Close`，活不过几十毫秒）。用「活满这段时间」把"还在收尾的旧连接"和
/// "真正要用的连接"区分开，才不会把事件推给一条随即关掉的连接（推给它的帧会被静默
/// 丢掉，测试就变成随机失败）。
///
/// 500ms 是刻意留宽的余量：慢机器上关闭握手也远快于这个量级，而用例的总预算
/// （`support::DEADLINE` = 25s）完全吃得下这点等待。
pub const CONNECTION_SETTLE: Duration = Duration::from_millis(500);

// `support` 里的同名小工具是私有的，这里再来一份：网关自带状态，不依赖上层脚手架。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

static ACTION_MESSAGE_ID: AtomicI64 = AtomicI64::new(20_000);

/// 网关回给 `send_*_msg` 的 `data.message_id`。只求唯一，不参与断言。
fn next_action_message_id() -> i64 {
    ACTION_MESSAGE_ID.fetch_add(1, Ordering::SeqCst)
}

/// 网关配置。
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// 需要的 `Authorization: Bearer <token>`；`None` 表示不校验。
    pub token: Option<String>,
    /// 事件里的 `self_id`（adapter 会按它过滤事件）。
    pub self_id: i64,
    /// 心跳节拍。
    pub heartbeat: Duration,
}

impl GatewayConfig {
    /// 只指定 `self_id`，其余取默认（不校验 token、1s 心跳）。
    pub fn new(self_id: i64) -> Self {
        Self {
            token: None,
            self_id,
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }

    /// 要求 `Authorization: Bearer <token>`，错了就 401 拒绝握手。
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// 改心跳节拍。
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }
}

/// 收到的一条 action 请求（原样留档，测试据此断言 payload）。
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedAction {
    /// 来自第几条连接（第 1 条 = 1）。
    pub connection: u32,
    /// `action` 字段。
    pub action: String,
    /// `params` 字段（缺失时是 `Value::Null`）。
    pub params: Value,
    /// `echo` 字段（缺失时是 `Value::Null`）。
    pub echo: Value,
}

/// 写任务要发出去的一帧。
enum Outgoing {
    /// 一段 JSON 文本帧（事件或 action 响应）。
    Text(String),
    /// 回 ping。
    Pong(Vec<u8>),
    /// 主动断开。
    Close,
}

/// 一条活着的连接。
struct LiveConnection {
    /// 命令通道：写进去的帧由这条连接的写任务发出。
    command: mpsc::Sender<Outgoing>,
    /// 握手完成的时刻，用来判断它是否已经稳定（见 [`CONNECTION_SETTLE`]）。
    since: Instant,
}

struct GatewayInner {
    config: GatewayConfig,
    /// 每接受一条连接 +1；测试据此判断 adapter 重连过。
    generation: AtomicU32,
    /// 活着的连接：generation → 通道。每条连接只登记/注销自己那一代，
    /// 所以「探测连接关了」不会把真连接一起注销掉。
    live: Mutex<BTreeMap<u32, LiveConnection>>,
    actions: Mutex<Vec<RecordedAction>>,
    /// 网关推出去的每个事件（排查用）。
    delivered: Mutex<Vec<Value>>,
    /// 被 401 拒掉的 `Authorization` 头（原样，含 `None` = 压根没带）。
    rejected: Mutex<Vec<Option<String>>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

/// 一个进程内的 OneBot 11 正向 WebSocket 网关。
///
/// `Drop` 会关掉监听并 abort 所有连接任务，所以用例结束不用手工收尾。
pub struct Gateway {
    addr: SocketAddr,
    inner: Arc<GatewayInner>,
    accept_task: JoinHandle<()>,
}

impl Gateway {
    /// 绑 `127.0.0.1:0`（端口由内核给），返回已经能接受连接的网关。
    pub async fn start(config: GatewayConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("绑定 OneBot 网关端口");
        let addr = listener.local_addr().expect("读取 OneBot 网关端口");
        let inner = Arc::new(GatewayInner {
            config,
            generation: AtomicU32::new(0),
            live: Mutex::new(BTreeMap::new()),
            actions: Mutex::new(Vec::new()),
            delivered: Mutex::new(Vec::new()),
            rejected: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
        });
        let accept_inner = Arc::clone(&inner);
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let connection_inner = Arc::clone(&accept_inner);
                let handle = tokio::spawn(serve(stream, connection_inner));
                lock(&accept_inner.connections).push(handle);
            }
        });
        Self {
            addr,
            inner,
            accept_task,
        }
    }

    /// 监听地址（`127.0.0.1:<port>`）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// adapter 用的 WS URL（路径随便是 OneBot 11 正向 WS 的常见写法）。
    pub fn ws_url(&self) -> String {
        format!("ws://{}/onebot/v11/ws", self.addr)
    }

    /// 已经接受过几条连接（adapter 每重连一次就 +1）。
    pub fn generation(&self) -> u32 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// 当前活着的最新一条连接的 generation；没有连接时是 `None`。
    ///
    /// 注意这里**包含**还在收尾的探测连接 —— 想知道「能不能往上推事件」，用
    /// [`Gateway::wait_ready`]（它会等到出现一条稳定的连接）。
    pub fn active_generation(&self) -> Option<u32> {
        lock(&self.inner.live).keys().next_back().copied()
    }

    /// 有没有活着的连接（同样包含探测连接）。
    pub fn live(&self) -> bool {
        !lock(&self.inner.live).is_empty()
    }

    /// 等到出现一条**稳定**的连接，返回它的 generation。
    ///
    /// 「稳定」= 所有活着的连接都已经活过 [`CONNECTION_SETTLE`]，于是返回最新那条。
    /// 适配器启动时那次一次性探测连接会在这段时间内关掉，所以返回值就是真客户端
    /// 那一条；`push` / `push_then_disconnect` 也按同一判据选目标。
    pub async fn wait_ready(&self, timeout: Duration) -> Result<u32, String> {
        self.wait_for(timeout, 0).await
    }

    /// 等到出现一条 generation **大于 `previous`** 的稳定连接，返回它的 generation。
    ///
    /// 「断开 → 重连」必须用这个：刚把 `Close` 塞进队列时旧连接还在注册表里（写任务
    /// 可能还没被调度），只等「有稳定连接」会立刻拿回旧的那条。
    pub async fn wait_ready_after(&self, previous: u32, timeout: Duration) -> Result<u32, String> {
        self.wait_for(timeout, previous).await
    }

    async fn wait_for(&self, timeout: Duration, previous: u32) -> Result<u32, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(generation) = self.stable_generation() {
                if generation > previous {
                    return Ok(generation);
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "OneBot 网关在 {timeout:?} 内没有出现 generation > {previous} 的稳定连接（共接受过 {} 条，当前活着 {} 条）",
                    self.generation(),
                    lock(&self.inner.live).len(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 最新一条稳定连接；有任何连接还没活满 [`CONNECTION_SETTLE`] 时返回 `None`。
    fn stable_generation(&self) -> Option<u32> {
        let live = lock(&self.inner.live);
        if live
            .values()
            .any(|connection| connection.since.elapsed() < CONNECTION_SETTLE)
        {
            return None;
        }
        live.keys().next_back().copied()
    }

    /// 推一个事件。
    pub async fn push(&self, event: Value) -> Result<(), String> {
        self.send(Outgoing::Text(event.to_string())).await?;
        lock(&self.inner.delivered).push(event);
        Ok(())
    }

    /// 推一个事件，然后**立刻主动断开**（考验 adapter 的重连）。
    ///
    /// 两帧进同一个有序队列，所以一定是「先事件、后 close」。
    pub async fn push_then_disconnect(&self, event: Value) -> Result<(), String> {
        self.send(Outgoing::Text(event.to_string())).await?;
        lock(&self.inner.delivered).push(event);
        self.send(Outgoing::Close).await
    }

    async fn send(&self, outgoing: Outgoing) -> Result<(), String> {
        let target = self.stable_generation().and_then(|generation| {
            lock(&self.inner.live)
                .get(&generation)
                .map(|connection| (generation, connection.command.clone()))
        });
        let Some((generation, command)) = target else {
            return Err(format!(
                "OneBot 网关当前没有活着的连接（稳定满 {CONNECTION_SETTLE:?} 才算）",
            ));
        };

        command
            .send(outgoing)
            .await
            .map_err(|_| format!("OneBot 网关第 {generation} 条连接已关闭"))
    }

    /// 收到过的全部 action（按到达顺序）。
    pub fn actions(&self) -> Vec<RecordedAction> {
        lock(&self.inner.actions).clone()
    }

    /// 按名字过滤 action。
    pub fn actions_named(&self, action: &str) -> Vec<RecordedAction> {
        self.actions()
            .into_iter()
            .filter(|recorded| recorded.action == action)
            .collect()
    }

    /// 推出去过的事件（排查用）。
    pub fn delivered(&self) -> Vec<Value> {
        lock(&self.inner.delivered).clone()
    }

    /// 被拒绝的握手请求里的 `Authorization` 头（原样）。
    pub fn rejected_authorizations(&self) -> Vec<Option<String>> {
        lock(&self.inner.rejected).clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.accept_task.abort();
        for handle in lock(&self.inner.connections).drain(..) {
            handle.abort();
        }
    }
}

/// 一条连接的生命周期：握手（可能 401 拒绝）→ 注册 → 读循环 + 写循环。
// `clippy::result_large_err`：tungstenite 的 `handshake::server::Callback` 把
// `ErrorResponse = http::Response<Option<String>>`（136 字节）**按值**写进了签名，
// 这里没法 Box（Box 了就不再是 `Callback`），是外部 trait 逼出来的，不是本模块的选择。
#[allow(clippy::result_large_err)]
async fn serve(stream: TcpStream, inner: Arc<GatewayInner>) {
    let expected = inner.config.token.clone();
    let reject_inner = Arc::clone(&inner);
    let callback =
        move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let presented = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let accepted = match expected.as_deref() {
                None => true,
                Some(token) => presented.as_deref() == Some(format!("Bearer {token}").as_str()),
            };
            if accepted {
                return Ok(response);
            }
            lock(&reject_inner.rejected).push(presented);
            // tungstenite 会先把这帧响应写出去、再返回错误，所以客户端拿到的是真 401。
            let mut reject: ErrorResponse = HttpResponse::new(Some("unauthorized".to_owned()));
            *reject.status_mut() = StatusCode::UNAUTHORIZED;
            Err(reject)
        };

    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };

    let generation = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (sink, mut source) = ws.split();
    let (command, commands) = mpsc::channel::<Outgoing>(64);
    // 登记**自己这一代**（每条连接一条记录，互不覆盖）。
    lock(&inner.live).insert(
        generation,
        LiveConnection {
            command: command.clone(),
            since: Instant::now(),
        },
    );
    let writer = tokio::spawn(write_loop(sink, commands, Arc::clone(&inner)));

    // OneBot 11 正向 WS：连上先推 lifecycle(connect)。
    let _ = command
        .send(Outgoing::Text(lifecycle_event(inner.config.self_id)))
        .await;

    while let Some(frame) = source.next().await {
        match frame {
            Ok(Message::Text(text)) => handle_action(&text, &inner, generation, &command).await,
            Ok(Message::Binary(bytes)) => {
                if let Ok(text) = String::from_utf8(bytes) {
                    handle_action(&text, &inner, generation, &command).await;
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = command.send(Outgoing::Pong(payload)).await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // 只注销自己这一代：探测连接关掉时不能把真连接一起抹掉。
    lock(&inner.live).remove(&generation);
    writer.abort();
}

/// 写半边：命令优先，空闲时按节拍推心跳。
async fn write_loop(
    mut sink: Sink,
    mut commands: mpsc::Receiver<Outgoing>,
    inner: Arc<GatewayInner>,
) {
    let mut heartbeat = tokio::time::interval(inner.config.heartbeat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` 的第一拍是立即完成的；先吃掉它，让「lifecycle 之后隔一个节拍
    // 才有第一帧心跳」，顺序更接近真网关。
    heartbeat.tick().await;
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => {
                match command {
                    Some(Outgoing::Text(text)) => {
                        if sink.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Pong(payload)) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Close) => {
                        let _ = sink.close().await;
                        break;
                    }
                    None => {
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                let interval_ms = u64::try_from(inner.config.heartbeat.as_millis()).unwrap_or(1000);
                let payload = json!({
                    "post_type": "meta_event",
                    "meta_event_type": "heartbeat",
                    "time": now_seconds(),
                    "self_id": inner.config.self_id,
                    "status": { "online": true, "good": true },
                    "interval": interval_ms,
                });
                if sink.send(Message::Text(payload.to_string())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// 处理一条 action 请求：记账 + 回 OneBot 11 形状的响应。
async fn handle_action(
    text: &str,
    inner: &Arc<GatewayInner>,
    generation: u32,
    command: &mpsc::Sender<Outgoing>,
) {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(action) = frame
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    let echo = frame.get("echo").cloned().unwrap_or(Value::Null);
    lock(&inner.actions).push(RecordedAction {
        connection: generation,
        action: action.clone(),
        params,
        echo: echo.clone(),
    });
    let response = match action.as_str() {
        "send_group_msg" | "send_private_msg" => json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": next_action_message_id() },
            "echo": echo,
        }),
        "get_status" => json!({
            "status": "ok",
            "retcode": 0,
            "data": { "online": true, "good": true },
            "echo": echo,
        }),
        _ => json!({
            "status": "failed",
            "retcode": RETCODE_UNSUPPORTED_ACTION,
            "echo": echo,
        }),
    };
    let _ = command.send(Outgoing::Text(response.to_string())).await;
}

/// 连上后第一帧：`meta_event/lifecycle`，`sub_type=connect`。
fn lifecycle_event(self_id: i64) -> String {
    json!({
        "post_type": "meta_event",
        "meta_event_type": "lifecycle",
        "sub_type": "connect",
        "time": now_seconds(),
        "self_id": self_id,
    })
    .to_string()
}

/// 一条群消息事件（字段照 OneBot 11；`message_id` 为 `None` 时**不写**这个字段）。
pub fn group_message(
    self_id: i64,
    message_id: Option<i64>,
    group_id: i64,
    user_id: i64,
    message: Value,
    raw_message: &str,
) -> Value {
    let mut event = json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": now_seconds(),
        "self_id": self_id,
        "user_id": user_id,
        "group_id": group_id,
        "raw_message": raw_message,
        "message": message,
        "sender": { "user_id": user_id, "nickname": "群友", "card": "" },
    });
    if let Some(message_id) = message_id {
        event["message_id"] = json!(message_id);
    }
    event
}

/// 一条私聊消息事件（`sub_type=friend`）。
pub fn private_message(
    self_id: i64,
    message_id: Option<i64>,
    user_id: i64,
    message: Value,
    raw_message: &str,
) -> Value {
    let mut event = json!({
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "time": now_seconds(),
        "self_id": self_id,
        "user_id": user_id,
        "raw_message": raw_message,
        "message": message,
        "sender": { "user_id": user_id, "nickname": "私聊的人", "card": "" },
    });
    if let Some(message_id) = message_id {
        event["message_id"] = json!(message_id);
    }
    event
}

/// `{"type":"text","data":{"text":…}}`。
pub fn text_segment(text: &str) -> Value {
    json!({ "type": "text", "data": { "text": text } })
}

/// `{"type":"at","data":{"qq":…}}`。
pub fn at_segment(qq: i64) -> Value {
    json!({ "type": "at", "data": { "qq": qq.to_string() } })
}

/// `{"type":"image","data":{"file":…}}`。
pub fn image_segment(file: &str) -> Value {
    json!({ "type": "image", "data": { "file": file } })
}
