//! QQ 官方网关的 **WebSocket 客户端**：取 token → 取网关地址 → 握手 → 心跳 → 分发 → 重连。
//!
//! 拓扑（`docs/02-architecture.md` §4 的消息主链路）：
//!
//! ```text
//! bots.qq.com/app/getAppAccessToken        （POST，取 access_token）
//! api.sgroup.qq.com/gateway                （GET，取 wss:// 地址）
//! api.sgroup.qq.com/websocket/             （WS，本端主动连出）
//!   │
//!   ▼
//! supervisor ── 指数退避 1/2/4…30s ±20% 抖动（复用 onebot/backoff.rs）──► session
//!                                                                          │
//!                                         events: bounded mpsc ────────────┘
//!                                         事件 → NormalizedInbound
//! ```
//!
//! # 帧处理表（实测口径，未核实项已标）
//!
//! | 帧 | 方向 | 本模块的处理 |
//! |---|---|---|
//! | `op:10` Hello（`d.heartbeat_interval`，毫秒） | 服务端 → 客户端 | 取心跳间隔；**缺失即报 protocol 错**，不猜默认值 |
//! | `op:2` Identify（`d.token`/`d.intents`/`d.shard`） | 客户端 → 服务端 | 仅在没有可复用会话时发 |
//! | `op:6` Resume（`d.token`/`d.session_id`/`d.seq`） | 客户端 → 服务端 | 有 `session_id` + `seq` 时发（断线重连优先 Resume） |
//! | `op:1` Heartbeat（`d` = 最近事件的 `s`，可为 `null`） | 双向 | 按服务器给的间隔发；服务端主动要心跳时立刻补发一次 |
//! | `op:11` Heartbeat ACK | 服务端 → 客户端 | 清看门狗截止时间；**连续一个心跳周期没有 ACK**（即 `heartbeat_timeout` 到期）就重连 |
//! | `op:0` Dispatch（`t`/`s`/`d`） | 服务端 → 客户端 | 更新 `seq`；`READY` 记 `session_id`；消息事件归一化后投递 |
//! | `op:7` Reconnect | 服务端 → 客户端 | 结束当前会话（**保留** `session_id`/`seq`，下次走 Resume） |
//! | `op:9` Invalid Session | 服务端 → 客户端 | 清 `session_id`/`seq`，下次**重新 Identify** |
//! | 其它 `op` | — | 记 debug 日志后忽略 |
//!
//! WebSocket 层的 `Ping` 回 `Pong`；`Pong` 只当活证。
//!
//! # 几条刻意的约束
//!
//! 1. **不丢消息**：`events` 通道有界，满了就让 session 停止读取套接字（背压），
//!    而不是丢弃或无限增长（`AGENTS.md` §5「通道一律有界」）。
//! 2. **先建队列再连接**：`start()` 先建好通道与 supervisor 再返回，重连窗口内
//!    产生的事件不会因为「订阅还没建」而丢。
//! 3. **分类失败不杀连接**：单帧解析失败只 warn 并跳过（网关偶发非标准帧），
//!    只有传输层错误 / 心跳看门狗超时 / `op:7` / `op:9` 才结束会话。
//! 4. **TLS 只校验、不降级**：`wss://` 走 `crate::tls` 的 rustls 配置，证书校验失败
//!    报 [`AdapterError::Tls`]，不会退回明文，也没有跳过校验的开关。私有 CA 用
//!    [`QqConfig::with_extra_root_certificates`] 追加信任根。
//! 5. **重连的退避算法不重写**：直接复用 `platforms::onebot::backoff`（`Backoff` +
//!    `Jitter`）。两个平台的退避口径本来就一致（1s 起、30s 封顶、±20% 抖动），
//!    在这里再写一份就变成两个真源。
//!
//! # 会话多久会「过期」
//!
//! `session_id` 只在网关侧有效（**有效期未核实**）。本模块不做本地计时淘汰，
//! 只靠服务端的 `op:9`（invalid session）/ 连接失败来兜底：Resume 被拒时服务端
//! 会直接给 `op:9`，此时清空会话状态、下一次 Identify 即可。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::error::AdapterError;
use crate::normalize::NormalizedInbound;
use crate::platforms::qq::api::ApiClient;
use crate::platforms::qq::auth::TokenProvider;
use crate::platforms::qq::config::QqConfig;
use crate::platforms::qq::event::{
    EVENT_READY, GatewayFrame, OP_DISPATCH, OP_HEARTBEAT, OP_HEARTBEAT_ACK, OP_HELLO,
    OP_INVALID_SESSION, OP_RECONNECT, heartbeat_payload, identify_payload, resume_payload,
};
use crate::platforms::qq::normalize::normalize;
use crate::platforms::qq::send::{QqSender, SendReceipt};

