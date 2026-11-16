//! RT-06：`Debug` 不许把密钥打出来（红队用例 f03/f04 的同款断言 + 判据矩阵）。
//!
//! 这些类型都会进日志与"发给模型的工具输出"，所以 `Debug` 的形状是**安全契约**：
//! 字段名必须在（否则等于删掉调试能力），敏感值必须是
//! [`hbot_core::redact::REDACTED`]，普通词（`max_tokens` / `tokenizer` / `monkey`）
//! 必须原样可见。共享的判据在 `hbot_core::redact`（唯一一份实现）。

use std::str::FromStr;

use hbot_core::redact::{REDACTED, RedactedJson, is_secret_key, redact_arg, redact_value};
use hbot_core::{
    BotId, CommandSpec, ConversationId, InboundMessage, MessageId, SessionId, Timestamp, ToolCall,
    ToolCallStatus,
};
use serde_json::{Value, json};

/// 与 `crates/hbot-redteam/tests/f_secret_leak.rs` 同款假密钥。
const SECRET: &str = "sk-live-1234";

fn message(payload: Value) -> InboundMessage {
    InboundMessage {
        id: MessageId::from_str("55555555-5555-4555-8555-555555555555").expect("valid uuid"),
        bot_id: BotId::from_str("22222222-2222-4222-8222-222222222222").expect("valid uuid"),
        conversation_id: ConversationId::from_str("33333333-3333-4333-8333-333333333333")
            .expect("valid uuid"),
        external_id: "evt-0001".to_owned(),
        sender_id: "10001".to_owned(),
        payload,
        received_at: Timestamp::from_unix_millis(1_700_000_000_000),
        processed_at: None,
    }
}

#[test]
fn debug_of_command_spec_never_prints_secrets() {
    let mut spec = CommandSpec::new("cmd", std::env::temp_dir());
    spec.env = vec![
        ("API_TOKEN".to_owned(), SECRET.to_owned()),
        ("RUST_LOG".to_owned(), "info".to_owned()),
    ];
    spec.args = vec![
        format!("--token={SECRET}"),
        "--verbose".to_owned(),
        "status".to_owned(),
    ];
    let printed = format!("{spec:?}");

    assert!(
        !printed.contains(SECRET),
        "CommandSpec 的 Debug 泄漏了密钥：{printed}"
    );
    // 字段名都在（调试价值不能丢）。
    for field in [
        "program",
        "args",
        "cwd",
        "workspace_root",
        "env",
        "inherit_env",
        "inherit_entire_env",
        "timeout",
        "max_output_bytes",
        "sandbox",
        "allowed_programs",
    ] {
        assert!(printed.contains(field), "Debug 里缺字段 {field}：{printed}");
    }
    // 非敏感值原样保留。
    assert!(printed.contains("API_TOKEN"), "变量名应保留：{printed}");
    assert!(printed.contains("RUST_LOG"), "变量名应保留：{printed}");
    assert!(printed.contains("info"), "非敏感值应原样：{printed}");
    assert!(printed.contains("--verbose"), "非敏感参数应原样：{printed}");
    assert!(printed.contains(REDACTED), "应出现脱敏占位符：{printed}");
}

#[test]
fn debug_of_inbound_redacts_authorization() {
    // 入站 payload 是任意 JSON：`Authorization` 在嵌套对象里，`auth_token=…` 在字符串里。
    let message = message(json!({
        "text": "hi",
        "headers": {"Authorization": "QRBT abc"},
        "message_scene": {"ext": "auth_token=xyz"},
        "max_tokens": 4096,
    }));
    let printed = format!("{message:?}");

    assert!(
        !printed.contains("QRBT"),
        "payload 里的 Authorization 值泄漏：{printed}"
    );
    assert!(
        !printed.contains("xyz"),
        "payload 里的 auth_token 值泄漏：{printed}"
    );
    // 键名与不敏感内容必须还在。
    assert!(printed.contains("Authorization"), "键名应保留：{printed}");
    assert!(printed.contains("auth_token"), "键名应保留：{printed}");
    assert!(
        printed.contains("\"text\""),
        "payload 结构应保留：{printed}"
    );
    assert!(printed.contains("hi"), "非敏感值应原样：{printed}");
    assert!(printed.contains("4096"), "max_tokens 不该被抹：{printed}");
    assert!(
        printed.contains("evt-0001"),
        "external_id 应原样：{printed}"
    );
}

