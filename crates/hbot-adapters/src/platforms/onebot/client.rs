//! OneBot 11 **正向 WebSocket 客户端**：连接、事件接收、动作发送、断线重连。
//!
//! 拓扑（`docs/02-architecture.md` §4）：
//!
//! ```text
//! OneBot 实现（NapCat 等）
//!   │  ws://host:port  （本端主动连出，正向 WS）
//!   ▼
//! supervisor ── 指数退避 1/2/4…30s ±20% 抖动 ──► session
//!                                                   │
//!                    events: bounded mpsc ─────────┤  事件 → NormalizedInbound
//!                    actions: bounded mpsc ────────┘  动作 → JSON + echo 匹配
//! ```
//!
//! 几条刻意的约束：
//!
//! 1. **不丢消息**：`events` 通道有界，满了就让 `session` 停止读取套接字
//!    （背压），而不是丢弃或无限增长（`AGENTS.md` §5 "通道一律有界"）。
//! 2. **先建队列再连接**：`start()` 先创建通道与 supervisor，再由 supervisor
//!    去连；这样重连窗口内产生的事件不会因为"订阅还没建"而丢
//!    （`AGENTS.md` §7.1 的旧坑）。
//! 3. **分类失败不杀连接**：单帧 JSON 坏掉只记日志并跳过（OneBot 实现
//!    偶发发非标准帧），只有传输层/心跳超时才重连。
//! 4. **wss 不支持**：本 crate 没有 TLS 后端，`wss://` 明确报
//!    [`AdapterError::Unsupported`]，不静默降级成明文。
//!
//! 动作（`send_group_msg` 等）用 `echo` 与响应配对；`ActionHandle` 可以在
//! 多个任务里 `clone`（内部同一个有界队列 + 单调 echo 计数器）。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use hbot_core::Clock;

use super::backoff::{Backoff, Jitter, NoJitter};
use super::event::OneBotEvent;
use super::message::outbound_json;
use super::normalize::{NormalizedInbound, normalize};
use super::send::{ActionSender, BoxFuture};
use crate::error::AdapterError;

/// 默认心跳超时：OneBot 实现一般 30s 一次 heartbeat，留 1.5 倍余量。
pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);

/// 默认事件通道容量（与 `docs/02-architecture.md` §4 的会话队列同量级）。
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// 默认动作通道容量。
pub const DEFAULT_ACTION_CAPACITY: usize = 64;

/// 单帧文本上限（防对端发超大 JSON 撑爆内存）。
const MAX_FRAME_TEXT_BYTES: usize = 8 * 1024 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 连接配置。
#[derive(Clone)]
pub struct OneBotConfig {
    /// 正向 WebSocket 地址，必须 `ws://`（`wss://` 需要 TLS，当前不支持）。
    pub ws_url: String,
    /// access token；非空时以 `Authorization: Bearer <token>` 发送。
    pub access_token: Option<String>,
    /// 心跳超时；`None` 表示不启用（例如对接不发 heartbeat 的实现）。
    pub heartbeat_timeout: Option<Duration>,
    /// 事件通道容量（有界，必须 > 0）。
    pub event_capacity: usize,
    /// 动作通道容量（有界，必须 > 0）。
    pub action_capacity: usize,
    /// 重连退避参数。
    pub backoff: Backoff,
    /// 时间源（注入以便测试）。
    pub clock: Arc<dyn Clock>,
    /// 退避抖动源（注入以便测试确定性）。
    pub jitter: Arc<dyn Jitter>,
}

