//! OneBot 11 正向 WebSocket 的 [`BotAdapter`] 实现。
//!
//! ## 为什么要有这个文件（而不是让 supervisor 直接用 `OneBotClient`）
//!
//! `hbot_adapters::platforms::onebot::OneBotClient` 是**一个具体协议客户端**：它自己
//! 管重连、心跳、echo 匹配，事件从内部有界 channel 里 `next_event().await` 取。
//! `hbot_bot::BotAdapter` 是**端口**：`subscribe(&self)` 交出事件流，`send(&self)` 发一条。
//!
//! 两者的所有权要求冲突：`next_event` 要 `&mut self`，而 `subscribe` 只有 `&self`。
//! 解法不是 `Mutex<OneBotClient>`（那样 `subscribe` 之后 supervisor 拿不到事件，因为
//! 读取循环还得反复借那个锁），而是**一个读者任务**：
//!
//! ```text
//! connect(): 只校验配置 + 建事件队列（不碰网络）
//! subscribe(): probe_session() ──► OneBotClient::start_with_session ──► tokio::spawn(reader)
//!              └─ 探测建立的这条连接直接交给常驻客户端：一次 bot_start = **一条** WS 连接
//! send():      ActionHandle（Clone，内部是 action channel）
//! ```
//!
//! 读者任务独占 client 的可变所有权，`subscribe` 只搬一个 receiver 出来。顺带满足
//! `docs/02-architecture.md` §4.1 的背压要求：事件 channel 是**有界**的（容量
//! [`ADAPTER_EVENT_CAPACITY`]），满了 `send().await` 就会卡住读者任务 → 不再从
//! `OneBotClient` 取事件 → 不再从 socket 读。**暂停读取，不丢消息**。
//!
//! ## 已知边界（不要在这里假装支持）
//!
//! * `ws://` 与 `wss://` 都支持：`wss://` 走 rustls + `webpki-roots`，**证书校验始终
//!   开启**，失败报 `AdapterError::Tls`（wire 上是 -32006 internal），不会降级成明文
//!   `ws://`，也**没有**"跳过校验"的开关（私有 CA / 自签部署用
//!   `OneBotConfig::with_extra_root_certificates` 追加根证书）。
//! * **启动前探测（probe → handoff）**：`subscribe()` 先跑一次 [`probe_session`] —— 连上、
//!   等 OneBot 生命周期握手（`meta_event/lifecycle` / 心跳）、失败原因经
//!   [`describe_probe_failure`] 翻成一句人话再进 `bot_start` 的错误返回。**探测建立的
//!   连接不关闭**，而是交给 [`OneBotClient::start_with_session`] 当常驻客户端的第一条
//!   连接：一次 `bot_start` 对平台只开**一次** WS 连接（旧实现是"探测一条 + 真客户端一条"）。
//!   没有这一步时，证书错 / token 错 / 地址错只表现为"启动成功 + 后台反复重连的
//!   `warn`"，用户永远不知道自己配错了（见 `docs/02-architecture.md` §10）。
//!
//!   为什么探测放在 `subscribe` 而不是 `create`：`create` 是 `AdapterFactory` 的同步端口，
//!   它的既有语义是"**只校验配置**、不碰网络"（`tests/bot_runtime.rs::wss_is_supported…`
//!   按这个语义钉了 `create` 对一个没人监听的 `wss://` 地址返回 Ok）。探测要网络、要
//!   `await`，塞进 `create` 会把那条契约一起改掉。`subscribe` 是 `bot_start` 里
//!   `Supervisor::start` 的第一跳，失败照样经 `start_inner` 冒到 `bot_start` 的错误返回，
//!   "启动前探一次"的目的同样达到，而且探测失败时 supervisor 还没开始消费事件。
//! * 被动回复：`OutboundEnvelope.reply_to` 带着触发这条回复的平台消息 ID
//!   （`AGENTS.md` §4.1），这里原样转给 `send_group_msg` / `send_private_msg`，
//!   由 `hbot_adapters::platforms::onebot::message` 组 `reply` 消息段。
//!   `None` = 主动消息，**不**伪造引用段。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_adapters::AdapterError;
use hbot_adapters::platforms::onebot::{
    ConnectedSession, LIFECYCLE_CONFIRM_WINDOW, OneBotClient, OneBotConfig, probe_session,
};
use hbot_bot::supervisor::ADAPTER_EVENT_CAPACITY;
use hbot_bot::{AdapterSubscription, BotAdapter, ConversationRef, OutboundEnvelope, PlatformEvent};
use hbot_core::{ConversationKind, HbotError};
use hbot_store::Bot;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::runtime::AdapterFactory;
use crate::app::runtime::config::BotConfig;