/// 已升级的 WebSocket 流（ws 或 wss）。
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 一个 QQ 官方机器人客户端。
///
/// 由 [`QqClient::start`] 创建；后台任务负责「连接 → 握手 → 心跳 → 分发 → 重连」，
/// 调用方只从 [`QqClient::next_event`] 取归一化后的入站消息。
///
/// ```no_run
/// # use hbot_adapters::qq::{QqClient, QqConfig};
/// # async fn run() -> Result<(), hbot_adapters::AdapterError> {
/// let config = QqConfig::new("app-id", "app-secret");
/// let mut client = QqClient::start(config)?;
/// while let Some(message) = client.next_event().await {
///     let _ = client.reply(&message, "收到").await;
/// }
/// # Ok(())
/// # }
/// ```
pub struct QqClient {
    events: mpsc::Receiver<NormalizedInbound>,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    sender: QqSender,
}

impl QqClient {
    /// 启动客户端：校验配置、建好有界事件通道、spawn 后台 supervisor，然后立刻返回。
    ///
    /// 这里**不会**阻塞等握手完成 —— 网关是否可达、凭据是否有效都在后台重试中体现，
    /// 调用方不需要（也不应该）在启动路径上卡一个网络往返。
    ///
    /// # Errors
    ///
    /// 配置非法（空 `app_id`/`client_secret`、容量为 0、端点不是 http(s)、shard 越界）
    /// 时返回 [`AdapterError::Protocol`]。
    pub fn start(config: QqConfig) -> Result<Self, AdapterError> {
        config.validate()?;
        let auth = Arc::new(TokenProvider::new(&config));
        let api = Arc::new(ApiClient::new(&config, Arc::clone(&auth)));
        let sender = QqSender::new(Arc::clone(&api), config.msg_seq_capacity());
        let (events_tx, events) = mpsc::channel(config.event_capacity());
        let (cancel, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(config, api, events_tx, cancel_rx));
        Ok(Self {
            events,
            cancel,
            task: Some(task),
            sender,
        })
    }

    /// 取下一条归一化后的入站消息；客户端停机（或事件通道关闭）时返回 `None`。
    ///
    /// 底层是有界通道：消费端慢下来时，后台 session 会停止读套接字（背压），
    /// 消息留在网关/内核缓冲里，**不会**被静默丢弃。
    pub async fn next_event(&mut self) -> Option<NormalizedInbound> {
        self.events.recv().await
    }

    /// 取一个可 clone 的被动回复发送器（可以交给别的任务长期持有）。
    pub fn sender(&self) -> QqSender {
        self.sender.clone()
    }

    /// 按入站消息的会话自动选被动回复路径（见 [`QqSender::reply`]）。
    pub async fn reply(
        &self,
        message: &NormalizedInbound,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        self.sender.reply(message, content).await
    }