#[test]
fn debug_of_domain_tool_call_redacts_args() {
    let call = ToolCall {
        id: "call-1".to_owned(),
        session_id: SessionId::from_str("44444444-4444-4444-8444-444444444444")
            .expect("valid uuid"),
        name: "http_fetch".to_owned(),
        args: json!({"url": "https://example.com", "api_key": SECRET}),
        result: Some(json!({"ok": true, "echo": format!("token={SECRET}")})),
        status: ToolCallStatus::Succeeded,
        approval: None,
        created_at: Timestamp::from_unix_millis(1),
        finished_at: None,
    };
    let printed = format!("{call:?}");

    assert!(
        !printed.contains(SECRET),
        "ToolCall 的 Debug 泄漏了密钥：{printed}"
    );
    assert!(
        printed.contains("https://example.com"),
        "非敏感值应原样：{printed}"
    );
    assert!(printed.contains("finished_at"), "字段名应保留：{printed}");
}

#[test]
fn ordinary_words_containing_key_are_not_redacted() {
    // 反向用例：这些词的"值"必须原样可见，否则调参日志不可读。
    let message = message(json!({
        "max_tokens": 4096,
        "tokenizer": "cl100k",
        "monkey": true,
        "keyboard": "layout-us",
    }));
    let printed = format!("{message:?}");

    for visible in ["4096", "cl100k", "true", "layout-us"] {
        assert!(printed.contains(visible), "{visible} 不该被抹：{printed}");
    }
    assert!(!printed.contains(REDACTED), "不该出现脱敏占位符：{printed}");

    // 文本形态（`redact_arg`）同样不动它们。
    assert_eq!(redact_arg("max_tokens=4096"), "max_tokens=4096");
    assert_eq!(redact_arg("tokenizer=cl100k"), "tokenizer=cl100k");
    assert_eq!(redact_arg("monkey=true"), "monkey=true");
    assert_eq!(redact_arg("keyboard=layout-us"), "keyboard=layout-us");
}

#[test]
fn sensitive_key_variants_are_recognised() {
    // 判据矩阵：下划线 / 连字符 / 驼峰 / 全大写都算同一个词义。
    let keys = [
        "token",
        "api_key",
        "apikey",
        "secret",
        "password",
        "passwd",
        "authorization",
        "credential",
        "cookie",
        "auth_token",
        "OPENAI_API_KEY",
        "access-token",
        "clientSecret",
        "X-Auth-Token",
    ];
    for key in keys {
        assert!(is_secret_key(key), "{key} 应判为敏感键名");

        // JSON 形态：命中键名的**整个值**被换掉，键名保留。
        let mut map = serde_json::Map::new();
        map.insert(key.to_owned(), json!(SECRET));
        let printed = format!("{:?}", RedactedJson(&Value::Object(map)));
        assert!(!printed.contains(SECRET), "{key} 的值泄漏：{printed}");
        assert!(printed.contains(key), "{key} 的键名应保留：{printed}");

        // 文本形态：`key=value` 与 `key: value` 都抹。
        assert!(
            !redact_arg(&format!("{key}={SECRET}")).contains(SECRET),
            "{key}=… 未脱敏"
        );
        assert!(
            !redact_arg(&format!("{key}: {SECRET}")).contains(SECRET),
            "{key}: … 未脱敏"
        );
    }
}

#[test]
fn redact_value_recurses_into_arrays_and_objects() {
    let payload = json!({
        "events": [
            {"headers": {"authorization": format!("Bearer {SECRET}")}},
            {"note": "safe", "password": SECRET},
        ],
        "max_tokens": 4096,
    });
    let safe = redact_value(&payload);
    let printed = format!("{safe:?}");

    assert!(!printed.contains(SECRET), "嵌套数组里的密钥泄漏：{printed}");
    assert!(printed.contains("safe"), "非敏感值应原样：{printed}");
    assert!(printed.contains("4096"), "max_tokens 不该被抹：{printed}");
}
