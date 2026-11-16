//! 事件 JSON 形状的跨 crate 防漂移测试。
//!
//! `hbot-code` 只允许依赖 `hbot-core`（见 `xtask/src/deps.rs`），所以拿不到
//! `hbot_bot::event_json` 的真实输出；`hbot-rpc` 同时依赖两者，在这里把
//! `HOST_DTS` 里 `MessageCtx` 声明的字段名集合与 `event_json` 实际输出的键集合
//! 直接比对。任何一边加字段忘改另一边，这里就红。

use std::collections::BTreeSet;

use hbot_bot::{EventSource, event_json};
use hbot_code::dts::HOST_DTS;
use hbot_core::{BotId, ConversationId, ConversationKind, MessageId, Timestamp};
use serde_json::json;
use uuid::Uuid;

/// `MessageCtx` 里声明的属性名（不含 `reply` / `log` / `sleep` 这些方法）。
fn message_ctx_properties() -> BTreeSet<String> {
    let start = HOST_DTS
        .find("export interface MessageCtx {")
        .expect("hbot.d.ts 应有 `MessageCtx` 接口");
    let block = &HOST_DTS[start..];
    let end = block.find("\n  }").expect("MessageCtx 应有收尾 `}`");

    block[..end]
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("    ")?;
            let trimmed = rest.trim_start();
            // 跳过属性上方的文档注释行。
            if trimmed.starts_with('*') || trimmed.starts_with('/') {
                return None;
            }
            let (name, after) = rest.split_once(':')?;
            // 属性行以 `;` 结尾；方法行在 `:` 之前带 `(`。
            if !after.trim_end().ends_with(';') || rest.contains('(') {
                return None;
            }
            let name = name.trim();
            let is_property = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
            is_property.then(|| name.to_owned())
        })
        .collect()
}

fn sample_source<'a>(payload: &'a serde_json::Value) -> EventSource<'a> {
    EventSource {
        bot_id: BotId::from_uuid(Uuid::nil()),
        conversation_id: ConversationId::from_uuid(Uuid::nil()),
        message_id: MessageId::from_uuid(Uuid::nil()),
        external_id: "evt-1",
        sender_id: "u-1",
        text: None,
        kind: ConversationKind::Unknown,
        conversation_external_id: "group-42",
        received_at: Timestamp::from_unix_millis(1_700_000_000_123),
        payload,
    }
}

#[test]
fn event_json_keys_match_the_message_ctx_declaration() {
    let payload = json!({"postType": "message"});
    let event = event_json(&sample_source(&payload));
    let actual: BTreeSet<String> = event
        .as_object()
        .expect("event_json 必须输出 JSON 对象")
        .keys()
        .cloned()
        .collect();
    let declared = message_ctx_properties();

    assert!(
        !declared.is_empty(),
        "HOST_DTS 里没解析到 MessageCtx 属性，扫描逻辑失效了"
    );
    assert_eq!(
        actual, declared,
        "event_json 输出的键与 MessageCtx 声明不一致：运行时 {actual:?}，类型 {declared:?}"
    );
}

#[test]
fn missing_text_stays_an_empty_string_in_the_shared_implementation() {
    let payload = json!({"image": "https://example.test/a.png"});
    let event = event_json(&sample_source(&payload));

    assert_eq!(
        event.get("text"),
        Some(&json!("")),
        "hbot.d.ts 声明 `text: string`（空串，不是 null）；共享拼装点不能改回 null"
    );
}