/// OneBot 适配器的平台标识（`bots.platform` 列里存的就是这个）。
pub const PLATFORM: &str = "onebot11";

/// 启动前探测的**超时下限**：10 秒。
///
/// 探测的职责是"别让配错的人继续猜"，不是"快速失败"。用 1 秒那种激进的超时会把
/// "慢但确实能连"（跨地域、TLS 握手排队、反向代理没常驻）判成配置错误 —— 那比不探测更糟，
/// 因为它会让本来能用的 bot 起不来。所以这里取一个**地板值**：显式配置更长就用更长的，
/// 显式配置更短也仍然按 10 秒算（`connect_timeout` 是给持续连接用的，探测跟它的取舍不同）。
pub const PROBE_MIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 一个 bot 的 OneBot 适配器：一个读者任务 + 一个 action 句柄。
pub struct OneBotAdapter {
    /// 出站：`Clone` 的 action 句柄，内部是有界 channel。
    ///
    /// `subscribe` 之前是 `None` —— `connect` / `create` 只校验配置、不碰网络，
    /// 所以这里不能提前造句柄（旧实现会在 `connect` 里直接连一次）。
    actions: Mutex<Option<hbot_adapters::platforms::onebot::ActionHandle>>,
    /// 地址，只用于诊断日志（token 不在这里，绝不打日志）。
    ws_url: String,
    /// 常驻连接配置（重连、容量、退避、心跳）：`subscribe` 时连同探测连接一起交给
    /// [`OneBotClient::start_with_session`]。
    config: OneBotConfig,
    /// 启动前探测用的配置快照（`subscribe` 时用；`connect_timeout` 已被抬到
    /// [`PROBE_MIN_TIMEOUT`] 以上）。
    probe: OneBotConfig,
    /// `self_id`：读者任务用它过滤"挂错号"的事件。
    self_id: Option<i64>,
    /// 入站事件的写端；`subscribe` 取走后为 `None`（第二次订阅由此立刻失败）。
    events_tx: Mutex<Option<mpsc::Sender<PlatformEvent>>>,
    /// 入站事件的一次性交付口：`subscribe` 取走后为 `None`。
    subscriber: Mutex<Option<mpsc::Receiver<PlatformEvent>>>,
    /// 读者任务的句柄；`Drop` 时 abort（client 在任务里，随之关闭连接）。
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl OneBotAdapter {
    /// 校验配置、建好事件队列；**不碰网络**（连接在 [`BotAdapter::subscribe`] 里建立）。
    ///
    /// `self_id` 给了就做一次乐观校验：事件里带了 `self_id` 且不相等时丢弃并 warn
    /// （一个 ws 端点上挂错号是最常见的配置错误）。
    ///
    /// # Errors
    ///
    /// 地址非法（`ws://`/`wss://` 以外）、容量为 0——直接来自
    /// [`OneBotClient::validate`]，经 `From<AdapterError> for HbotError` 映射：
    /// `Unsupported` → -32005。
    pub fn connect(config: OneBotConfig, self_id: Option<i64>) -> Result<Self, HbotError> {
        // 先校验，再起任何任务：地址不合法时不要留下半个连接。
        OneBotClient::validate(&config).map_err(HbotError::from)?;
        let ws_url = config.ws_url.clone();
        // 探测配置要在 `config` 被移交给常驻客户端之前留一份（`OneBotConfig: Clone`）。
        let probe = probe_config(&config);

        // 事件队列先建好（`AGENTS.md` §7.1「先建队列再连接」）：探测交接之前就有一个
        // 空的、有界的事件队列，读者任务一启动就能往里送，重连窗口内的事件不会丢。
        let (tx, rx) = mpsc::channel::<PlatformEvent>(ADAPTER_EVENT_CAPACITY);

        Ok(Self {
            actions: Mutex::new(None),
            ws_url,
            config,
            probe,
            self_id,
            events_tx: Mutex::new(Some(tx)),
            subscriber: Mutex::new(Some(rx)),
            reader: Mutex::new(None),
        })
    }

    /// 从项目配置（`bots.json`）建一个 OneBot 适配器。
    ///
    /// # Errors
    ///
    /// 同 [`OneBotAdapter::connect`]。
    pub fn from_bot_config(config: &BotConfig) -> Result<Self, HbotError> {
        Self::connect(onebot_config(config), config.self_id)
    }

    /// 这个适配器连接的地址（诊断用；不含 token）。
    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }
}

