//! RT-06：`NormalizedInbound` 的 `Debug` 不许打出原始事件里的凭据。
//!
//! 平台原始事件（`raw`）里就带凭据：QQ 官方的 `message_scene.ext` 含
//! `auth_token=…`，部分 OneBot 实现会把请求头（含 `Authorization`）一起塞进事件。
//! 适配器一旦 `tracing::warn!(?inbound)` 或把归一化失败包进错误链，密钥就落进
//! `logs` 表。判据复用 `hbot_core::redact`，这里钉住形状与递归覆盖。

use hbot_adapters::NormalizedInbound;
use hbot_core::redact::REDACTED;
use hbot_core::{ConversationKind, Timestamp};
use serde_json::json;

fn inbound(raw: serde_json::Value) -> NormalizedInbound {
    NormalizedInbound {
        bot_platform_id: Some(10001),
        external_id: "evt-0001".to_owned(),
        conversation_external_id: "group_openid_abc".to_owned(),
        kind: ConversationKind::Group,
        sender_id: "user_openid_xyz".to_owned(),
        text: "hi".to_owned(),
        reply_to: None,
        platform_message_id: None,
        raw,
        received_at: Timestamp::from_unix_millis(1_700_000_000_000),
    }
}

#[test]
fn debug_of_normalized_inbound_redacts_raw_credentials() {
    let printed = format!(
        "{:?}",
        inbound(json!({
            "d": {"id": "evt-0001", "content": "hi"},
            "message_scene": {"ext": "auth_token=sk-live-1234"},
            "headers": {"Authorization": "QRBT abc"},
            "seq": 4096,
        }))
    );

    assert!(
        !printed.contains("sk-live-1234"),
        "raw 里的 auth_token 泄漏：{printed}"
    );
    assert!(
        !printed.contains("QRBT"),
        "raw 里的 Authorization 泄漏：{printed}"
    );
    // 键名、平台 ID 与非敏感内容必须还在，否则排查入站事件没有依据。
    assert!(printed.contains("auth_token"), "键名应保留：{printed}");
    assert!(printed.contains("Authorization"), "键名应保留：{printed}");
    assert!(
        printed.contains("group_openid_abc"),
        "会话 ID 应原样：{printed}"
    );
    assert!(
        printed.contains("user_openid_xyz"),
        "发送者 ID 应原样：{printed}"
    );
    assert!(printed.contains("4096"), "普通数字应原样：{printed}");
    assert!(printed.contains(REDACTED), "应出现脱敏占位符：{printed}");
}

#[test]
fn debug_of_normalized_inbound_keeps_ordinary_words() {
    let printed = format!(
        "{:?}",
        inbound(json!({"max_tokens": 4096, "tokenizer": "cl100k", "monkey": true}))
    );

    for visible in ["4096", "cl100k", "true"] {
        assert!(printed.contains(visible), "{visible} 不该被抹：{printed}");
    }
    assert!(!printed.contains(REDACTED), "不该出现脱敏占位符：{printed}");
}
