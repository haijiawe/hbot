//! 命令契约测试：25 个 `Command` 的 `type` 值与字段名（命令一律 camelCase 字段）。

mod support;

use hbot_core::{ApprovalDecision, BotStatus};
use hbot_rpc::{AppInfo, BotSummary, Command, DirEntry, FileContent, LogLevel, RunLog, StepKind};
use serde_json::json;
use support::{
    APPROVAL_UUID, AT, BOT_UUID, CONVERSATION_UUID, EDIT_UUID, SEARCH_UUID, SESSION_UUID, approval,
    assert_camel_case_keys, assert_command, at, bot, conversation, edit, search, session,
};

#[test]
fn app_project_and_agent_commands_use_camel_case_fields() {
    // `type` 是变体的 snake_case；字段一律 camelCase（命令与事件统一）。
    assert_command(Command::AppInfo, json!({ "type": "app_info" }));

    assert_command(
        Command::ProjectOpen {
            path: "E:\\demo".to_string(),
        },
        json!({ "type": "project_open", "path": "E:\\demo" }),
    );

    // 新建项目：`data` 是 `ProjectSummary`（path/name/entry），不是 `AppInfo`。
    assert_command(
        Command::ProjectCreate {
            path: "E:\\demo-new".to_string(),
            name: "演示项目".to_string(),
        },
        json!({
            "type": "project_create",
            "path": "E:\\demo-new",
            "name": "演示项目",
        }),
    );

    assert_command(
        Command::AgentSend {
            session_id: Some(session()),
            text: "你好".to_string(),
        },
        json!({
            "type": "agent_send",
            "sessionId": SESSION_UUID,
            "text": "你好",
        }),
    );

    // 新建会话：`sessionId: null`（UI 第一次发消息就是这条），核心回 `data.sessionId`。
    assert_command(
        Command::AgentSend {
            session_id: None,
            text: "你好".to_string(),
        },
        json!({
            "type": "agent_send",
            "sessionId": null,
            "text": "你好",
        }),
    );

    assert_command(
        Command::AgentCancel {
            session_id: session(),
        },
        json!({ "type": "agent_cancel", "sessionId": SESSION_UUID }),
    );

    assert_command(
        Command::AgentApprove {
            approval_id: approval(),
            decision: ApprovalDecision::Deny,
            always: false,
        },
        json!({
            "type": "agent_approve",
            "approvalId": APPROVAL_UUID,
            "decision": "deny",
            "always": false,
        }),
    );
}

#[test]
fn file_commands_use_camel_case_fields() {
    assert_command(
        Command::FileRead {
            path: "src/main.rs".to_string(),
        },
        json!({ "type": "file_read", "path": "src/main.rs" }),
    );

    assert_command(
        Command::FileWrite {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
            base_hash: Some("sha256:abc".to_string()),
        },
        json!({
            "type": "file_write",
            "path": "src/main.rs",
            "content": "fn main() {}",
            "baseHash": "sha256:abc",
        }),
    );

    // 可选字段必须显式是 null（不省略），否则 UI 侧 union 类型会少分支。
    assert_command(
        Command::FileWrite {
            path: "src/main.rs".to_string(),
            content: String::new(),
            base_hash: None,
        },
        json!({
            "type": "file_write",
            "path": "src/main.rs",
            "content": "",
            "baseHash": null,
        }),
    );

    assert_command(
        Command::FileList {
            path: "src".to_string(),
        },
        json!({ "type": "file_list", "path": "src" }),
    );

    assert_command(
        Command::FileCreate {
            path: "src/lib.rs".to_string(),
            content: "// hi\n".to_string(),
        },
        json!({
            "type": "file_create",
            "path": "src/lib.rs",
            "content": "// hi\n",
        }),
    );

    assert_command(
        Command::FileRename {
            from: "a.txt".to_string(),
            to: "b.txt".to_string(),
        },
        json!({ "type": "file_rename", "from": "a.txt", "to": "b.txt" }),
    );

    assert_command(
        Command::FileDelete {
            path: "a.txt".to_string(),
            to_trash: true,
        },
        json!({ "type": "file_delete", "path": "a.txt", "toTrash": true }),
    );
}