    /// 优雅关闭：通知后台任务退出并等它结束。
    ///
    /// 后台任务在等一次 HTTP/WS 操作时有超时上限（[`QqConfig::request_timeout`] /
    /// [`QqConfig::connect_timeout`]），所以这里最多等一个超时周期。
    pub async fn stop(mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for QqClient {
    /// `Drop` 里无法 `await`，所以只发取消信号并 `abort` 后台任务。
    ///
    /// 想让它把当前的 `Close` 帧写出去，用 [`QqClient::stop`]。
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// 跨重连保留的会话状态：断线后能 Resume 就 Resume，能省一次全量快照。
#[derive(Debug, Default)]
struct SessionState {
    session_id: Option<String>,
    last_seq: Option<i64>,
}

impl SessionState {
    /// 清空会话（收到 `op:9` invalid session 时用）。
    fn reset(&mut self) {
        self.session_id = None;
        self.last_seq = None;
    }
}

/// 一次会话的结局。
enum SessionOutcome {
    /// 调用方要求停机。
    Cancelled,
    /// 连接断了 / 被服务端要求重连 / 握手失败。
    Ended {
        /// 失败原因（正常关闭时为 `None`）。
        error: Option<AdapterError>,
        /// 这次会话有没有真的收到过帧（用来决定退避是否从 0 重算）。
        had_activity: bool,
    },
}

impl SessionOutcome {
    fn ended(error: Option<AdapterError>, had_activity: bool) -> Self {
        Self::Ended {
            error,
            had_activity,
        }
    }
}

/// 后台 supervisor：一直重连到被取消为止。
async fn supervise(
    config: QqConfig,
    api: Arc<ApiClient>,
    events: mpsc::Sender<NormalizedInbound>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut state = SessionState::default();
    let mut attempt: u32 = 0;
    loop {
        if *cancel.borrow() {
            return;
        }
        let outcome = session(&config, &api, &events, &mut cancel, &mut state).await;
        let (error, had_activity) = match outcome {
            SessionOutcome::Cancelled => return,
            SessionOutcome::Ended {
                error,
                had_activity,
            } => (error, had_activity),
        };
        if let Some(error) = &error {
            tracing::warn!(error = %error, had_activity, "qq gateway session ended");
        } else {
            tracing::debug!(had_activity, "qq gateway session ended");
        }
        // 上一次连接确实工作过 → 退避从 1s 重新开始；否则继续加长（避免
        // 凭据错/网络断时把平台打穿）。
        if had_activity {
            attempt = 0;
        }
        attempt = attempt.saturating_add(1);
        let delay = config.backoff().delay(attempt, config.jitter());
        tracing::debug!(
            attempt,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "qq gateway reconnecting after backoff"
        );
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// 一次完整会话：发现网关 → 连接 → 等 Hello → Identify/Resume → 主循环。
async fn session(
    config: &QqConfig,
    api: &ApiClient,
    events: &mpsc::Sender<NormalizedInbound>,
    cancel: &mut watch::Receiver<bool>,
    state: &mut SessionState,
) -> SessionOutcome {
    let mut had_activity = false;

    // 1) 取网关地址。每次会话都重新取：平台可能在重连时给另一个节点。
    let gateway_url = match discover_gateway(config, api).await {
        Ok(url) => url,
        Err(error) => return SessionOutcome::ended(Some(error), false),
    };

    // 2) 取 token（单飞 + 提前续期，见 `auth.rs`）。
    let token = match api.auth().access_token().await {
        Ok(token) => token,
        Err(error) => return SessionOutcome::ended(Some(error), false),
    };

    // 3) 建连。
    let ws = match connect_ws(&gateway_url, config).await {
        Ok(ws) => ws,
        Err(error) => return SessionOutcome::ended(Some(error), false),
    };
    tracing::debug!(url = %gateway_url, "qq gateway connected");
    let (mut write, mut read) = ws.split();

    // 4) 等 Hello（服务端连上就先发 op:10）。
    let interval = match wait_for_hello(&mut read, config.connect_timeout()).await {
        Ok(interval) => interval,
        Err(error) => return SessionOutcome::ended(Some(error), false),
    };
    tracing::debug!(
        interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
        "qq gateway hello received"
    );

    // 5) 有可复用会话就 Resume，否则 Identify。
    let handshake = match (state.session_id.as_deref(), state.last_seq) {
        (Some(session_id), Some(seq)) if !session_id.is_empty() => {
            resume_payload(token.access_token(), session_id, seq)
        }
        _ => identify_payload(token.access_token(), config.intents(), config.shard()),
    };
    if let Err(error) = write_frame(&mut write, &handshake).await {
        return SessionOutcome::ended(Some(error), false);
    }

    // 6) 心跳调度 + 看门狗。
    let ack_timeout = config.heartbeat_timeout().unwrap_or(interval * 2);
    let mut next_beat = Instant::now() + interval;
    let mut ack_deadline: Option<Instant> = None;

    loop {
        let wake = match ack_deadline {
            Some(deadline) => deadline.min(next_beat),
            None => next_beat,
        };
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return SessionOutcome::Cancelled;
                }
            }
            () = tokio::time::sleep_until(wake) => {
                let now = Instant::now();
                if let Some(deadline) = ack_deadline {
                    if now >= deadline {
                        // 心跳发出去了但一个周期内没有 ACK：当断线处理，不静默假死。
                        return SessionOutcome::ended(
                            Some(AdapterError::timeout("qq heartbeat ack", ack_timeout)),
                            had_activity,
                        );
                    }
                }
                if now >= next_beat {
                    let payload = heartbeat_payload(state.last_seq);
                    if let Err(error) = write_frame(&mut write, &payload).await {
                        return SessionOutcome::ended(Some(error), had_activity);
                    }
                    // 看门狗只从**第一条未被 ACK 的心跳**开始计时：后续心跳不能把
                    // 截止时间一直往后推，否则永远等不到超时、连接静默假死。
                    if ack_deadline.is_none() {
                        ack_deadline = Some(Instant::now() + ack_timeout);
                    }
                    next_beat = Instant::now() + interval;
                }
            }
            message = read.next() => {
                let Some(message) = message else {
                    return SessionOutcome::ended(
                        Some(AdapterError::transport("qq gateway stream ended")),
                        had_activity,
                    );
                };
                let message = match message {
                    Ok(message) => message,
                    Err(err) => {
                        return SessionOutcome::ended(
                            Some(AdapterError::transport(format!("qq gateway read failed: {err}"))),
                            had_activity,
                        );
                    }
                };
                // WebSocket 控制帧。
                match &message {
                    WsMessage::Ping(payload) => {
                        if let Err(err) = write.send(WsMessage::Pong(payload.clone())).await {
                            return SessionOutcome::ended(
                                Some(AdapterError::transport(format!("qq gateway pong failed: {err}"))),
                                had_activity,
                            );
                        }
                        continue;
                    }
                    WsMessage::Pong(_) => {
                        had_activity = true;
                        continue;
                    }
                    WsMessage::Close(frame) => {
                        let reason = frame.as_ref().map(|frame| frame.to_string());
                        return SessionOutcome::ended(
                            Some(AdapterError::transport(match reason {
                                Some(reason) => format!("qq gateway closed the connection: {reason}"),
                                None => "qq gateway closed the connection".to_string(),
                            })),
                            had_activity,
                        );
                    }
                    _ => {}
                }
                let Some(text) = frame_text(&message) else {
                    tracing::debug!("qq gateway: dropped frame that is not valid utf-8 text");
                    continue;
                };
                let frame = match GatewayFrame::parse(&text) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // 单帧坏掉不杀连接（网关偶发心跳/系统帧形状不同）。
                        tracing::warn!(error = %err, "qq gateway: skipping malformed frame");
                        continue;
                    }
                };
                had_activity = true;
                match frame.op {
                    OP_DISPATCH => {
                        if let Some(seq) = frame.seq {
                            state.last_seq = Some(seq);
                        }
                        if frame.event_name() == Some(EVENT_READY) {
                            if let Some(session_id) = frame.session_id() {
                                state.session_id = Some(session_id.to_owned());
                            }
                        }
                        match normalize(&frame, config.clock().now()) {
                            Ok(Some(message)) => {
                                // 有界通道：满则 await（背压），不丢消息。
                                if events.send(message).await.is_err() {
                                    tracing::warn!("qq event receiver dropped; stopping session");
                                    return SessionOutcome::ended(None, had_activity);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::warn!(error = %error, "qq gateway: skipping event that failed to normalize");
                            }
                        }
                    }
                    OP_HEARTBEAT_ACK => {
                        ack_deadline = None;
                    }
                    OP_HEARTBEAT => {
                        // 服务端主动要心跳：立刻补一次（`d` 仍带最近 seq）。
                        let payload = heartbeat_payload(state.last_seq);
                        if let Err(error) = write_frame(&mut write, &payload).await {
                            return SessionOutcome::ended(Some(error), had_activity);
                        }
                        // 这次也算「一个未 ACK 的心跳」，但同样不延长已有的截止时间。
                        if ack_deadline.is_none() {
                            ack_deadline = Some(Instant::now() + ack_timeout);
                        }
                    }
                    OP_RECONNECT => {
                        // 服务端要求重连：保留 session_id/seq，下次走 Resume。
                        tracing::info!("qq gateway asked for a reconnect (op:7)");
                        return SessionOutcome::ended(None, had_activity);
                    }
                    OP_INVALID_SESSION => {
                        // invalid session：会话不可复用，清空后在下次会话重新 Identify。
                        tracing::info!("qq gateway rejected the session (op:9); re-identifying");
                        state.reset();
                        return SessionOutcome::ended(None, had_activity);
                    }
                    OP_HELLO => {
                        tracing::debug!("qq gateway: ignoring duplicate hello");
                    }
                    other => {
                        tracing::debug!(op = other, "qq gateway: ignoring unsupported op");
                    }
                }
            }
        }
    }
}

/// 取网关地址：`GET {api_base}/gateway` → `{"url":"wss://…"}`。
async fn discover_gateway(config: &QqConfig, api: &ApiClient) -> Result<String, AdapterError> {
    let response = api.get_json(&config.gateway_url()).await?;
    let url = response
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::protocol("qq gateway response is missing `url`"))?;
    validate_ws_url(url)?;
    Ok(url.to_owned())
}