impl std::fmt::Debug for OneBotAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OneBotAdapter")
            .field("ws_url", &self.ws_url)
            .field("subscribed", &lock(&self.subscriber).is_none())
            .field("connected", &lock(&self.actions).is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for OneBotAdapter {
    fn drop(&mut self) {
        // abort 之后 client 在任务里被丢弃，`OneBotClient::drop` 会发 cancel。
        if let Some(reader) = lock(&self.reader).take() {
            reader.abort();
        }
    }
}

#[async_trait]
impl BotAdapter for OneBotAdapter {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        // 事件的唯一消费者：supervisor 的 read_loop。第二次订阅不能静默返回一个永远
        // 收不到事件的 channel（那会让 supervisor 看起来 "Running" 却一条消息都收不到）。
        // 先取写端：它同时充当"是否已订阅"的判据，失败时不要先建一条连接再报错。
        let events_tx = lock(&self.events_tx).take().ok_or_else(|| {
            HbotError::conflict("onebot 适配器只能订阅一次：事件读者任务已经把流交给了上一个订阅者")
        })?;

        // 启动前探测：`bot_start` 走到这里时配置与数据库行都已就位，但**还没**开始消费
        // 事件。连上的这条连接**不关闭**——直接交给常驻客户端做第一次会话（probe →
        // handoff），所以一次 `bot_start` 对平台只开一条 WS 连接。失败就当场报错，让
        // `bot_start` 带着一句人话失败（而不是"启动成功 + 后台无限重连的 warn"）。
        let session = probe_startup(&self.probe).await?;

        let mut client = OneBotClient::start_with_session(self.config.clone(), session)
            .map_err(HbotError::from)?;
        *lock(&self.actions) = Some(client.action_handle());

        let self_id = self.self_id;
        let reader = tokio::spawn(async move {
            read_events(&mut client, &events_tx, self_id).await;
            // 事件流结束（连接被关闭 / 订阅者消失）：显式关闭客户端，让重连任务收尾。
            client.shutdown().await;
        });
        *lock(&self.reader) = Some(reader);

        let receiver = lock(&self.subscriber).take().ok_or_else(|| {
            HbotError::conflict("onebot 适配器只能订阅一次：事件读者任务已经把流交给了上一个订阅者")
        })?;
        Ok(AdapterSubscription::new(receiver))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        // 句柄是 `Clone` 的：guard 在第一个 `.await` 之前就丢掉，避免把 MutexGuard 带过
        // await 点（那会让 `OneBotAdapter` 不再是 `Send`，也会把并发发送串成一条）。
        let actions = lock(&self.actions).clone().ok_or_else(|| {
            HbotError::conflict("onebot 适配器尚未订阅：发送前必须先 subscribe（bot_start 会做）")
        })?;
        let address = address_of(&envelope)?;
        // 被动回复要绑定到触发它的那条平台消息（§4.1）；`None` 是主动消息。
        let reply_to = envelope.reply_to.as_deref();
        // 有附件时读文件 + base64 是阻塞 IO（几 MiB），必须离开 reactor；没有附件就是
        // 一个纯 JSON 构造，仍然走同一个函数，免得两条路径的段顺序各写一遍。
        let text = envelope.text.clone();
        let attachment = envelope.attachment.clone();
        let reply_to_owned = reply_to.map(str::to_string);
        let message = tokio::task::spawn_blocking(move || {
            hbot_adapters::onebot::outbound_json_with_attachment(
                &text,
                reply_to_owned.as_deref(),
                attachment.as_ref(),
            )
        })
        .await
        .map_err(|error| HbotError::internal(format!("附件编码任务没有跑完：{error}")))?
        .map_err(HbotError::from)?;
        let sent = match address {
            Address::Group(group_id) => actions.send_group_message(group_id, message).await,
            Address::Private(user_id) => actions.send_private_message(user_id, message).await,
        };
        sent.map(|_response| ()).map_err(|error| {
            tracing::warn!(
                outbound = %envelope.id,
                conversation = %envelope.conversation_id,
                error = %error,
                "onebot 发送失败"
            );
            HbotError::from(error)
        })
    }
}