#[test]
fn edit_search_and_diagnostics_commands_use_camel_case_fields() {
    assert_command(
        Command::EditApply {
            edit_id: edit(),
            accept_hunks: vec![0, 2],
        },
        json!({
            "type": "edit_apply",
            "editId": EDIT_UUID,
            "acceptHunks": [0, 2],
        }),
    );

    assert_command(
        Command::EditReject { edit_id: edit() },
        json!({ "type": "edit_reject", "editId": EDIT_UUID }),
    );

    assert_command(
        Command::SearchStart {
            query: "fn main".to_string(),
            regex: false,
            globs: vec!["**/*.rs".to_string()],
        },
        json!({
            "type": "search_start",
            "query": "fn main",
            "regex": false,
            "globs": ["**/*.rs"],
        }),
    );

    assert_command(
        Command::SearchCancel {
            search_id: search(),
        },
        json!({ "type": "search_cancel", "searchId": SEARCH_UUID }),
    );

    assert_command(
        Command::Diagnostics { path: None },
        json!({ "type": "diagnostics", "path": null }),
    );
}

#[test]
fn bot_debug_and_log_commands_use_camel_case_fields() {
    assert_command(
        Command::BotStart { bot_id: bot() },
        json!({ "type": "bot_start", "botId": BOT_UUID }),
    );

    assert_command(
        Command::BotStop { bot_id: bot() },
        json!({ "type": "bot_stop", "botId": BOT_UUID }),
    );

    assert_command(
        Command::BotInject {
            bot_id: bot(),
            conversation_id: conversation(),
            text: "ping".to_string(),
        },
        json!({
            "type": "bot_inject",
            "botId": BOT_UUID,
            "conversationId": CONVERSATION_UUID,
            "text": "ping",
        }),
    );

    assert_command(
        Command::DebugSetBreakpoints {
            bot_id: bot(),
            file: "src/main.rs".to_string(),
            lines: vec![10, 20],
        },
        json!({
            "type": "debug_set_breakpoints",
            "botId": BOT_UUID,
            "file": "src/main.rs",
            "lines": [10, 20],
        }),
    );

    assert_command(
        Command::DebugResume { bot_id: bot() },
        json!({ "type": "debug_resume", "botId": BOT_UUID }),
    );

    assert_command(
        Command::DebugStep {
            bot_id: bot(),
            kind: StepKind::Out,
        },
        json!({
            "type": "debug_step",
            "botId": BOT_UUID,
            "kind": "out",
        }),
    );

    assert_command(
        Command::LogsTail {
            bot_id: bot(),
            limit: 200,
        },
        json!({ "type": "logs_tail", "botId": BOT_UUID, "limit": 200 }),
    );
}