impl OneBotConfig {
    /// 构造最小配置，其余取默认值。
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            access_token: None,
            heartbeat_timeout: Some(DEFAULT_HEARTBEAT_TIMEOUT),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            action_capacity: DEFAULT_ACTION_CAPACITY,
            backoff: Backoff::reconnect_default(),
            clock: Arc::new(crate::clock::SystemClock),
            jitter: Arc::new(NoJitter),
        }
    }

    /// 设置 access token。
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// 设置心跳超时；`None` 关闭心跳检测。
    pub fn with_heartbeat_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.heartbeat_timeout = timeout;
        self
    }

    /// 设置事件/动作通道容量（两个都必须 > 0，`start` 会校验）。
    pub fn with_capacities(mut self, events: usize, actions: usize) -> Self {
        self.event_capacity = events;
        self.action_capacity = actions;
        self
    }

    /// 设置重连退避参数。
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// 注入时钟。
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 注入抖动源。
    pub fn with_jitter(mut self, jitter: Arc<dyn Jitter>) -> Self {
        self.jitter = jitter;
        self
    }
}

impl fmt::Debug for OneBotConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneBotConfig")
            .field("ws_url", &self.ws_url)
            // token 不落日志（`AGENTS.md` §4.4 "日志、错误、审计统一脱敏"）。
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("heartbeat_timeout", &self.heartbeat_timeout)
            .field("event_capacity", &self.event_capacity)
            .field("action_capacity", &self.action_capacity)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

/// 校验 WebSocket 地址：只接受 `ws://`；`wss://` 明确报"缺 TLS 后端"。
pub fn validate_ws_url(url: &str) -> Result<(), AdapterError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::protocol("ws url must not be empty"));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("ws://") {
        if lower.len() <= "ws://".len() {
            return Err(AdapterError::protocol("ws url has no host"));
        }
        return Ok(());
    }
    if lower.starts_with("wss://") {
        return Err(AdapterError::unsupported(
            "wss:// requires a TLS backend, which hbot-adapters does not link; \
             use ws:// on a loopback/private link or add a TLS stack",
        ));
    }
    Err(AdapterError::blocked(format!(
        "unsupported websocket scheme in `{trimmed}`: only ws:// is allowed"
    )))
}

/// 一条待发送的动作。
struct ActionCommand {
    action: String,
    params: Value,
    echo: String,
    reply: oneshot::Sender<Result<Value, AdapterError>>,
}

/// 动作句柄：可克隆，投递到同一条串行 WebSocket 写路径。
#[derive(Clone)]
pub struct ActionHandle {
    tx: mpsc::Sender<ActionCommand>,
    next_echo: Arc<AtomicU64>,
}

impl ActionHandle {
    /// 发送动作并等待平台响应；返回平台响应的 `data` 字段。
    pub async fn send(
        &self,
        action: impl Into<String>,
        params: Value,
    ) -> Result<Value, AdapterError> {
        let echo = format!("hbot-{}", self.next_echo.fetch_add(1, Ordering::Relaxed));
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ActionCommand {
                action: action.into(),
                params,
                echo,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AdapterError::transport("onebot connection is not running"))?;
        reply_rx
            .await
            .map_err(|_| AdapterError::transport("onebot connection closed before response"))?
    }

