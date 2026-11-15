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
//! connect(): OneBotClient::start ──► tokio::spawn(reader) ──► mpsc::Sender<PlatformEvent>
//! subscribe(): 交出那个 mpsc::Receiver（一次性）
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
//! * `probe_connection()` 目前**没有**在这里接线：所以控制台里证书错表现为"反复重连 +
//!   `warn` 日志"，而不是一条当场报出来的错误。要接的话，在 `OneBotAdapter::connect`
//!   之前调一次探测（`hbot_adapters::platforms::onebot::probe_connection`）。
//! * 被动回复：`OutboundEnvelope.reply_to` 带着触发这条回复的平台消息 ID
//!   （`AGENTS.md` §4.1），这里原样转给 `send_group_msg` / `send_private_msg`，
//!   由 `hbot_adapters::platforms::onebot::message` 组 `reply` 消息段。
//!   `None` = 主动消息，**不**伪造引用段。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use hbot_adapters::platforms::onebot::{OneBotClient, OneBotConfig, validate_ws_url};
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

/// 一 个 bot 的 OneBot 适配器：一个读者任务 + 一个 action 句柄。
pub struct OneBotAdapter {
    /// 出站：`Clone` 的 action 句柄，内部是有界 channel。
    actions: hbot_adapters::platforms::onebot::ActionHandle,
    /// 地址，只用于诊断日志（token 不在这里，绝不打日志）。
    ws_url: String,
    /// 入站事件的一次性交付口：`subscribe` 取走后为 `None`。
    subscriber: Mutex<Option<mpsc::Receiver<PlatformEvent>>>,
    /// 读者任务的句柄；`Drop` 时 abort（client 在任务里，随之关闭连接）。
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl OneBotAdapter {
    /// 连上一个 OneBot 11 正向 WS 服务端，并启动唯一的事件读者任务。
    ///
    /// `self_id` 给了就做一次乐观校验：事件里带了 `self_id` 且不相等时丢弃并 warn
    /// （一个 ws 端点上挂错号是最常见的配置错误）。
    ///
    /// # Errors
    ///
    /// 地址非法（包括 `wss://`）、容量为 0——直接来自 [`OneBotClient::start`]，
    /// 经 `From<AdapterError> for HbotError` 映射：`Unsupported` → -32005。
    pub fn connect(config: OneBotConfig, self_id: Option<i64>) -> Result<Self, HbotError> {
        // 先校验，再起任何任务：地址不合法时不要留下半个连接。
        validate_ws_url(&config.ws_url).map_err(HbotError::from)?;
        let ws_url = config.ws_url.clone();

        let mut client = OneBotClient::start(config).map_err(HbotError::from)?;
        let actions = client.action_handle();

        let (tx, rx) = mpsc::channel::<PlatformEvent>(ADAPTER_EVENT_CAPACITY);
        let reader = tokio::spawn(async move {
            read_events(&mut client, &tx, self_id).await;
            // 事件流结束（连接被关闭 / 订阅者消失）：显式关闭客户端，让重连任务收尾。
            client.shutdown().await;
        });

        Ok(Self {
            actions,
            ws_url,
            subscriber: Mutex::new(Some(rx)),
            reader: Mutex::new(Some(reader)),
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
        let receiver = lock(&self.subscriber).take().ok_or_else(|| {
            HbotError::conflict("onebot 适配器只能订阅一次：事件读者任务已经把流交给了上一个订阅者")
        })?;
        Ok(AdapterSubscription::new(receiver))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        let address = address_of(&envelope)?;
        // 被动回复要绑定到触发它的那条平台消息（§4.1）；`None` 是主动消息。
        let reply_to = envelope.reply_to.as_deref();
        let sent = match address {
            Address::Group(group_id) => {
                self.actions
                    .send_group_msg(group_id, &envelope.text, reply_to)
                    .await
            }
            Address::Private(user_id) => {
                self.actions
                    .send_private_msg(user_id, &envelope.text, reply_to)
                    .await
            }
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
            "onebot 适配器已连接（单一读任务 + 有界事件通道）"
        );
        Ok(Arc::new(adapter))
    }
}

/// `BotConfig` → `OneBotConfig`（token 只走 `with_token`，不参与日志）。
fn onebot_config(config: &BotConfig) -> OneBotConfig {
    let base = OneBotConfig::new(config.ws_url.clone());
    match &config.access_token {
        Some(token) => base.with_token(token.clone()),
        None => base,
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