#[test]
fn command_fields_never_serialize_as_snake_case() {
    // 覆盖全部 25 个命令：任何一个字段漏改 camelCase 都会在这里红。
    // 与上面的形状断言重复构造是有意的——这里只关心"没有 `_` 字段名"。
    let commands = vec![
        Command::AppInfo,
        Command::ProjectOpen {
            path: "p".to_string(),
        },
        Command::ProjectCreate {
            path: "p".to_string(),
            name: "n".to_string(),
        },
        Command::AgentSend {
            session_id: Some(session()),
            text: "t".to_string(),
        },
        Command::AgentCancel {
            session_id: session(),
        },
        Command::AgentApprove {
            approval_id: approval(),
            decision: ApprovalDecision::Approve,
            always: true,
        },
        Command::FileRead {
            path: "p".to_string(),
        },
        Command::FileWrite {
            path: "p".to_string(),
            content: "c".to_string(),
            base_hash: Some("h".to_string()),
        },
        Command::FileList {
            path: "p".to_string(),
        },
        Command::FileCreate {
            path: "p".to_string(),
            content: "c".to_string(),
        },
        Command::FileRename {
            from: "a".to_string(),
            to: "b".to_string(),
        },
        Command::FileDelete {
            path: "p".to_string(),
            to_trash: false,
        },
        Command::EditApply {
            edit_id: edit(),
            accept_hunks: vec![1],
        },
        Command::EditReject { edit_id: edit() },
        Command::SearchStart {
            query: "q".to_string(),
            regex: true,
            globs: vec!["**/*.rs".to_string()],
        },
        Command::SearchCancel {
            search_id: search(),
        },
        Command::Diagnostics {
            path: Some("p".to_string()),
        },
        Command::BotStart { bot_id: bot() },
        Command::BotStop { bot_id: bot() },
        Command::BotInject {
            bot_id: bot(),
            conversation_id: conversation(),
            text: "t".to_string(),
        },
        Command::BotList,
        Command::DebugSetBreakpoints {
            bot_id: bot(),
            file: "f".to_string(),
            lines: vec![1],
        },
        Command::DebugResume { bot_id: bot() },
        Command::DebugStep {
            bot_id: bot(),
            kind: StepKind::Over,
        },
        Command::LogsTail {
            bot_id: bot(),
            limit: 1,
        },
    ];
    assert_eq!(commands.len(), 25, "命令数量变化时同步这张表");

    for command in &commands {
        let value = serde_json::to_value(command).expect("ser");
        assert_camel_case_keys(&value, "command");
        // `type` 值必须仍是 snake_case（允许点分名，但命令里没有）。
        let ty = value["type"].as_str().expect("type 必须是字符串");
        assert!(!ty.is_empty());
        assert_eq!(ty, ty.to_lowercase(), "type 值必须小写：{ty}");
    }
}

#[test]
fn command_step_kind_values() {
    for (kind, wire) in [
        (StepKind::Over, "over"),
        (StepKind::Into, "into"),
        (StepKind::Out, "out"),
    ] {
        assert_eq!(serde_json::to_value(kind).expect("ser"), json!(wire));
    }
}

#[test]
fn optional_command_fields_may_be_omitted_by_old_ui() {
    // 兼容旧 UI：不传 baseHash / path 等同 None（serde default）。
    let write: Command = serde_json::from_value(json!({
        "type": "file_write",
        "path": "a.txt",
        "content": "x",
    }))
    .expect("de");
    assert!(matches!(
        write,
        Command::FileWrite {
            base_hash: None,
            ..
        }
    ));

    let diagnostics: Command =
        serde_json::from_value(json!({ "type": "diagnostics" })).expect("de");
    assert!(matches!(diagnostics, Command::Diagnostics { path: None }));

    // `agentSend` 的 `sessionId` 也是可缺省的：不传 = 新建会话。
    let created: Command = serde_json::from_value(json!({
        "type": "agent_send",
        "text": "hi",
    }))
    .expect("de");
    assert!(matches!(
        created,
        Command::AgentSend {
            session_id: None,
            ..
        }
    ));
}

#[test]
fn commands_reject_unknown_type() {
    let err = serde_json::from_value::<Command>(json!({ "type": "does_not_exist" }));
    assert!(err.is_err(), "未知 Command 必须被拒绝");

    // 缺少必填字段也要报错，不能静默补默认值。
    let missing = serde_json::from_value::<Command>(json!({ "type": "file_read" }));
    assert!(missing.is_err(), "缺少 path 必须报错");
}