    /// 发送群消息；`reply_to` 非空时在消息段前加 `reply` 段。
    pub async fn send_group_msg(
        &self,
        group_id: i64,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<Value, AdapterError> {
        self.send(
            "send_group_msg",
            json!({
                "group_id": group_id,
                "message": outbound_json(text, reply_to),
            }),
        )
        .await
    }

    /// 发送私聊消息；`reply_to` 非空时在消息段前加 `reply` 段。
    pub async fn send_private_msg(
        &self,
        user_id: i64,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<Value, AdapterError> {
        self.send(
            "send_private_msg",
            json!({
                "user_id": user_id,
                "message": outbound_json(text, reply_to),
            }),
        )
        .await
    }
}

impl ActionSender for ActionHandle {
    fn send_action<'a>(
        &'a self,
        action: String,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, AdapterError>> {
        Box::pin(async move { self.send(action, params).await })
    }
}

impl fmt::Debug for ActionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionHandle")
            .field("next_echo", &self.next_echo.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// 客户端：事件出口 + 动作入口 + supervisor 任务。
pub struct OneBotClient {
    actions: ActionHandle,
    events: mpsc::Receiver<NormalizedInbound>,
    cancel: watch::Sender<bool>,
    /// supervisor 任务；`Option` 是为了 `shutdown` 里能把它取出来 await
    /// （带 `Drop` 的类型不能直接移出字段）。
    task: Option<JoinHandle<()>>,
}

impl OneBotClient {
    /// 启动客户端（立即返回，连接在后台建立/重连）。
    ///
    /// 校验失败（空容量、非 `ws://`）直接返回错误，不起任务。
    pub fn start(config: OneBotConfig) -> Result<Self, AdapterError> {
        validate_ws_url(&config.ws_url)?;
        if config.event_capacity == 0 {
            return Err(AdapterError::unsupported("event capacity must be > 0"));
        }
        if config.action_capacity == 0 {
            return Err(AdapterError::unsupported("action capacity must be > 0"));
        }
        let (events_tx, events_rx) = mpsc::channel(config.event_capacity);
        let (actions_tx, actions_rx) = mpsc::channel(config.action_capacity);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let next_echo = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(supervise(config, events_tx, actions_rx, cancel_rx));
        Ok(Self {
            actions: ActionHandle {
                tx: actions_tx,
                next_echo,
            },
            events: events_rx,
            cancel: cancel_tx,
            task: Some(task),
        })
    }

    /// 取下一个已归一化的入站消息；通道关闭（客户端停机）返回 `None`。
    pub async fn next_event(&mut self) -> Option<NormalizedInbound> {
        self.events.recv().await
    }

    /// 直接发送任意 OneBot 动作。
    pub async fn send(
        &self,
        action: impl Into<String>,
        params: Value,
    ) -> Result<Value, AdapterError> {
        self.actions.send(action, params).await
    }

    /// 可克隆的动作句柄（交给 [`super::send::OutboundSender`] 用）。
    pub fn action_handle(&self) -> ActionHandle {
        self.actions.clone()
    }

    /// 发送群消息。
    pub async fn send_group_msg(
        &self,
        group_id: i64,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<Value, AdapterError> {
        self.actions.send_group_msg(group_id, text, reply_to).await
    }

    /// 发送私聊消息。
    pub async fn send_private_msg(
        &self,
        user_id: i64,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<Value, AdapterError> {
        self.actions.send_private_msg(user_id, text, reply_to).await
    }

    /// 主动停机：通知 supervisor 退出并等待任务结束。
    pub async fn shutdown(mut self) {
        let _ = self.cancel.send(true);
        // supervisor 每个 select 分支都会看到 cancel；读阻塞时也能被打断。
        if let Some(task) = self.task.take() {
            if let Err(err) = task.await {
                tracing::warn!(error = %err, "onebot supervisor task ended abnormally");
            }
        }
    }
}

impl Drop for OneBotClient {
    fn drop(&mut self) {
        // 不 await（Drop 里不能），只发信号；任务会在下一次 select 退出。
        let _ = self.cancel.send(true);
    }
}

impl fmt::Debug for OneBotClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneBotClient").finish_non_exhaustive()
    }
}

/// 心跳看门狗：记录最后一次"活着"的证据，超时即判死。
///
/// 任何入站帧（含 heartbeat / lifecycle / 普通事件 / 动作响应）都算证据：
/// 心跳只是最常见的活证，不是唯一。
#[derive(Debug, Clone)]
pub struct HeartbeatWatchdog {
    timeout: Duration,
    last_seen: Instant,
}

impl HeartbeatWatchdog {
    /// 以 `now` 为起点开始计时。
    pub fn new(timeout: Duration, now: Instant) -> Self {
        Self {
            timeout,
            last_seen: now,
        }
    }

    /// 记录一次活证。
    pub fn observe(&mut self, now: Instant) {
        self.last_seen = now;
    }

    /// 下一次判死的时刻。
    pub fn deadline(&self) -> Instant {
        self.last_seen + self.timeout
    }

    /// 是否已超时。
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline()
    }

    /// 超时阈值。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// 一帧 WebSocket 文本的分类结果。
#[derive(Debug)]
pub enum Frame {
    /// 消息事件（已归一化）。
    Event(Box<NormalizedInbound>),
    /// meta_event（heartbeat/lifecycle），用于心跳活性。
    Meta(Box<OneBotEvent>),
    /// 动作响应（按 `echo` 配对）。
    Response {
        /// 动作请求里带的 `echo`，用于与响应配对。
        echo: String,
        /// 成功为平台响应的 `data`；失败为平台报错（`retcode != 0` / `status` 非 ok）。
        result: Result<Value, AdapterError>,
    },
    /// 不关心或解析不了的帧；**不致命**，只记日志。
    Ignored {
        /// 被忽略的原因（进 debug 日志，便于排查实现方言）。
        reason: String,
    },
}

/// 把一帧文本分类。**总是返回**（坏帧落 `Ignored`）：单帧异常不该拖垮连接。
pub fn classify_frame(text: &str, clock: &dyn Clock) -> Frame {
    if text.len() > MAX_FRAME_TEXT_BYTES {
        return Frame::Ignored {
            reason: format!("frame too large: {} bytes", text.len()),
        };
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            return Frame::Ignored {
                reason: format!("invalid json: {err}"),
            };
        }
    };

    // 先看 echo：动作响应与事件理论上不会同时带 echo，但响应优先。
    if let Some(echo) = value.get("echo").and_then(Value::as_str) {
        let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(0);
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let ok_status = status.is_empty()
            || status.eq_ignore_ascii_case("ok")
            || status.eq_ignore_ascii_case("async");
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let result = if retcode == 0 && ok_status {
            Ok(data)
        } else {
            let detail = value
                .get("msg")
                .or_else(|| value.get("wording"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Err(AdapterError::protocol(format!(
                "onebot action failed: retcode={retcode} status={status:?} msg={detail:?}"
            )))
        };
        return Frame::Response {
            echo: echo.to_string(),
            result,
        };
    }

    let event: OneBotEvent = match serde_json::from_value(value.clone()) {
        Ok(event) => event,
        Err(err) => {
            return Frame::Ignored {
                reason: format!("not a onebot event: {err}"),
            };
        }
    };

    match event.post_type {
        super::event::PostType::Message => match normalize(&event, value, clock.now()) {
            Ok(normalized) => Frame::Event(Box::new(normalized)),
            Err(err) => Frame::Ignored {
                reason: format!("message event rejected: {err}"),
            },
        },
        super::event::PostType::MetaEvent => Frame::Meta(Box::new(event)),
        other => Frame::Ignored {
            reason: format!("post_type `{}` not handled by the adapter", other.as_str()),
        },
    }
}

/// 一次连接会话的结局。
enum SessionOutcome {
    /// 主动停机。
    Cancelled,
    /// 连接断了/心跳超时，需要重连。
    Ended {
        error: Option<AdapterError>,
        /// 本次会话是否收到过任何入站帧（决定退避是否归零）。
        had_activity: bool,
    },
}

/// 超级循环：连 → 跑 → 断了按退避重连，直到 cancel。
async fn supervise(
    config: OneBotConfig,
    events_tx: mpsc::Sender<NormalizedInbound>,
    mut actions_rx: mpsc::Receiver<ActionCommand>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut attempt: u32 = 0;
    loop {
        if *cancel_rx.borrow() {
            break;
        }
        let outcome = session(&config, &events_tx, &mut actions_rx, &mut cancel_rx).await;
        match outcome {
            SessionOutcome::Cancelled => break,
            SessionOutcome::Ended {
                error,
                had_activity,
            } => {
                if had_activity {
                    // 连通过并且有数据：下次断线重新从 1s 起算，避免长时间运行后
                    // 一次抖动直接等 30s。
                    attempt = 0;
                }
                attempt = attempt.saturating_add(1);
                let delay = config.backoff.delay(attempt, config.jitter.as_ref());
                tracing::warn!(
                    attempt,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = error.map(|err| err.to_string()).unwrap_or_default(),
                    "onebot connection lost; reconnecting"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    _ = cancel_rx.changed() => break,
                }
            }
        }
    }
    // 停机：让所有在等的动作立即失败，而不是挂到超时。
    actions_rx.close();
    while let Ok(command) = actions_rx.try_recv() {
        let _ = command
            .reply
            .send(Err(AdapterError::transport("onebot client stopped")));
    }
    tracing::info!("onebot supervisor stopped");
}

/// 单次连接会话。
async fn session(
    config: &OneBotConfig,
    events_tx: &mpsc::Sender<NormalizedInbound>,
    actions_rx: &mut mpsc::Receiver<ActionCommand>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> SessionOutcome {
    let ws = match connect_ws(config).await {
        Ok(ws) => ws,
        Err(error) => {
            return SessionOutcome::Ended {
                error: Some(error),
                had_activity: false,
            };
        }
    };
    tracing::info!(url = %config.ws_url, "onebot websocket connected");
    let (mut write, mut read) = ws.split();
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, AdapterError>>> = HashMap::new();
    let mut watchdog = config
        .heartbeat_timeout
        .map(|timeout| HeartbeatWatchdog::new(timeout, Instant::now()));
    let mut had_activity = false;

    loop {
        // 没有看门狗时给一个很远的唤醒点（1h），保持 select 结构统一。
        let wake_at = watchdog
            .as_ref()
            .map(HeartbeatWatchdog::deadline)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    fail_pending(&mut pending, "onebot client stopped");
                    return SessionOutcome::Cancelled;
                }
            }
            command = actions_rx.recv() => {
                match command {
                    None => {
                        // 所有 ActionHandle 都已释放 + 客户端停机。
                        fail_pending(&mut pending, "onebot client stopped");
                        return SessionOutcome::Cancelled;
                    }
                    Some(command) => {
                        pending.insert(command.echo.clone(), command.reply);
                        let frame = json!({
                            "action": command.action,
                            "params": command.params,
                            "echo": command.echo,
                        });
                        let text = match serde_json::to_string(&frame) {
                            Ok(text) => text,
                            Err(err) => {
                                // 参数不可序列化：按协议错误回给调用方，连接继续。
                                if let Some(reply) = pending.remove(&command.echo) {
                                    let _ = reply.send(Err(AdapterError::protocol(format!(
                                        "action params are not serializable: {err}"
                                    ))));
                                }
                                continue;
                            }
                        };
                        if let Err(err) = write.send(WsMessage::text(text)).await {
                            fail_pending(&mut pending, "onebot websocket write failed");
                            return SessionOutcome::Ended {
                                error: Some(AdapterError::transport(format!("ws write failed: {err}"))),
                                had_activity,
                            };
                        }
                    }
                }
            }
            message = read.next() => {
                let Some(message) = message else {
                    fail_pending(&mut pending, "onebot websocket closed by peer");
                    return SessionOutcome::Ended {
                        error: Some(AdapterError::transport("websocket closed by peer")),
                        had_activity,
                    };
                };
                let message = match message {
                    Ok(message) => message,
                    Err(err) => {
                        fail_pending(&mut pending, "onebot websocket read failed");
                        return SessionOutcome::Ended {
                            error: Some(AdapterError::transport(format!("ws read failed: {err}"))),
                            had_activity,
                        };
                    }
                };
                match &message {
                    WsMessage::Ping(payload) => {
                        if let Err(err) = write.send(WsMessage::Pong(payload.clone())).await {
                            fail_pending(&mut pending, "onebot websocket write failed");
                            return SessionOutcome::Ended {
                                error: Some(AdapterError::transport(format!("ws pong failed: {err}"))),
                                had_activity,
                            };
                        }
                    }
                    WsMessage::Close(_) => {
                        fail_pending(&mut pending, "onebot websocket closed by peer");
                        return SessionOutcome::Ended {
                            error: Some(AdapterError::transport("websocket closed by peer")),
                            had_activity,
                        };
                    }
                    WsMessage::Pong(_) => {
                        // 协议层 pong 也算活证。
                        if let Some(watchdog) = watchdog.as_mut() {
                            watchdog.observe(Instant::now());
                        }
                    }
                    WsMessage::Text(_) | WsMessage::Binary(_) => {
                        let text = frame_text(&message);
                        let Some(text) = text else {
                            tracing::debug!("onebot: dropped frame that is not valid utf-8 text");
                            continue;
                        };
                        if let Some(watchdog) = watchdog.as_mut() {
                            watchdog.observe(Instant::now());
                        }
                        let frame = classify_frame(&text, config.clock.as_ref());
                        if let Some(outcome) = dispatch_frame(
                            frame,
                            events_tx,
                            &mut pending,
                            &mut had_activity,
                        ).await {
                            return outcome;
                        }
                    }
                    // Frame / other variants: 不关心。
                    _ => {}
                }
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {
                let timeout_ms = watchdog
                    .as_ref()
                    .map(|w| u64::try_from(w.timeout().as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or_default();
                fail_pending(&mut pending, "onebot heartbeat timeout");
                return SessionOutcome::Ended {
                    error: Some(AdapterError::Timeout {
                        operation: "onebot heartbeat".to_string(),
                        timeout_ms,
                    }),
                    had_activity,
                };
            }
        }
    }
}

/// 从帧里取文本：`Text` 直接用，`Binary` 尝试 UTF-8（部分实现发二进制 JSON）。
fn frame_text(message: &WsMessage) -> Option<String> {
    match message {
        WsMessage::Text(_) => message.to_text().ok().map(str::to_owned),
        WsMessage::Binary(bytes) => std::str::from_utf8(bytes.as_ref()).ok().map(str::to_owned),
        _ => None,
    }
}

/// 让所有在等的动作立即失败（连接断了，响应不会来了）。
fn fail_pending(
    pending: &mut HashMap<String, oneshot::Sender<Result<Value, AdapterError>>>,
    reason: &str,
) {
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(AdapterError::transport(reason)));
    }
}

/// 分发一帧；返回 `Some(outcome)` 表示会话要结束。
async fn dispatch_frame(
    frame: Frame,
    events_tx: &mpsc::Sender<NormalizedInbound>,
    pending: &mut HashMap<String, oneshot::Sender<Result<Value, AdapterError>>>,
    had_activity: &mut bool,
) -> Option<SessionOutcome> {
    match frame {
        Frame::Event(normalized) => {
            *had_activity = true;
            // 有界通道满 → 这里 await，读取暂停（背压），不丢消息。
            if events_tx.send(*normalized).await.is_err() {
                tracing::warn!("onebot event receiver dropped; stopping session");
                return Some(SessionOutcome::Ended {
                    error: None,
                    had_activity: *had_activity,
                });
            }
        }
        Frame::Meta(event) => {
            *had_activity = true;
            tracing::trace!(
                meta_event_type = event.meta_event_type.as_deref().unwrap_or(""),
                "onebot meta event"
            );
        }
        Frame::Response { echo, result } => {
            *had_activity = true;
            match pending.remove(&echo) {
                Some(reply) => {
                    // 调用方可能已放弃等待，忽略发送失败。
                    let _ = reply.send(result);
                }
                None => {
                    tracing::debug!(echo, "onebot: unsolicited action response");
                }
            }
        }
        Frame::Ignored { reason } => {
            tracing::debug!(reason, "onebot: ignored frame");
        }
    }
    None
}

/// 建立一次 WebSocket 连接。
async fn connect_ws(config: &OneBotConfig) -> Result<WsStream, AdapterError> {
    let mut request = config
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|err| AdapterError::protocol(format!("invalid ws url: {err}")))?;
    if let Some(token) = config
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            AdapterError::protocol("access token cannot be encoded as an http header value")
        })?;
        request.headers_mut().insert("authorization", value);
    }
    let ws_config = WebSocketConfig::default();
    let (stream, _response) =
        tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false)
            .await
            .map_err(|err| AdapterError::transport(format!("ws connect failed: {err}")))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;

    #[test]
    fn validate_allows_ws_and_rejects_everything_else() {
        assert!(validate_ws_url("ws://127.0.0.1:3001").is_ok());
        assert!(validate_ws_url("  WS://127.0.0.1:3001  ").is_ok());
        assert!(validate_ws_url("wss://example.com/ws").is_err());
        assert!(validate_ws_url("http://127.0.0.1:3001").is_err());
        assert!(validate_ws_url("file:///etc/passwd").is_err());
        assert!(validate_ws_url("ws://").is_err());
        assert!(validate_ws_url("").is_err());
    }

    #[test]
    fn wss_error_says_tls_is_missing() {
        let err = validate_ws_url("wss://example.com/ws").expect_err("wss must be rejected");
        assert!(matches!(err, AdapterError::Unsupported { .. }));
        assert!(err.to_string().contains("TLS"));
    }

    #[test]
    fn watchdog_expires_after_timeout() {
        let start = Instant::now();
        let mut watchdog = HeartbeatWatchdog::new(Duration::from_secs(30), start);
        assert!(!watchdog.expired(start + Duration::from_secs(29)));
        assert!(watchdog.expired(start + Duration::from_secs(31)));
        watchdog.observe(start + Duration::from_secs(20));
        assert!(!watchdog.expired(start + Duration::from_secs(40)));
        assert!(watchdog.expired(start + Duration::from_secs(51)));
    }

    #[test]
    fn classify_routes_frames_without_panicking() {
        let clock = SystemClock;
        let private = r#"{"post_type":"message","message_type":"private","sub_type":"friend",
            "message_id":7,"user_id":42,"raw_message":"hi","time":1700000000,
            "message":"hi","sender":{"nickname":"n"}}"#;
        match classify_frame(private, &clock) {
            Frame::Event(event) => {
                assert_eq!(event.external_id, "7");
                assert_eq!(event.text, "hi");
            }
            other => panic!("expected event, got {other:?}"),
        }

        let heartbeat = r#"{"post_type":"meta_event","meta_event_type":"heartbeat","time":1}"#;
        assert!(matches!(classify_frame(heartbeat, &clock), Frame::Meta(_)));

        let response = r#"{"status":"ok","retcode":0,"data":{"message_id":9},"echo":"hbot-1"}"#;
        match classify_frame(response, &clock) {
            Frame::Response { echo, result } => {
                assert_eq!(echo, "hbot-1");
                let data = result.expect("ok response");
                assert_eq!(data.get("message_id").and_then(Value::as_i64), Some(9));
            }
            other => panic!("expected response, got {other:?}"),
        }

        let failed = r#"{"status":"failed","retcode":100,"msg":"bad","echo":"hbot-2"}"#;
        match classify_frame(failed, &clock) {
            Frame::Response { result, .. } => assert!(result.is_err()),
            other => panic!("expected failed response, got {other:?}"),
        }

        assert!(matches!(
            classify_frame("not json", &clock),
            Frame::Ignored { .. }
        ));
        assert!(matches!(
            classify_frame(
                r#"{"post_type":"notice","notice_type":"group_upload"}"#,
                &clock
            ),
            Frame::Ignored { .. }
        ));
        assert!(matches!(
            classify_frame(r#"{"post_type":"message"}"#, &clock),
            Frame::Ignored { .. }
        ));
    }

    #[test]
    fn config_debug_redacts_token() {
        let config = OneBotConfig::new("ws://127.0.0.1:3001").with_token("super-secret");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("<redacted>"));
    }
}
