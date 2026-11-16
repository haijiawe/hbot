//! RT-06：`ToolCallRequest` 的 `Debug` 不许打出参数里的密钥。
//!
//! 这个类型是「模型 → 工具」那一跳的载体，`arguments` 完全由模型给；
//! 它进日志（`tracing::warn!(?call)`）、进 `thiserror` 的 `#[source]` 链、
//! 也进"工具调用失败"回给模型的描述 —— 派生 `Debug` 就是密钥落库的最短路径。
//! 判据复用 `hbot_core::redact`（唯一一份实现），这里只钉住形状。

use hbot_agent::ToolCallRequest;
use hbot_core::redact::REDACTED;
use serde_json::json;

fn call(arguments: serde_json::Value) -> ToolCallRequest {
    ToolCallRequest {
        id: "call-rt06".to_owned(),
        name: "http_fetch".to_owned(),
        arguments,
    }
}

#[test]
fn debug_of_tool_call_redacts_nested_arguments() {
    let printed = format!(
        "{:?}",
        call(json!({
            "url": "https://example.com/data",
            "headers": {"Authorization": "Bearer sk-x"},
            "max_tokens": 4096,
        }))
    );

    assert!(
        !printed.contains("sk-x"),
        "嵌套 Authorization 的值泄漏：{printed}"
    );
    // 键名、非敏感值、字段名都保留 —— 这行日志还得能用来排查。
    assert!(printed.contains("Authorization"), "键名应保留：{printed}");
    assert!(
        printed.contains("https://example.com/data"),
        "非敏感值应原样：{printed}"
    );
    assert!(printed.contains("4096"), "max_tokens 不该被抹：{printed}");
    assert!(printed.contains("call-rt06"), "id 应保留：{printed}");
    assert!(printed.contains("http_fetch"), "name 应保留：{printed}");
    assert!(printed.contains(REDACTED), "应出现脱敏占位符：{printed}");
}

#[test]
fn debug_of_tool_call_redacts_flat_credential_keys() {
    let printed = format!(
        "{:?}",
        call(json!({
            "api_key": "sk-live-1234",
            "token": "tok-5678",
            "password": "hunter2",
            "query": "safe",
        }))
    );

    for secret in ["sk-live-1234", "tok-5678", "hunter2"] {
        assert!(!printed.contains(secret), "{secret} 泄漏：{printed}");
    }
    assert!(printed.contains("safe"), "非敏感值应原样：{printed}");
    assert!(printed.contains("api_key"), "键名应保留：{printed}");
}

#[test]
fn debug_of_tool_call_keeps_ordinary_words() {
    let printed = format!(
        "{:?}",
        call(json!({"tokenizer": "cl100k", "monkey": true, "max_tokens": 8192}))
    );

    for visible in ["cl100k", "true", "8192"] {
        assert!(printed.contains(visible), "{visible} 不该被抹：{printed}");
    }
    assert!(!printed.contains(REDACTED), "不该出现脱敏占位符：{printed}");
}