/// 校验网关地址只接受 `ws://` / `wss://`。
///
/// 非 ws(s) 的 scheme 报 [`AdapterError::Blocked`]（安全策略）：适配器**有** TLS 能力，
/// 这里拒绝的是「拿 http 地址当 WebSocket 用」这种误配。
pub fn validate_ws_url(url: &str) -> Result<(), AdapterError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::protocol("qq gateway url is empty"));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("ws://") || lower.starts_with("wss://") {
        // 至少要有个主机名，不能只有 scheme。
        let rest = &trimmed[trimmed.find("//").map_or(0, |index| index + 2)..];
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with(':') {
            return Err(AdapterError::protocol(format!(
                "qq gateway url is missing a host: `{trimmed}`"
            )));
        }
        return Ok(());
    }
    Err(AdapterError::blocked(format!(
        "unsupported websocket scheme in `{trimmed}`: only ws:// and wss:// are allowed"
    )))
}

/// 连一次网关；`wss://` 走 [`crate::tls`] 的 rustls 配置（校验始终开启）。
async fn connect_ws(url: &str, config: &QqConfig) -> Result<WsStream, AdapterError> {
    let request = url
        .into_client_request()
        .map_err(|err| AdapterError::protocol(format!("invalid qq gateway url: {err}")))?;
    let connector = if is_tls_url(url) {
        Some(Connector::Rustls(crate::tls::client_config(
            config.extra_root_certificates(),
        )?))
    } else {
        None
    };
    let connecting = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(WebSocketConfig::default()),
        false,
        connector,
    );
    let limit = config.connect_timeout();
    match tokio::time::timeout(limit, connecting).await {
        Err(_) => Err(AdapterError::timeout("qq gateway connect", limit)),
        Ok(Err(err)) => Err(ws_connect_error(err)),
        Ok(Ok((stream, _response))) => Ok(stream),
    }
}