#[test]
fn command_result_types_use_camel_case() {
    // 「命令 → data 形状」表里的四个类型：字段名必须与表一致。
    let info = AppInfo {
        name: "hbot".to_string(),
        version: "0.1.0".to_string(),
        platform: "windows".to_string(),
        data_dir: "C:\\data\\hbot".to_string(),
    };
    assert_eq!(
        serde_json::to_value(&info).expect("ser"),
        json!({
            "name": "hbot",
            "version": "0.1.0",
            "platform": "windows",
            "dataDir": "C:\\data\\hbot",
        })
    );

    let content = FileContent {
        content: "fn main() {}".to_string(),
        hash: "sha256:abc".to_string(),
    };
    assert_eq!(
        serde_json::to_value(&content).expect("ser"),
        json!({ "content": "fn main() {}", "hash": "sha256:abc" })
    );

    let entry = DirEntry {
        name: "main.rs".to_string(),
        path: "src/main.rs".to_string(),
        is_dir: false,
    };
    assert_eq!(
        serde_json::to_value(&entry).expect("ser"),
        json!({ "name": "main.rs", "path": "src/main.rs", "isDir": false })
    );

    let log = RunLog {
        at: at(),
        level: LogLevel::Info,
        line: "started".to_string(),
    };
    assert_eq!(
        serde_json::to_value(&log).expect("ser"),
        json!({ "at": AT, "level": "info", "line": "started" })
    );

    // 往返，并确认没漏改的 snake_case 键。
    for value in [
        serde_json::to_value(&info).expect("ser"),
        serde_json::to_value(&content).expect("ser"),
        serde_json::to_value(&entry).expect("ser"),
        serde_json::to_value(&log).expect("ser"),
    ] {
        assert_camel_case_keys(&value, "result");
    }
    let parsed: DirEntry =
        serde_json::from_value(json!({ "name": "a", "path": "b", "isDir": true })).expect("de");
    assert!(parsed.is_dir);
}

#[test]
fn request_ids_must_be_uuid_strings() {
    // 审批/编辑/搜索的关联键都是 UUID newtype：旧的手写短 ID 必须被拒，
    // 不能在传输层默默接受字符串（否则 dispatch 拿到的类型不可靠）。
    for bad in ["appr_1", "edit_1", "search_1", "not-a-uuid", ""] {
        let edit_apply = serde_json::from_value::<Command>(json!({
            "type": "edit_apply",
            "editId": bad,
            "acceptHunks": [],
        }));
        assert!(edit_apply.is_err(), "EditApply 应拒绝 editId={bad:?}");

        let search_cancel = serde_json::from_value::<Command>(json!({
            "type": "search_cancel",
            "searchId": bad,
        }));
        assert!(
            search_cancel.is_err(),
            "SearchCancel 应拒绝 searchId={bad:?}"
        );
    }

    // 合法 UUID 仍然往返：类型 + 字段名不变。
    assert_command(
        Command::AgentApprove {
            approval_id: approval(),
            decision: ApprovalDecision::Approve,
            always: false,
        },
        json!({
            "type": "agent_approve",
            "approvalId": APPROVAL_UUID,
            "decision": "approve",
            "always": false,
        }),
    );
}

#[test]
fn bot_list_command_has_no_fields() {
    // 无字段命令：线格式里只有 `type`，多一个键都算契约漂移。
    assert_command(Command::BotList, json!({ "type": "bot_list" }));
}

#[test]
fn bot_summary_wire_shape_is_exact() {
    // `bot_list` 的行：键集固定（少一个 UI 就漏字段，多一个说明造了库里没有的列）。
    let summary = BotSummary {
        bot_id: bot(),
        name: "演示 bot".to_string(),
        platform: "onebot11".to_string(),
        status: BotStatus::Running,
        updated_at: at(),
    };

    let value = serde_json::to_value(&summary).expect("ser");
    let object = value.as_object().expect("BotSummary 必须是对象");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["botId", "name", "platform", "status", "updatedAt"],
        "BotSummary 的键集是契约"
    );
    assert_eq!(
        value,
        json!({
            "botId": BOT_UUID,
            "name": "演示 bot",
            "platform": "onebot11",
            "status": "running",
            "updatedAt": AT,
        })
    );

    // 状态枚举与 `bot.status` 事件共用同一个 `BotStatus`：值必须是 snake_case。
    for (status, wire) in [
        (BotStatus::Stopped, "stopped"),
        (BotStatus::Starting, "starting"),
        (BotStatus::Running, "running"),
        (BotStatus::Stopping, "stopping"),
        (BotStatus::Degraded, "degraded"),
    ] {
        let mut row = summary.clone();
        row.status = status;
        let value = serde_json::to_value(&row).expect("ser");
        assert_eq!(value["status"], json!(wire));
    }

    // 往返 + 无 snake_case 键。
    assert_camel_case_keys(&value, "botSummary");
    let parsed: BotSummary = serde_json::from_value(value).expect("de");
    assert_eq!(parsed, summary);
}
