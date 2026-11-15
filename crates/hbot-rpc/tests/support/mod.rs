//! 契约测试共用的小工具：固定 ID/时间、camelCase 键检查、逐条断言。
//!
//! 这是 `tests/` 下的模块（不是测试 target），由各测试文件 `mod support;` 引入。
//! 因不同测试文件只用到其中一部分，这里允许 `dead_code`。

#![allow(dead_code)]

use std::str::FromStr;

use hbot_core::{ApprovalId, BotId, ConversationId, EditId, SearchId, SessionId, Timestamp};
use hbot_rpc::{Command, Event};
use serde_json::Value;

/// 项目 ID 的固定 UUID。
pub const PROJECT_UUID: &str = "11111111-1111-4111-8111-111111111111";
/// Bot ID 的固定 UUID。
pub const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";
/// 会话 ID 的固定 UUID。
pub const CONVERSATION_UUID: &str = "33333333-3333-4333-8333-333333333333";
/// Agent 会话 ID 的固定 UUID。
pub const SESSION_UUID: &str = "44444444-4444-4444-8444-444444444444";
/// 消息 ID 的固定 UUID。
pub const MESSAGE_UUID: &str = "55555555-5555-4555-8555-555555555555";
/// 审批请求 ID 的固定 UUID。
pub const APPROVAL_UUID: &str = "88888888-8888-4888-8888-888888888888";
/// 编辑批次 ID 的固定 UUID。
pub const EDIT_UUID: &str = "99999999-9999-4999-8999-999999999999";
/// 搜索会话 ID 的固定 UUID。
pub const SEARCH_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
/// 固定时间戳（Unix 毫秒，与 UUID 一起让快照完全确定）。
pub const AT: i64 = 1_700_000_000_000;

/// 固定 Agent 会话 ID。
pub fn session() -> SessionId {
    SessionId::from_str(SESSION_UUID).expect("valid uuid")
}

/// 固定 bot ID。
pub fn bot() -> BotId {
    BotId::from_str(BOT_UUID).expect("valid uuid")
}

/// 固定会话 ID。
pub fn conversation() -> ConversationId {
    ConversationId::from_str(CONVERSATION_UUID).expect("valid uuid")
}

/// 固定时间戳。
pub fn at() -> Timestamp {
    Timestamp::from_unix_millis(AT)
}

/// 固定审批请求 ID。
pub fn approval() -> ApprovalId {
    ApprovalId::from_str(APPROVAL_UUID).expect("valid uuid")
}

/// 固定编辑批次 ID。
pub fn edit() -> EditId {
    EditId::from_str(EDIT_UUID).expect("valid uuid")
}

/// 固定搜索会话 ID。
pub fn search() -> SearchId {
    SearchId::from_str(SEARCH_UUID).expect("valid uuid")
}

/// 递归断言：所有对象键都不含 `_`（即没有漏改的 snake_case 字段）。
///
/// 只检查 key；值里的 snake_case（如枚举 `"workspace_write"`）是合法的。
pub fn assert_camel_case_keys(value: &Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !key.contains('_'),
                    "字段名出现 snake_case：`{path}.{key}`（应为 camelCase）"
                );
                assert_camel_case_keys(child, &format!("{path}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_camel_case_keys(child, &format!("{path}[{index}]"));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// 断言命令 JSON：形状完全一致、字段全 camelCase、且能往返。
pub fn assert_command(command: Command, expected: Value) {
    let actual = serde_json::to_value(&command).expect("serialize command");
    assert_eq!(actual, expected, "Command JSON 形状不符：{command:?}");
    assert_camel_case_keys(&actual, "command");
    let parsed: Command = serde_json::from_value(expected.clone()).expect("deserialize command");
    assert_eq!(parsed, command, "Command 往返不一致：{expected}");
}

/// 断言事件 JSON：形状完全一致、字段全 camelCase、且能往返。
pub fn assert_event(event: Event, expected: Value) {
    let actual = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(actual, expected, "Event JSON 形状不符：{event:?}");
    assert_camel_case_keys(&actual, "event");
    let parsed: Event = serde_json::from_value(expected.clone()).expect("deserialize event");
    assert_eq!(parsed, event, "Event 往返不一致：{expected}");
}