/// 通过 `bots.json` 里的地址造真 OneBot 适配器（生产路径）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OneBotAdapterFactory;

impl AdapterFactory for OneBotAdapterFactory {
    fn create(&self, bot: &Bot, config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError> {
        if bot.platform != PLATFORM {
            return Err(HbotError::invalid(format!(
                "bot {} 的平台是 `{}`，不是 `{PLATFORM}`：不能用 OneBot 适配器启动",
                bot.id, bot.platform
            )));
        }
        let adapter = OneBotAdapter::from_bot_config(config)?;
        tracing::info!(
            bot = %bot.id,
            ws_url = adapter.ws_url(),
            "onebot 适配器已装配（不碰网络；连接在 subscribe 时建立：probe → handoff，单一读任务 + 有界事件通道）"
        );
        Ok(Arc::new(adapter))
    }
}

/// `BotConfig` → `OneBotConfig`（token 只走 `with_token`，不参与日志）。
///
/// `platform = onebot11` 的 `url` 由 `runtime::config::read_bot_config` 保证非空；
/// 这里对"缺地址"的兜底是**空串** —— 紧接着的 `validate_ws_url` 会立刻拒绝它
/// （`connect` 的第一行），不会拿空地址去连。之所以不在这里返回 `Result`：这个函数的
/// 唯一调用方 `from_bot_config` 前面刚做过同样的平台校验，多一层 `Result` 只是重复。
fn onebot_config(config: &BotConfig) -> OneBotConfig {
    let base = OneBotConfig::new(config.ws_url.clone().unwrap_or_default());
    match &config.access_token {
        Some(token) => base.with_token(token.clone()),
        None => base,
    }
}

/// 探测用的配置：把 `connect_timeout` 抬到不低于 [`PROBE_MIN_TIMEOUT`]。
///
/// `OneBotConfig::connect_timeout` 为 `None` 时 `connect_ws` **完全不加超时**，探测会
/// 无限挂住——那会把 `bot_start` 卡死，所以这里一律补一个值。
fn probe_config(config: &OneBotConfig) -> OneBotConfig {
    let timeout = config
        .connect_timeout
        .unwrap_or(PROBE_MIN_TIMEOUT)
        .max(PROBE_MIN_TIMEOUT);
    config.clone().with_connect_timeout(Some(timeout))
}

/// 启动前探一次连通性；失败时把原因翻成一句人话并记日志。
///
/// 与旧实现的关键区别：探测**不再把连接关掉**。`probe_session` 返回的
/// [`ConnectedSession`] 连同探到的配置一起交给 [`OneBotClient::start_with_session`]，
/// 成为常驻客户端的第一条会话——一次 `bot_start` 因此只对平台开一条 WS 连接，
/// 而探测的失败语义（连不上 / 握手失败 / 401 / 超时 → `bot_start` 当场报错）保持不变。
///
/// “OneBot 生命周期握手”的确认是**宽限式**的：`probe_session` 最多等
/// [`LIFECYCLE_CONFIRM_WINDOW`] 看对端有没有推 `meta_event/lifecycle` 或心跳。
/// 收到了就是强确认；窗口内一帧都没收到仍然算连接可用（有些协议端只在有事件时才说话，
/// 而心跳超时由常驻客户端的心跳看门狗负责），但会打一条 `warn` 留下证据。
async fn probe_startup(config: &OneBotConfig) -> Result<ConnectedSession, HbotError> {
    match probe_session(config).await {
        Ok(session) => {
            if session.lifecycle_seen() {
                tracing::debug!(
                    ws_url = %config.ws_url,
                    "onebot 启动前探测通过：已确认 OneBot 生命周期握手，连接将交给常驻客户端"
                );
            } else {
                tracing::warn!(
                    ws_url = %config.ws_url,
                    window_ms = u64::try_from(LIFECYCLE_CONFIRM_WINDOW.as_millis())
                        .unwrap_or(u64::MAX),
                    "onebot 启动前探测通过（宽限）：对端在确认窗口内没有推 meta_event/lifecycle，\
                     连接仍然交给常驻客户端；心跳超时后会按既有逻辑重连"
                );
            }
            Ok(session)
        }
        Err(error) => {
            let reason = describe_probe_failure(&error, config);
            // 日志与错误返回用**同一句**原因：用户在控制台点"启动"看到的那句话，
            // 必须能在日志里找到，否则排障又变成两边对照猜。
            tracing::error!(
                ws_url = %config.ws_url,
                error = %error,
                reason = %reason,
                "onebot 启动前探测失败：bot_start 将被拒绝"
            );
            Err(probe_error(&error, reason))
        }
    }
}

/// 把适配器的原始错误翻成"用户能照做"的一句话。
///
/// 分类依据只能是错误文本：`AdapterError` 的变体粒度到不了"DNS / 拒绝连接 / 证书 / 401"
/// 这一层（`Transport` 吃掉了所有非 TLS 的握手失败，HTTP 状态码只在 tungstenite 的
/// Display 里）。所以按子串分流，且**每条分支都给出可执行的下一步**，不只贴原文。
fn describe_probe_failure(error: &AdapterError, config: &OneBotConfig) -> String {
    let raw = error.to_string();
    let lower = raw.to_ascii_lowercase();
    let hint = if lower.contains("lookup address")
        || lower.contains("no such host")
        || lower.contains("failed to lookup")
        || lower.contains("dns")
    {
        "域名解析失败（DNS）：检查 bots.json 里 url 的主机名拼写、本机 DNS 与代理设置"
    } else if lower.contains("refused") || lower.contains("10061") {
        "连接被拒绝：该地址端口上没有 OneBot 服务端在监听（端口写错，或服务端没开正向 WebSocket）"
    } else if matches!(error, AdapterError::Tls { .. })
        || lower.contains("certificate")
        || lower.contains("unknown issuer")
    {
        "TLS/证书校验失败：证书不受信任或主机名不匹配（自签与私有 CA 用 extraRootCertificates \
         追加根证书；本项目没有跳过校验的开关）"
    } else if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        "认证失败（HTTP 401/403）：accessToken 与服务端配置不一致，或服务端要求 token 而这里没配"
    } else if matches!(error, AdapterError::Timeout { .. }) || lower.contains("timed out") {
        "连接超时：对端在超时内没有完成 TCP/TLS/WebSocket 握手（网络不通、被防火墙拦，或服务端卡住）"
    } else {
        "无法建立 WebSocket 连接"
    };
    let timeout_ms = config.connect_timeout.map(|timeout| timeout.as_millis());
    match timeout_ms {
        Some(limit) => format!(
            "{hint}（探测超时上限 {limit} ms）；地址 {}；原始错误：{raw}",
            config.ws_url
        ),
        None => format!("{hint}；地址 {}；原始错误：{raw}", config.ws_url),
    }
}

