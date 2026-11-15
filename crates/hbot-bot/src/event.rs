//! 适配器归一化后的入站事件。
//!
//! 平台协议（OneBot 11 / Webhook / Telegram…）的解析不属于本 crate：适配器把平台事件
//! 归一化成 [`PlatformEvent`] 后交给 supervisor，去重、排序、持久化由本 crate 统一做
//! （`docs/02-architecture.md` §4.2）。因此这里只有平台无关字段，没有原始 JSON 结构。

use hbot_core::{ConversationKind, Timestamp};

/// 一个平台会话的引用：适配器侧标识 + 本地展示信息。
///
/// `external_id` 与 `bot_id` 一起构成 `conversations` 的唯一键；本地
/// [`hbot_core::ConversationId`] 由 `(bot_id, external_id)` 确定性派生
/// （见 [`crate::conversation_id_for`]），所以这里不携带本地 ID。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationRef {
    /// 平台侧会话标识（群号、用户 id、频道 id…），用于去重与发送定位。
    pub external_id: String,
    /// 会话类型；平台没给时用 [`ConversationKind::Unknown`]。
    pub kind: ConversationKind,
    /// 展示名；平台没给时为 `None`。
    pub title: Option<String>,
}

impl ConversationRef {
    /// 构造一个会话引用（无标题）。
    pub fn new(external_id: impl Into<String>, kind: ConversationKind) -> Self {
        Self {
            external_id: external_id.into(),
            kind,
            title: None,
        }
    }

    /// 带上展示名。
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// 落库用的稳定字符串（与 `maps` 无关，直接对应 `conversations.kind`）。
    pub(crate) fn kind_str(&self) -> &'static str {
        kind_str(self.kind)
    }
}

/// [`ConversationKind`] 的落库字符串。
pub(crate) fn kind_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Private => "private",
        ConversationKind::Group => "group",
        ConversationKind::Channel => "channel",
        ConversationKind::Unknown => "unknown",
    }
}

/// 适配器归一化后的一条入站消息。
///
/// 不变量（`AGENTS.md` §4.1）：
///
/// * `external_id` 是平台事件 ID，`(bot_id, external_id)` 唯一 → 重复投递只执行一次；
/// * supervisor 先落 `inbound` 行再入队，因此本结构在落库前只存在于内存一瞬。
#[derive(Clone, Debug)]
pub struct PlatformEvent {
    /// 平台事件 ID（`inbound.external_id`），去重键的一半。
    pub external_id: String,
    /// 所属会话。
    pub conversation: ConversationRef,
    /// 发送者平台 ID（仅用于审计与生成代码上下文，不参与去重）。
    pub sender_id: String,
    /// 归一化后的事件载荷（不透明 JSON，交给生成代码解释）。
    pub payload: serde_json::Value,
    /// 平台侧收到时间；`None` 时由 supervisor 用注入的时钟补当前时间。
    pub received_at: Option<Timestamp>,
}

impl PlatformEvent {
    /// 构造一条事件（时间留空，由 supervisor 补时钟）。
    pub fn new(
        external_id: impl Into<String>,
        conversation: ConversationRef,
        sender_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            external_id: external_id.into(),
            conversation,
            sender_id: sender_id.into(),
            payload,
            received_at: None,
        }
    }

    /// 指定平台侧收到时间。
    pub fn with_received_at(mut self, received_at: Timestamp) -> Self {
        self.received_at = Some(received_at);
        self
    }
}

#[cfg(test)]
mod tests {
    use hbot_core::ConversationKind;

    use super::{ConversationRef, PlatformEvent, kind_str};

    #[test]
    fn kind_strings_match_schema_expectations() {
        assert_eq!(kind_str(ConversationKind::Private), "private");
        assert_eq!(kind_str(ConversationKind::Group), "group");
        assert_eq!(kind_str(ConversationKind::Channel), "channel");
        assert_eq!(kind_str(ConversationKind::Unknown), "unknown");
        assert_eq!(
            ConversationRef::new("g1", ConversationKind::Group).kind_str(),
            "group"
        );
    }

    #[test]
    fn event_builder_keeps_fields() {
        let event = PlatformEvent::new(
            "evt-1",
            ConversationRef::new("group-42", ConversationKind::Group).with_title("demo"),
            "user-7",
            serde_json::json!({ "text": "hi" }),
        );
        assert_eq!(event.external_id, "evt-1");
        assert_eq!(event.conversation.external_id, "group-42");
        assert_eq!(event.conversation.title.as_deref(), Some("demo"));
        assert_eq!(event.sender_id, "user-7");
        assert_eq!(event.payload["text"], "hi");
        assert!(event.received_at.is_none());
    }
}
