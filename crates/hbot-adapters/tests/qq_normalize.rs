//! 归一化（QQ 官方事件 → 共用 `NormalizedInbound`）的测试。
//!
//! 这里是「协议形状」的钉子：字段名、会话命名空间（openid ≠ QQ 号）、@ 标记剥离、
//! `auth_token` 脱敏、时间戳偏移解析。不需要网络。

use serde_json::{Value, json};

use hbot_adapters::qq::{GatewayFrame, is_bot_mentioned, normalize, strip_at_mentions};
use hbot_core::domain::{ConversationKind, Timestamp};

const GROUP_OPENID: &str = "196AAC0A1E7A06AC0D5EEB7FCF778303";
const MEMBER_OPENID: &str = "336390D2FCA8472A739B0B1B2B6A3A90";
const USER_OPENID: &str = "9A8B7C6D5E4F30211203F4E5D6C7B8A9";
const MESSAGE_ID: &str = "ROBOT1.0_mPc6ExampleMessageId-!_";
const AUTH_TOKEN_SECRET: &str = "SECRET-AUTH-TOKEN";
const SENTINEL: i64 = 1_700_000_000_000;

fn frame_of(payload: Value) -> GatewayFrame {
    GatewayFrame::parse(&payload.to_string()).expect("frame must parse")
}

fn group_payload(event_name: &str) -> Value {
    json!({
        "op": 0,
        "s": 42,
        "t": event_name,
        "id": "EVENT-ID",
        "d": {
            "id": MESSAGE_ID,
            "content": format!("<@{GROUP_OPENID}> 你好"),
            "group_id": GROUP_OPENID,
            "group_openid": GROUP_OPENID,
            "message_type": 0,
            "timestamp": "2026-11-16T08:06:39+08:00",
            "author": {
                "id": "10240001",
                "member_openid": MEMBER_OPENID,
                "member_nickname": "测试",
                "member_role": "member",
                "union_openid": "",
                "username": "",
                "bot": false
            },
            "mentions": [{
                "bot": true,
                "id": "10240001",
                "is_you": true,
                "member_openid": GROUP_OPENID,
                "member_role": "member",
                "scope": "single"
            }],
            "message_scene": {
                "ext": ["msg_idx=REFIDX_EXAMPLE", format!("auth_token={AUTH_TOKEN_SECRET}")],
                "source": "default"
            }
        }
    })
}

#[test]
fn group_messages_use_openids_and_the_official_message_id() {
    let frame = frame_of(group_payload("GROUP_MESSAGE_CREATE"));
    let message = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect("group message must normalize")
        .expect("a group message is an inbound message");

    assert_eq!(message.external_id, MESSAGE_ID);
    assert_eq!(message.reply_to.as_deref(), Some(MESSAGE_ID));
    assert_eq!(message.conversation_external_id, GROUP_OPENID);
    // 会话与发送者都是 openid，不是 QQ 号。
    assert_eq!(message.sender_id, MEMBER_OPENID);
    assert_ne!(message.sender_id, "10240001");
    assert_eq!(message.kind, ConversationKind::Group);
    assert_eq!(message.text, "你好");
    assert_eq!(message.bot_platform_id, None);
    assert_eq!(message.platform_message_id, None);
    assert_eq!(message.received_at.unix_millis(), 1_794_787_599_000);
}

#[test]
fn the_legacy_group_event_name_is_accepted_too() {
    // 官方文档写 `GROUP_AT_MESSAGE_CREATE`，真网关实测发的是 `GROUP_MESSAGE_CREATE`；
    // 两个都认，避免网关哪天换回文档名就丢消息。
    let frame = frame_of(group_payload("GROUP_AT_MESSAGE_CREATE"));
    let message = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect("legacy group message must normalize")
        .expect("a group message is an inbound message");
    assert_eq!(message.kind, ConversationKind::Group);
    assert_eq!(message.conversation_external_id, GROUP_OPENID);
    assert_eq!(message.text, "你好");
}

#[test]
fn c2c_messages_map_to_the_users_openid_conversation() {
    let payload = json!({
        "op": 0,
        "s": 7,
        "t": "C2C_MESSAGE_CREATE",
        "d": {
            "id": MESSAGE_ID,
            "content": "私聊内容",
            "timestamp": "2026-11-16T08:06:39+08:00",
            "author": {
                "id": "10240001",
                "union_openid": "",
                "user_openid": USER_OPENID,
                "username": "",
                "bot": false
            }
        }
    });
    let message = normalize(&frame_of(payload), Timestamp::from_unix_millis(SENTINEL))
        .expect("c2c message must normalize")
        .expect("a c2c message is an inbound message");

    assert_eq!(message.kind, ConversationKind::Private);
    assert_eq!(message.conversation_external_id, USER_OPENID);
    assert_eq!(message.sender_id, USER_OPENID);
    assert_eq!(message.text, "私聊内容");
    assert_eq!(message.reply_to.as_deref(), Some(MESSAGE_ID));
}

#[test]
fn channel_at_messages_are_marked_as_channels() {
    // **未核实**：频道事件的真实字段形状还没在真网关上验过。这里按
    // 「有 `channel_id` 就当频道会话」的保守口径钉住，发送侧对频道一律 Unsupported。
    let payload = json!({
        "op": 0,
        "s": 9,
        "t": "AT_MESSAGE_CREATE",
        "d": {
            "id": MESSAGE_ID,
            "content": "<@!9> 频道消息",
            "channel_id": "CHANNEL-1",
            "timestamp": "2026-11-16T08:06:39+08:00",
            "author": { "id": "USER-1" }
        }
    });
    let message = normalize(&frame_of(payload), Timestamp::from_unix_millis(SENTINEL))
        .expect("channel message must normalize")
        .expect("a channel message is an inbound message");

    assert_eq!(message.kind, ConversationKind::Channel);
    assert_eq!(message.conversation_external_id, "CHANNEL-1");
    assert_eq!(message.sender_id, "USER-1");
    // `<@!9>` 这种数字形式的提及也要剥掉。
    assert_eq!(message.text, "频道消息");
}

