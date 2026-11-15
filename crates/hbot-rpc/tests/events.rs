//! 事件契约测试：12 个 `Event` 的点分 `type` 值与 camelCase 字段。

mod support;

use hbot_core::{ApprovalDecision, BotStatus, Timestamp, ToolCall, ToolCallStatus};
use hbot_rpc::{
    DebugScope, Diagnostic, DiagnosticSeverity, DiffHunk, EditFile, Event, FileChangeKind,
    LogLevel, RiskLevel, SearchMatch, StackFrame,
};
use serde_json::json;
use support::{
    APPROVAL_UUID, AT, BOT_UUID, EDIT_UUID, SEARCH_UUID, SESSION_UUID, approval, assert_event, at,
    bot, edit, search, session,
};

#[test]
fn agent_events_use_dotted_type_and_camel_case_fields() {
    assert_event(
        Event::AgentDelta {
            session_id: session(),
            text: "hi".to_string(),
        },
        json!({
            "type": "agent.delta",
            "sessionId": SESSION_UUID,
            "text": "hi",
        }),
    );

    assert_event(
        Event::AgentStep {
            session_id: session(),
            step: 3,
        },
        json!({ "type": "agent.step", "sessionId": SESSION_UUID, "step": 3 }),
    );

    let call = ToolCall {
        id: "call_1".to_string(),
        session_id: session(),
        name: "read_file".to_string(),
        args: json!({ "path": "src/main.rs" }),
        result: Some(json!({ "content": "fn main() {}" })),
        status: ToolCallStatus::Succeeded,
        approval: Some(ApprovalDecision::Approve),
        created_at: at(),
        finished_at: Some(Timestamp::from_unix_millis(AT + 5)),
    };
    assert_event(
        Event::AgentTool {
            session_id: session(),
            call: call.clone(),
        },
        json!({
            "type": "agent.tool",
            "sessionId": SESSION_UUID,
            "call": {
                "id": "call_1",
                "sessionId": SESSION_UUID,
                "name": "read_file",
                "args": { "path": "src/main.rs" },
                "result": { "content": "fn main() {}" },
                "status": "succeeded",
                "approval": "approve",
                "createdAt": AT,
                "finishedAt": AT + 5,
            },
        }),
    );
}

#[test]
fn approval_bot_run_and_project_events_use_dotted_type_and_camel_case_fields() {
    assert_event(
        Event::ApprovalRequest {
            approval_id: approval(),
            tool: "run_command".to_string(),
            args: json!({ "cmd": "cargo test" }),
            risk: RiskLevel::High,
        },
        json!({
            "type": "approval.request",
            "approvalId": APPROVAL_UUID,
            "tool": "run_command",
            "args": { "cmd": "cargo test" },
            "risk": "high",
        }),
    );

    assert_event(
        Event::BotStatus {
            bot_id: bot(),
            status: BotStatus::Degraded,
            error: Some("platform disconnected".to_string()),
        },
        json!({
            "type": "bot.status",
            "botId": BOT_UUID,
            "status": "degraded",
            "error": "platform disconnected",
        }),
    );

    // 正常状态时 error 显式 null。
    assert_event(
        Event::BotStatus {
            bot_id: bot(),
            status: BotStatus::Running,
            error: None,
        },
        json!({
            "type": "bot.status",
            "botId": BOT_UUID,
            "status": "running",
            "error": null,
        }),
    );

    assert_event(
        Event::RunLog {
            bot_id: bot(),
            level: LogLevel::Warn,
            line: "retrying outbound".to_string(),
            at: at(),
        },
        json!({
            "type": "run.log",
            "botId": BOT_UUID,
            "level": "warn",
            "line": "retrying outbound",
            "at": AT,
        }),
    );

    assert_event(
        Event::ProjectChanged {
            path: "src/main.rs".to_string(),
            kind: FileChangeKind::Updated,
        },
        json!({
            "type": "project.changed",
            "path": "src/main.rs",
            "kind": "updated",
        }),
    );
}