/// 探测失败 → wire 上的错误码。
///
/// 选择 **-32002 invalid**：证书错、token 错、地址写错、端口没开，全都是"用户的配置不对"
/// —— 能改的是 `bots.json`，不是程序。用 -32006 internal（`Transport`/`Tls` 的默认映射）会
/// 把用户往"程序有 bug"的方向带偏，那正是当前这条 warn 日志的毛病。
///
/// 例外是 `Blocked` / `Unsupported`：那是**策略层拒绝**（不支持的 scheme、被拦掉的地址），
/// 属于 -32005 denied 的语义（"这么做不允许"），保留原映射。
fn probe_error(error: &AdapterError, reason: String) -> HbotError {
    match error {
        AdapterError::Blocked { .. } | AdapterError::Unsupported { .. } => {
            HbotError::denied(reason)
        }
        _ => HbotError::invalid(reason),
    }
}

/// 出站目标：OneBot 11 只有"群"和"私聊"两种可发地址。
enum Address {
    Group(i64),
    Private(i64),
}

fn address_of(envelope: &OutboundEnvelope) -> Result<Address, HbotError> {
    let external_id = envelope.conversation.external_id.trim();
    match envelope.conversation.kind {
        ConversationKind::Group => external_id
            .parse::<i64>()
            .map(Address::Group)
            .map_err(|_| bad_address(external_id, "群号")),
        ConversationKind::Private => external_id
            .parse::<i64>()
            .map(Address::Private)
            .map_err(|_| bad_address(external_id, "QQ 号")),
        other => Err(HbotError::denied(format!(
            "onebot 11 没有向 {} 会话发送的接口（只支持 private / group）",
            crate::app::runtime::conversation_kind_str(other)
        ))),
    }
}