/// 等 `op:10` Hello，返回心跳间隔（**必须**由服务端给，不猜默认值）。
async fn wait_for_hello<S>(read: &mut S, limit: Duration) -> Result<Duration, AdapterError>
where
    S: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let waiting = async {
        loop {
            let Some(message) = read.next().await else {
                return Err(AdapterError::transport(
                    "qq gateway closed before sending hello",
                ));
            };
            let message = message
                .map_err(|err| AdapterError::transport(format!("qq gateway read failed: {err}")))?;
            // 握手阶段只关心 op:10；控制帧照常处理，其它帧先忽略。
            match &message {
                WsMessage::Close(frame) => {
                    return Err(AdapterError::transport(match frame {
                        Some(frame) => format!("qq gateway closed before hello: {frame}"),
                        None => "qq gateway closed before hello".to_string(),
                    }));
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                _ => {}
            }
            let Some(text) = frame_text(&message) else {
                continue;
            };
            let frame = match GatewayFrame::parse(&text) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "qq gateway: malformed frame before hello");
                    continue;
                }
            };
            if frame.op != OP_HELLO {
                tracing::debug!(op = frame.op, "qq gateway: ignoring frame before hello");
                continue;
            }
            return frame.hello_interval().ok_or_else(|| {
                AdapterError::protocol(
                    "qq gateway hello frame is missing a positive `d.heartbeat_interval`",
                )
            });
        }
    };
    match tokio::time::timeout(limit, waiting).await {
        Err(_) => Err(AdapterError::timeout("qq gateway hello", limit)),
        Ok(result) => result,
    }
}

