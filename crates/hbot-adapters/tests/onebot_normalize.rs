//! 归一化集成测试：用真实 OneBot 11 JSON 形状（含各家方言）验证
//! 会话类型、发送者、文本渲染与幂等键。

use std::path::PathBuf;

use serde_json::Value;

use hbot_adapters::platforms::onebot::{MessageType, OneBotEvent, fallback_external_id, normalize};
use hbot_core::domain::{ConversationKind, Timestamp};
use hbot_core::ids::{BotId, ConversationId, MessageId};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} unreadable: {err}", path.display()));
    serde_json::from_str(&text).expect("fixture must be valid json")
}

fn parse(value: Value) -> OneBotEvent {
    serde_json::from_value(value).expect("fixture must parse as OneBotEvent")
}

fn normalized(
    name: &str,
) -> (
    OneBotEvent,
    hbot_adapters::platforms::onebot::NormalizedInbound,
) {
    let raw = fixture(name);
    let event = parse(raw.clone());
    let normalized = normalize(&event, raw, Timestamp::from_unix_millis(1_700_000_000_000))
        .expect("message event must normalize");
    (event, normalized)
}

#[test]
fn group_message_normalizes_conversation_and_text() {
    let (_event, message) = normalized("onebot_group_message.json");
    assert_eq!(message.kind, ConversationKind::Group);
    assert_eq!(message.conversation_external_id, "30001");
    assert_eq!(message.sender_id, "20001");
    // `at` 段渲染成 `@qq`（有损：拿不到昵称，也不保留 at 结构）。
    assert_eq!(message.text, "大家好，我是 @20002 的朋友");
    assert_eq!(message.external_id, "1234");
    assert_eq!(message.platform_message_id, Some(1234));
    assert_eq!(message.bot_platform_id, Some(10000));
    assert_eq!(message.reply_to, None);
}

#[test]
fn private_message_uses_sender_as_conversation() {
    let (_event, message) = normalized("onebot_private_message.json");
    assert_eq!(message.kind, ConversationKind::Private);
    assert_eq!(message.conversation_external_id, "20002");
    assert_eq!(message.sender_id, "20002");
    assert_eq!(message.text, "私聊一句");
    assert_eq!(message.external_id, "5678");
}

#[test]
fn at_image_face_and_reply_segments_render_lossily() {
    let (event, message) = normalized("onebot_at_segment.json");
    // 段数组 → 纯文本：at → `@qq`，image → `[图片]`，face → `[表情]`，reply 不渲染进正文。
    assert_eq!(message.text, "@10000 在吗[图片][表情]");
    // reply 段单独抽出来，供被动回复回填 `reply_to`。
    assert_eq!(message.reply_to.as_deref(), Some("9000"));
    assert_eq!(event.reply_to().as_deref(), Some("9000"));
    assert_eq!(message.kind, ConversationKind::Group);
}

#[test]
fn missing_message_id_uses_stable_fallback_key() {
    let raw = fixture("onebot_missing_message_id.json");
    let event = parse(raw.clone());
    let first = normalize(&event, raw.clone(), Timestamp::from_unix_millis(1))
        .expect("normalizes without message_id");
    let second = normalize(&event, raw, Timestamp::from_unix_millis(2))
        .expect("normalizes without message_id");

    assert_eq!(first.platform_message_id, None);
    // 同一事件重放（不同接收时间）→ 幂等键必须一致，否则去重失效。
    assert_eq!(first.external_id, second.external_id);
    // 键的形状与文档一致：`{user_id}-{time}-{fnv1a64:016x}`。
    assert_eq!(
        first.external_id,
        fallback_external_id(20004, 1731653100, "这条没有 message_id")
    );
    assert_eq!(first.external_id.matches('-').count(), 2);

    // 不同用户 / 不同秒 / 不同文本 → 不同键（不相等冲突）。
    let other_user = fallback_external_id(20005, 1731653100, "这条没有 message_id");
    let other_time = fallback_external_id(20004, 1731653101, "这条没有 message_id");
    let other_text = fallback_external_id(20004, 1731653100, "另一条消息");
    for other in [other_user, other_time, other_text] {
        assert_ne!(first.external_id, other);
    }
}

#[test]
fn string_user_id_and_self_id_are_tolerated() {
    let raw = fixture("onebot_user_id_string.json");
    let event = parse(raw.clone());
    assert_eq!(event.user_id, 20005);
    assert_eq!(event.self_id, Some(10000));
    assert_eq!(event.group_id, Some(30003));

    let message = normalize(&event, raw, Timestamp::from_unix_millis(1)).expect("normalizes");
    assert_eq!(message.sender_id, "20005");
    assert_eq!(message.kind, ConversationKind::Private);
    assert_eq!(message.external_id, "3456");
}

#[test]
fn plain_string_message_body_is_accepted() {
    let (_event, message) = normalized("onebot_message_string.json");
    assert_eq!(message.text, "纯字符串消息体");
    assert_eq!(message.kind, ConversationKind::Group);
}

#[test]
fn non_message_events_are_rejected_by_normalize() {
    let raw = serde_json::json!({
        "post_type": "notice",
        "notice_type": "group_upload",
        "time": 1731653400,
        "self_id": 10000,
        "user_id": 20007,
        "group_id": 30005
    });
    let event = parse(raw.clone());
    let error = normalize(&event, raw, Timestamp::from_unix_millis(1))
        .expect_err("notice must not normalize as inbound message");
    assert!(error.to_string().contains("not a message event"), "{error}");
}

#[test]
fn group_message_without_group_id_is_rejected() {
    let raw = serde_json::json!({
        "post_type": "message",
        "message_type": "group",
        "time": 1731653500,
        "user_id": 20008,
        "message_id": 111,
        "message": "没有群号"
    });
    let event = parse(raw.clone());
    assert_eq!(event.message_type, Some(MessageType::Group));
    let error = normalize(&event, raw, Timestamp::from_unix_millis(1))
        .expect_err("group message without group_id must be rejected");
    assert!(error.to_string().contains("group_id"), "{error}");
}

#[test]
fn payload_json_and_inbound_carry_architecture_fields() {
    let (_event, message) = normalized("onebot_group_message.json");
    let payload = message.payload_json();
    assert_eq!(payload["text"], "大家好，我是 @20002 的朋友");
    assert_eq!(payload["senderId"], "20001");
    assert_eq!(payload["conversationExternalId"], "30001");
    assert_eq!(payload["kind"], "group");
    assert_eq!(payload["messageId"], 1234);
    assert!(payload["raw"].is_object());

    let message_id: MessageId = "00000000-0000-0000-0000-000000000001"
        .parse()
        .expect("uuid parses as MessageId");
    let bot_id: BotId = "00000000-0000-0000-0000-000000000002"
        .parse()
        .expect("uuid parses as BotId");
    let conversation_id: ConversationId = "00000000-0000-0000-0000-000000000003"
        .parse()
        .expect("uuid parses as ConversationId");
    let inbound = message.to_inbound(message_id, bot_id, conversation_id);
    assert_eq!(inbound.external_id, "1234");
    assert_eq!(inbound.sender_id, "20001");
    assert_eq!(
        inbound.received_at,
        Timestamp::from_unix_millis(1_700_000_000_000)
    );
    assert!(inbound.processed_at.is_none());
    assert_eq!(inbound.payload["kind"], "group");
}