fn bad_address(external_id: &str, what: &str) -> HbotError {
    HbotError::invalid(format!(
        "onebot 会话地址 `{external_id}` 不是整数{what}：无法据此发送"
    ))
}

/// 唯一的事件读者：独占 `OneBotClient` 的可变所有权，把归一化事件转成 [`PlatformEvent`]。
///
/// 通道满了就在这里 await —— 这是**有意的背压**（§4.1：暂停读取，不丢消息）。
async fn read_events(
    client: &mut OneBotClient,
    tx: &mpsc::Sender<PlatformEvent>,
    self_id: Option<i64>,
) {
    while let Some(event) = client.next_event().await {
        if let Some(expected) = self_id
            && let Some(actual) = event.bot_platform_id
            && actual != expected
        {
            tracing::warn!(
                expected,
                actual,
                message = %event.external_id,
                "丢弃不属于本 bot 的 onebot 事件（bots.json 的 selfId 与服务端账号不一致？）"
            );
            continue;
        }

        let payload = event.payload_json();
        let platform_event = PlatformEvent::new(
            event.external_id.clone(),
            ConversationRef::new(event.conversation_external_id.clone(), event.kind),
            event.sender_id.clone(),
            payload,
        )
        .with_received_at(event.received_at);

        if tx.send(platform_event).await.is_err() {
            // 订阅者消失（适配器被换掉 / supervisor 停了）：退出去关连接。
            return;
        }
    }
    tracing::info!("onebot 事件流已结束（连接关闭）");
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(ws_url: &str) -> OneBotConfig {
        OneBotConfig::new(ws_url)
    }

    /// 四类"用户配错了"都必须翻成可照做的一句话，且都落在 -32002。
    ///
    /// 这些是**纯函数**级断言：真实网络抖动、本机有没有 DNS 都不该影响分类逻辑，
    /// 所以直接构造 `AdapterError`；端到端（真的连不上）由
    /// `tests/onebot_probe.rs` 覆盖。
    #[test]
    fn probe_failures_explain_themselves_and_map_to_invalid() {
        let cases = [
            (
                AdapterError::transport(
                    "ws connect failed: IO error: failed to lookup address information: \
                     No such host is known (os error 11001)",
                ),
                "DNS",
            ),
            (
                AdapterError::transport(
                    "ws connect failed: IO error: Connection refused (os error 10061)",
                ),
                "连接被拒绝",
            ),
            (
                AdapterError::tls("wss handshake failed: invalid peer certificate: UnknownIssuer"),
                "证书",
            ),
            (
                AdapterError::transport("ws connect failed: HTTP error: 401 Unauthorized"),
                "认证失败",
            ),
            (
                AdapterError::timeout("connect", Duration::from_secs(10)),
                "超时",
            ),
        ];
        for (error, want) in cases {
            let probe = config("ws://127.0.0.1:1");
            let reason = describe_probe_failure(&error, &probe);
            assert!(
                reason.contains(want),
                "原因里应出现「{want}」，实际：{reason}"
            );
            // 原因必须带地址与原始错误，否则用户拿着这句去搜还是无从下手。
            assert!(reason.contains("ws://127.0.0.1:1"), "缺少地址：{reason}");
            assert!(
                reason.contains(&error.to_string()),
                "缺少原始错误：{reason}"
            );
            assert_eq!(
                probe_error(&error, reason).code(),
                -32002,
                "配置类失败必须是 invalid（让用户去改配置，而不是怀疑程序）"
            );
        }
    }

    /// 策略层拒绝保留 -32005：那不是"配错了"，是"这么做不允许"。
    #[test]
    fn policy_rejections_stay_denied() {
        for error in [
            AdapterError::blocked(
                "unsupported websocket scheme in `http://x`: only ws:// and wss://",
            ),
            AdapterError::unsupported("event capacity must be > 0"),
        ] {
            let reason = describe_probe_failure(&error, &config("http://127.0.0.1:1"));
            assert_eq!(probe_error(&error, reason).code(), -32005);
        }
    }

    /// 探测超时**只上调不下调**：1 秒会把"慢但能连"误判成配置错误。
    #[test]
    fn probe_timeout_is_never_below_ten_seconds() {
        assert_eq!(PROBE_MIN_TIMEOUT, Duration::from_secs(10));

        // 显式配了 1 秒 → 抬到 10 秒（探测的职责不是快速失败）。
        let tight = probe_config(
            &config("ws://127.0.0.1:1").with_connect_timeout(Some(Duration::from_secs(1))),
        );
        assert_eq!(tight.connect_timeout, Some(PROBE_MIN_TIMEOUT));

        // 显式配了 30 秒 → 尊重更长的配置（跨地域/慢代理）。
        let loose = probe_config(
            &config("ws://127.0.0.1:1").with_connect_timeout(Some(Duration::from_secs(30))),
        );
        assert_eq!(loose.connect_timeout, Some(Duration::from_secs(30)));

        // 配了 None（`connect_ws` 里等于**完全不加超时**）→ 必须补上 10 秒，否则
        // `bot_start` 会永久挂住。
        let unlimited = probe_config(&config("ws://127.0.0.1:1").with_connect_timeout(None));
        assert_eq!(unlimited.connect_timeout, Some(PROBE_MIN_TIMEOUT));

        // 没配也走默认 10 秒。
        assert_eq!(
            probe_config(&config("ws://127.0.0.1:1")).connect_timeout,
            Some(PROBE_MIN_TIMEOUT)
        );
    }

    /// 探测挂在 `subscribe` 上，**不是** `create` 上：`create` 的契约仍是"只校验配置"。
    ///
    /// 这条钉住的是刻意的分层（`AdapterFactory` 是同步端口，塞不进 `await` 的网络探测）：
    /// 谁把探测挪进 `create`，`tests/bot_runtime.rs::wss_is_supported_by_the_onebot_adapter`
    /// 会立刻红 —— 那里断言 `create` 对一个没人监听的 `wss://` 地址返回 Ok。
    ///
    /// 要 `#[tokio::test]` 是因为 `create` 内部的 `OneBotClient::start` 会 `tokio::spawn`
    /// 读任务（真实调用点 `bot_start` 本来就在运行时里）。
    #[tokio::test]
    async fn create_still_only_validates_configuration() {
        let bot = hbot_store::Bot {
            id: hbot_core::BotId::from_uuid(uuid::Uuid::from_u128(0x1)),
            project_id: hbot_core::ProjectId::from_uuid(uuid::Uuid::from_u128(0x2)),
            name: "probe-boundary".to_owned(),
            platform: PLATFORM.to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 0,
        };
        // 没人监听的地址：`create` 仍然成功（不碰网络），探测留到 `subscribe`。
        let adapter = OneBotAdapterFactory
            .create(&bot, &BotConfig::new("ws://127.0.0.1:1"))
            .expect("create 只校验配置，不做连通性探测");
        assert_eq!(adapter.id(), PLATFORM);

        // 平台不对时仍然在配置期就拒绝（这条与探测无关，别因为加探测而丢掉）。
        let other = hbot_store::Bot {
            platform: "other".to_owned(),
            ..bot
        };
        let error = match OneBotAdapterFactory.create(&other, &BotConfig::new("ws://127.0.0.1:1")) {
            Err(error) => error,
            Ok(_) => panic!("平台不匹配必须被拒绝"),
        };
        assert_eq!(error.code(), -32002);
    }
}