/// 发一帧 JSON 文本。
async fn write_frame<S>(write: &mut S, payload: &Value) -> Result<(), AdapterError>
where
    S: futures_util::Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    write
        .send(WsMessage::text(payload.to_string()))
        .await
        .map_err(|err| AdapterError::transport(format!("qq gateway write failed: {err}")))
}

/// 从帧里取文本：`Text` 直接用，`Binary` 尝试 UTF-8（个别网关实现发二进制 JSON）。
fn frame_text(message: &WsMessage) -> Option<String> {
    match message {
        WsMessage::Text(_) => message.to_text().ok().map(str::to_owned),
        WsMessage::Binary(bytes) => std::str::from_utf8(bytes.as_ref()).ok().map(str::to_owned),
        _ => None,
    }
}

/// 连接失败的错误分类。
///
/// 证书问题必须报 [`AdapterError::Tls`]，不能混进笼统的 transport：
/// tokio-tungstenite 把 rustls 的失败塞进 `Error::Io`（见其 `tls.rs` 的
/// `Err(e) => Err(Error::Io(e))`），主机名不合法则给 `Error::Tls(TlsError::InvalidDnsName)`。
fn ws_connect_error(err: tokio_tungstenite::tungstenite::Error) -> AdapterError {
    use tokio_tungstenite::tungstenite::Error as WsError;

    match &err {
        WsError::Io(io) => crate::tls::classify_tls_failure("qq wss handshake failed", io)
            .unwrap_or_else(|| {
                AdapterError::transport(format!("qq gateway connect failed: {err}"))
            }),
        WsError::Tls(_) => AdapterError::tls(format!("qq wss handshake failed: {err}")),
        _ => AdapterError::transport(format!("qq gateway connect failed: {err}")),
    }
}

/// 地址是否 `wss://`（大小写不敏感）。
fn is_tls_url(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("wss://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::qq::config::QqConfig;

    #[test]
    fn validate_allows_ws_and_wss_and_rejects_other_schemes() {
        assert!(validate_ws_url("ws://127.0.0.1:3001").is_ok());
        assert!(validate_ws_url("  WS://127.0.0.1:3001  ").is_ok());
        assert!(validate_ws_url("wss://api.sgroup.qq.com/websocket/").is_ok());
        assert!(validate_ws_url("WSS://api.sgroup.qq.com/websocket/").is_ok());
        assert!(validate_ws_url("http://127.0.0.1:3001").is_err());
        assert!(validate_ws_url("file:///etc/passwd").is_err());
        assert!(validate_ws_url("ws://").is_err());
        assert!(validate_ws_url("wss://").is_err());
        assert!(validate_ws_url("").is_err());
        assert!(validate_ws_url("ws:///path").is_err());
        assert!(validate_ws_url("ws://:8080").is_err());
    }

    #[test]
    fn non_websocket_scheme_is_blocked_not_unsupported() {
        let err = validate_ws_url("http://example.com/ws").expect_err("http must be rejected");
        assert!(matches!(err, AdapterError::Blocked { .. }));
        assert!(err.to_string().contains("ws:// and wss://"));
    }

    #[test]
    fn tls_url_detection_is_case_insensitive() {
        assert!(is_tls_url("wss://api.sgroup.qq.com/websocket/"));
        assert!(is_tls_url("  WSS://x/ws  "));
        assert!(!is_tls_url("ws://127.0.0.1:3001"));
        assert!(!is_tls_url("http://x/ws"));
    }

    #[test]
    fn client_debug_output_does_not_leak_the_secret() {
        // 客户端本身不持有凭据，凭据在 config/token provider 里；
        // 这里顺手钉住「Debug 不打印 secret 的整条链路」在 QqClient 侧也成立。
        let config = QqConfig::new("1024", "sk-test-DEADBEEF");
        let rendered = format!("{config:?}");
        assert!(rendered.contains("<redacted>"), "rendered: {rendered}");
        assert!(
            !rendered.contains("sk-test-DEADBEEF"),
            "rendered: {rendered}"
        );
    }
}