#[test]
fn non_dispatch_frames_and_unrelated_events_produce_no_inbound() {
    let hello = frame_of(json!({"op": 10, "d": {"heartbeat_interval": 41250}}));
    assert!(
        normalize(&hello, Timestamp::from_unix_millis(SENTINEL))
            .expect("hello must not be an error")
            .is_none()
    );

    let ready = frame_of(json!({"op": 0, "s": 1, "t": "READY", "d": {"session_id": "s"}}));
    assert!(
        normalize(&ready, Timestamp::from_unix_millis(SENTINEL))
            .expect("READY must not be an error")
            .is_none()
    );

    let presence = frame_of(json!({"op": 0, "s": 2, "t": "SOME_OTHER_EVENT", "d": {}}));
    assert!(
        normalize(&presence, Timestamp::from_unix_millis(SENTINEL))
            .expect("unknown events must not be an error")
            .is_none()
    );
}

#[test]
fn a_group_message_without_an_id_is_a_protocol_error() {
    let mut payload = group_payload("GROUP_MESSAGE_CREATE");
    payload["d"].as_object_mut().expect("object").remove("id");
    let frame = frame_of(payload);
    let error = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect_err("a message without `d.id` cannot be replied to");
    assert!(
        error.to_string().contains("d.id"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_auth_token_from_message_scene_is_redacted_in_raw() {
    let frame = frame_of(group_payload("GROUP_MESSAGE_CREATE"));
    let message = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect("group message must normalize")
        .expect("a group message is an inbound message");

    let raw = message.raw.to_string();
    assert!(
        !raw.contains(AUTH_TOKEN_SECRET),
        "raw must never carry the auth_token value: {raw}"
    );
    assert!(
        raw.contains("<redacted>"),
        "raw must keep the key with a redacted value: {raw}"
    );
    // 其它消息内容要完整保留（脱敏不能把整帧抹掉）。
    assert!(
        raw.contains("REFIDX_EXAMPLE"),
        "raw must keep the rest: {raw}"
    );
    assert!(
        raw.contains(MESSAGE_ID),
        "raw must keep the message id: {raw}"
    );
}

#[test]
fn a_broken_timestamp_falls_back_to_the_provided_clock() {
    let mut payload = group_payload("GROUP_MESSAGE_CREATE");
    payload["d"]["timestamp"] = json!("不是时间");
    let frame = frame_of(payload);
    let message = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect("a broken timestamp must not drop the message")
        .expect("a group message is an inbound message");
    assert_eq!(message.received_at.unix_millis(), SENTINEL);
}

#[test]
fn at_mentions_are_stripped_but_text_is_otherwise_untouched() {
    assert_eq!(strip_at_mentions("你好"), "你好");
    assert_eq!(
        strip_at_mentions(&format!("<@{GROUP_OPENID}> 你好")),
        "你好"
    );
    assert_eq!(
        strip_at_mentions(&format!("<@!{GROUP_OPENID}> 你好")),
        "你好"
    );
    assert_eq!(strip_at_mentions("<@!123456> 你好"), "你好");
    assert_eq!(strip_at_mentions("<@123456>你好"), "你好");
    // 中间与结尾的提及也要剥，其它内容原样保留（包括表情与换行）。
    assert_eq!(
        strip_at_mentions(&format!("<@{}> 前面 <@!123> 后面", GROUP_OPENID)),
        "前面 后面"
    );
    // 不是提及的尖括号内容不动。
    assert_eq!(strip_at_mentions("<b>粗体</b>"), "<b>粗体</b>");
    assert_eq!(strip_at_mentions("<@not-hex> hi"), "<@not-hex> hi");
}

#[test]
fn bot_mention_detection_relies_on_is_you() {
    let mentioned = group_payload("GROUP_MESSAGE_CREATE");
    assert!(is_bot_mentioned(&mentioned["d"]));

    // 提到了别人（或只有 @ 标记文本）不算 @ 了机器人。
    let mut other = group_payload("GROUP_MESSAGE_CREATE");
    other["d"]["mentions"][0]["is_you"] = json!(false);
    assert!(!is_bot_mentioned(&other["d"]));

    let mut no_mentions = group_payload("GROUP_MESSAGE_CREATE");
    no_mentions["d"]
        .as_object_mut()
        .expect("object")
        .remove("mentions");
    assert!(!is_bot_mentioned(&no_mentions["d"]));
}

#[test]
fn the_channel_conversation_namespace_is_documented_as_distinct_from_onebot() {
    // 这条测试是「不许混用两套会话命名空间」的钉子：QQ 官方会话 ID 是 openid 字符串。
    let frame = frame_of(group_payload("GROUP_MESSAGE_CREATE"));
    let message = normalize(&frame, Timestamp::from_unix_millis(SENTINEL))
        .expect("group message must normalize")
        .expect("a group message is an inbound message");
    assert_eq!(message.conversation_external_id.len(), 32);
    assert!(
        message
            .conversation_external_id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit()),
        "openid is 32 hex chars, not a QQ number"
    );
}