#[test]
fn search_and_diagnostics_events_use_dotted_type_and_camel_case_fields() {
    assert_event(
        Event::FileSearch {
            search_id: search(),
            matches: vec![SearchMatch {
                path: "src/main.rs".to_string(),
                line: 1,
                column: 4,
                text: "fn main() {}".to_string(),
            }],
        },
        json!({
            "type": "file.search",
            "searchId": SEARCH_UUID,
            "matches": [{
                "path": "src/main.rs",
                "line": 1,
                "column": 4,
                "text": "fn main() {}",
            }],
        }),
    );

    assert_event(
        Event::Diagnostics {
            path: "src/main.rs".to_string(),
            items: vec![Diagnostic {
                path: "src/main.rs".to_string(),
                line: 2,
                column: 7,
                severity: DiagnosticSeverity::Error,
                message: "cannot find type".to_string(),
                code: Some("E0412".to_string()),
            }],
        },
        json!({
            "type": "diagnostics",
            "path": "src/main.rs",
            "items": [{
                "path": "src/main.rs",
                "line": 2,
                "column": 7,
                "severity": "error",
                "message": "cannot find type",
                "code": "E0412",
            }],
        }),
    );
}

#[test]
fn diff_and_debug_events_use_dotted_type_and_camel_case_fields() {
    assert_event(
        Event::AgentDiff {
            edit_id: edit(),
            files: vec![EditFile {
                path: "src/main.rs".to_string(),
                hunks: vec![DiffHunk {
                    index: 0,
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 2,
                    lines: vec![
                        "-fn main() {}".to_string(),
                        "+fn main() { todo!() }".to_string(),
                    ],
                }],
            }],
        },
        json!({
            "type": "agent.diff",
            "editId": EDIT_UUID,
            "files": [{
                "path": "src/main.rs",
                "hunks": [{
                    "index": 0,
                    "oldStart": 1,
                    "oldLines": 1,
                    "newStart": 1,
                    "newLines": 2,
                    "lines": ["-fn main() {}", "+fn main() { todo!() }"],
                }],
            }],
        }),
    );

    assert_event(
        Event::DebugPaused {
            session_id: session(),
            file: "src/main.rs".to_string(),
            line: 12,
            scopes: vec![DebugScope {
                name: "locals".to_string(),
                variables: [("count".to_string(), json!(3))].into_iter().collect(),
            }],
            call_stack: vec![StackFrame {
                name: "main".to_string(),
                file: "src/main.rs".to_string(),
                line: 12,
                column: 1,
            }],
        },
        json!({
            "type": "debug.paused",
            "sessionId": SESSION_UUID,
            "file": "src/main.rs",
            "line": 12,
            "scopes": [{ "name": "locals", "variables": { "count": 3 } }],
            "callStack": [{
                "name": "main",
                "file": "src/main.rs",
                "line": 12,
                "column": 1,
            }],
        }),
    );

    assert_event(
        Event::DebugResumed {
            session_id: session(),
        },
        json!({ "type": "debug.resumed", "sessionId": SESSION_UUID }),
    );
}

#[test]
fn event_names_are_exactly_the_documented_set() {
    // 防止改名/加变体时悄悄破坏 UI 订阅表：这里逐一列出 §9.2 的 12 个事件名。
    let names = [
        "agent.delta",
        "agent.step",
        "agent.tool",
        "approval.request",
        "bot.status",
        "run.log",
        "project.changed",
        "file.search",
        "diagnostics",
        "agent.diff",
        "debug.paused",
        "debug.resumed",
    ];
    assert_eq!(names.len(), 12);

    for name in names {
        let err = serde_json::from_value::<Event>(json!({ "type": name }));
        // 只有 type 必然缺字段；但报错信息不应是 "unknown variant"。
        let text = err.expect_err("缺字段必须报错").to_string();
        assert!(
            !text.contains("unknown variant"),
            "事件 `{name}` 未在 Event 中定义：{text}"
        );
    }
}
