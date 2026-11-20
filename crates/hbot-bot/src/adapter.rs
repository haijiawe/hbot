//! 平台适配器抽象（本 crate 的**边界**）。
//!
//! `hbot-adapters` 目前还没有 `PlatformAdapter` 实现（`docs/03-directory.md`：平台协议归
//! `hbot-adapters`）。本批先在 `hbot-bot` 内定义 supervisor 需要的最小契约，好处是
//! supervisor/队列/outbox 都能被 fake 适配器完整测试；下一批接真实 OneBot 时实现的仍是
//! **这个** trait，supervisor 不需要改。
//!
//! 顺序约束（`AGENTS.md` §7.1：旧项目先连平台、后建订阅导致丢消息）：订阅必须在
//! `connect` 之前、由 supervisor 在队列就绪后建立，见 [`crate::supervisor`]。

use async_trait::async_trait;
use hbot_core::{AttachmentRef, BotId, ConversationId, HbotError, OutboundId};
use tokio::sync::mpsc;

use crate::event::{ConversationRef, PlatformEvent};

/// 交给适配器发送的一条出站消息。
///
/// 由 [`crate::outbox`] 组装：`id` 是 `outbound.id`，也是发送侧的幂等参考；`payload`
/// 是领域侧的纯文本（这一批只有文本，平台富媒体由适配器后续翻译）。
#[derive(Clone, Debug)]
pub struct OutboundEnvelope {
    /// 出站行主键。
    pub id: OutboundId,
    /// 所属 bot。
    pub bot_id: BotId,
    /// 目标会话本地 ID（日志/指标用）。
    pub conversation_id: ConversationId,
    /// 目标会话（平台外部 ID + 类型），适配器据此定位发送目标。
    pub conversation: ConversationRef,
    /// 待发送文本。
    pub text: String,
    /// 被动回复要引用的平台消息 ID（`None` = 主动消息）。
    ///
    /// 适配器按平台约定把它翻译成“引用段/回复段”：OneBot 走 `reply` 消息段
    /// （`hbot_adapters::platforms::onebot::message::build_outbound`），其它平台按各自协议。
    /// **不要在这里重新生成 ID，也不要复用无关的旧事件 ID**（`AGENTS.md` §4.1）。
    pub reply_to: Option<String>,
    /// 附件（`None` = 纯文本）。
    ///
    /// 适配器负责把它变成一个平台媒体段：路径已在入队前由宿主解析成绝对路径并核验
    /// 过（在项目根内、文件存在、大小不超限），**适配器不要重新按用户输入解析路径**。
    /// 读文件失败要返回 `Err`（交给 outbox 重试 / 死信），不要静默降级成「只发正文」。
    pub attachment: Option<AttachmentRef>,
    /// 本条消息已尝试次数（含本次）；用于适配器侧日志与死信判定。
    pub attempts: u32,
}

/// 订阅句柄：适配器建立的入站事件流。
///
/// 用有界通道：适配器写不进去时必须自己施加背压（`docs/02-architecture.md` §10），
/// 不得静默丢弃。
pub struct AdapterSubscription {
    /// 归一化后的入站事件流。
    pub events: mpsc::Receiver<PlatformEvent>,
}

impl AdapterSubscription {
    /// 用已有接收端包装。
    pub fn new(events: mpsc::Receiver<PlatformEvent>) -> Self {
        Self { events }
    }

    /// 建一个 `capacity` 大小的通道，返回（发送端交给适配器，订阅句柄交给 supervisor）。
    pub fn channel(capacity: usize) -> (mpsc::Sender<PlatformEvent>, Self) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (tx, Self { events: rx })
    }
}

/// 平台适配器：一个 bot 一个实例，订阅入站、发送出站。
#[async_trait]
pub trait BotAdapter: Send + Sync + 'static {
    /// 平台标识（`onebot11` / `webhook` / `telegram`…），用于日志与 `bots.platform` 校验。
    fn id(&self) -> &'static str;

    /// 启动时需要预先补建会话行的会话（例如已配置的群）。
    ///
    /// 这些会话在订阅之前就写入 `conversations`，保证第一波入站事件不需要临时建行
    /// （`ensure_conversation` 仍然兜底，见 [`crate::dedupe`]）。默认空。
    fn declared_conversations(&self) -> Vec<ConversationRef> {
        Vec::new()
    }

    /// 建立订阅：返回归一化事件流。
    ///
    /// 必须在 `conversations` 行就绪**之后**调用（supervisor 的启动顺序）。
    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError>;

    /// 发送一条出站消息；返回 `Err` 由 outbox 决定重试/死信。
    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError>;
}
