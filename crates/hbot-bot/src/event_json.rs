//! `onMessage(ctx)` 事件对象的唯一拼装点。
//!
//! 生产路径（`hbot-rpc` 的 `sandbox_handler`）与回放路径（`hbot-cli replay`）都从
//! 各自的来源填一份 [`EventSource`]，再调用 [`event_json`]。这样事件 JSON 的字段名
//! 与取值只有这一份实现，不会再出现「两边各自 `json!`、改一处忘另一处」。
//!
//! 对生成代码暴露的形状由 `hbot-code` 的 `HOST_DTS`（`hbot.d.ts` 的 `MessageCtx`）
//! 声明；跨 crate 的字段名一致性测试在 `crates/hbot-rpc/tests/event_shape_dts.rs`。
//! 文档口径见 `docs/02-architecture.md` §6.3。

use hbot_core::{BotId, ConversationId, ConversationKind, MessageId, Timestamp};

use crate::event::kind_str;

/// 拼装一条入站事件所需的全部字段（借用视图，不拥有载荷）。
///
/// 生产侧由 [`MessageCtx`](crate::MessageCtx) 填，回放侧由回放行
/// （`InboundReplayRow`）与解析后的 payload 填。字段名是内部契约；对 JS 暴露的
/// 名字由 [`event_json`] 固定。
#[derive(Clone, Copy, Debug)]
pub struct EventSource<'a> {
    /// 所属 bot。
    pub bot_id: BotId,
    /// 本地会话 id（`conversations.id`）。
    pub conversation_id: ConversationId,
    /// 消息 id（`inbound.id`，也是生成代码拿到的幂等键 `ctx.messageId`）。
    pub message_id: MessageId,
    /// 平台侧事件 id（去重键）。
    pub external_id: &'a str,
    /// 发送者平台 id；平台未提供时为空串。
    pub sender_id: &'a str,
    /// 归一化纯文本；`None` 表示 payload 里没有 `text` 字段（输出空串，见 [`event_json`]）。
    pub text: Option<&'a str>,
    /// 会话类型。
    pub kind: ConversationKind,
    /// 平台侧会话 id。
    pub conversation_external_id: &'a str,
    /// 收到时间。
    pub received_at: Timestamp,
    /// 归一化后的原始载荷。
    pub payload: &'a serde_json::Value,
}

/// 把 [`EventSource`] 拼成 `onMessage(ctx)` 收到的事件对象。
///
/// **这是唯一实现**：生产（`hbot-rpc` 的 `sandbox_handler`）与回放（`hbot-cli
/// replay`）都调用它。字段名必须与 `hbot-code/src/dts.rs` 的 `HOST_DTS` 里
/// `MessageCtx` 的声明一致（`botId` / `conversationId` / `messageId` / `externalId` /
/// `senderId` / `text` / `kind` / `conversationExternalId` / `receivedAt` /
/// `payload`）。
///
/// `text` 的缺失值是**空串**而不是 `null`：`hbot.d.ts` 声明 `text: string`，
/// 生成代码可以直接写 `ctx.text.length`（`docs/02-architecture.md` §6.3）。
#[must_use]
pub fn event_json(source: &EventSource<'_>) -> serde_json::Value {
    serde_json::json!({
        "botId": source.bot_id.to_string(),
        "conversationId": source.conversation_id.to_string(),
        "messageId": source.message_id.to_string(),
        "externalId": source.external_id,
        "senderId": source.sender_id,
        "text": source.text.unwrap_or_default(),
        "kind": kind_str(source.kind),
        "conversationExternalId": source.conversation_external_id,
        "receivedAt": source.received_at.unix_millis(),
        "payload": source.payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn source<'a>(text: Option<&'a str>, payload: &'a serde_json::Value) -> EventSource<'a> {
        EventSource {
            bot_id: BotId::from_uuid(Uuid::nil()),
            conversation_id: ConversationId::from_uuid(Uuid::nil()),
            message_id: MessageId::from_uuid(Uuid::nil()),
            external_id: "evt-1",
            sender_id: "u-1",
            text,
            kind: ConversationKind::Group,
            conversation_external_id: "group-42",
            received_at: Timestamp::from_unix_millis(1_700_000_000_123),
            payload,
        }
    }

    #[test]
    fn maps_every_field_to_its_documented_key() {
        let payload = json!({"text": "hi", "raw": true});
        let event = event_json(&source(Some("hi"), &payload));

        assert_eq!(
            event,
            json!({
                "botId": Uuid::nil().to_string(),
                "conversationId": Uuid::nil().to_string(),
                "messageId": Uuid::nil().to_string(),
                "externalId": "evt-1",
                "senderId": "u-1",
                "text": "hi",
                "kind": "group",
                "conversationExternalId": "group-42",
                "receivedAt": 1_700_000_000_123_i64,
                "payload": payload,
            })
        );
    }

    #[test]
    fn missing_text_is_an_empty_string_and_the_key_is_present() {
        let payload = json!({"image": "https://example.test/a.png"});
        let event = event_json(&source(None, &payload));

        assert_eq!(event.get("text"), Some(&json!("")));
    }
}
