//! 消息处理器抽象。
//!
//! supervisor / 会话队列**不认识**具体执行者：这一批还没有接 QuickJS（见
//! `docs/04-implementation-plan.md` M1），所以这里只定义一个平台无关的
//! [`HandlerExecutor`]，由组合根注入（之后是 `hbot-code` 的 isolate 调度器，
//! 测试里是 fake）。
//!
//! 约束（`AGENTS.md` §0.6 / §4.1）：
//!
//! * 同会话严格串行由 [`crate::queue`] 保证，实现方**不需要**自己加锁；
//! * 不同会话可并发，所以实现必须是 `Send + Sync`；
//! * 幂等键是 [`MessageCtx::message_id`]（`inbound.id`），重放时不变；
//! * 返回 `Err` 视为 handler 崩溃：不标记 `processed_at`，并计入连续失败，
//!   连续 3 次进入 `Degraded`（`docs/02-architecture.md` §12）。

use async_trait::async_trait;
use hbot_core::{
    BotId, ConversationId, ConversationKind, HbotError, InboundMessage, MessageId, Timestamp,
};

use crate::event::ConversationRef;

/// 一条入站消息的执行上下文。
///
/// 只带 handler 真正需要的东西：平台无关，不含平台原始事件（原始载荷在
/// [`Self::payload`]，由适配器归一化）。`Clone` 是给队列内部的重试/背压循环用的。
#[derive(Clone, Debug)]
pub struct MessageCtx {
    /// 所属 bot。
    pub bot_id: BotId,
    /// 入站行（含幂等键与载荷）。
    pub message: InboundMessage,
    /// 会话引用（平台外部 ID + 类型），出站回复要靠它定位目标。
    pub conversation: ConversationRef,
}

impl MessageCtx {
    /// 组装上下文。
    pub fn new(bot_id: BotId, message: InboundMessage, conversation: ConversationRef) -> Self {
        Self {
            bot_id,
            message,
            conversation,
        }
    }

    /// 幂等键（`inbound.id`）；同一事件重复投递时取值不变。
    pub fn message_id(&self) -> MessageId {
        self.message.id
    }

    /// 本消息所属会话的本地 ID。
    pub fn conversation_id(&self) -> ConversationId {
        self.message.conversation_id
    }

    /// 会话类型（private / group / channel）。
    pub fn conversation_kind(&self) -> ConversationKind {
        self.conversation.kind
    }

    /// 平台侧事件 ID（`inbound.external_id`）。
    pub fn external_id(&self) -> &str {
        &self.message.external_id
    }

    /// 收到时间（平台给了就用平台的）。
    pub fn received_at(&self) -> Timestamp {
        self.message.received_at
    }

    /// 归一化载荷。
    pub fn payload(&self) -> &serde_json::Value {
        &self.message.payload
    }

    /// 常见归一化形状 `{"text": "..."}` 的便捷读法。
    ///
    /// 只做便利访问：适配器没归一化 `text` 字段时返回 `None`，不猜编码格式。
    pub fn text(&self) -> Option<&str> {
        self.message
            .payload
            .get("text")
            .and_then(|value| value.as_str())
    }
}

/// 消息执行器：一次处理一条入站消息。
///
/// 实现方只需实现 `on_message`；会话串行、幂等、背压都由本 crate 提供。
#[async_trait]
pub trait HandlerExecutor: Send + Sync + 'static {
    /// 处理一条消息。
    ///
    /// 返回 `Err` 表示这次执行失败：不会标记 `processed_at`，并且计入 bot 的连续
    /// 崩溃计数。取消/超时由实现方自己映射成 [`HbotError`]。
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError>;
}

#[cfg(test)]
mod tests {
    use hbot_core::{
        BotId, ConversationId, ConversationKind, InboundMessage, MessageId, Timestamp,
    };
    use uuid::Uuid;

    use super::{ConversationRef, MessageCtx};

    fn ctx(payload: serde_json::Value) -> MessageCtx {
        let message = InboundMessage {
            id: MessageId::from_uuid(Uuid::from_u128(7)),
            bot_id: BotId::from_uuid(Uuid::from_u128(2)),
            conversation_id: ConversationId::from_uuid(Uuid::from_u128(3)),
            external_id: "evt-1".to_owned(),
            sender_id: "user-7".to_owned(),
            payload,
            received_at: Timestamp::from_unix_millis(1_700_000_000_000),
            processed_at: None,
        };
        MessageCtx::new(
            BotId::from_uuid(Uuid::from_u128(2)),
            message,
            ConversationRef::new("group-42", ConversationKind::Group),
        )
    }

    #[test]
    fn accessors_read_the_message() {
        let ctx = ctx(serde_json::json!({ "text": "hello" }));
        assert_eq!(ctx.text(), Some("hello"));
        assert_eq!(ctx.external_id(), "evt-1");
        assert_eq!(ctx.conversation_kind(), ConversationKind::Group);
        assert_eq!(ctx.received_at().unix_millis(), 1_700_000_000_000);
        assert_eq!(ctx.conversation.external_id, "group-42");
        assert_eq!(ctx.payload()["text"], "hello");
    }

    #[test]
    fn text_is_none_when_payload_is_not_text() {
        let ctx = ctx(serde_json::json!({ "image": "x.png" }));
        assert_eq!(ctx.text(), None);
    }
}
