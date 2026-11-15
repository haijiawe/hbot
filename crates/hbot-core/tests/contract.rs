//! `hbot-core` 契约测试：ID、错误码、时间戳与领域类型的 JSON 形状。
//!
//! 这些断言锁定的是**线上契约**，不是实现细节：改了 JSON 形状或错误码，
//! UI 与已落库数据就会失配，所以这里必须红。

use std::str::FromStr;

use hbot_core::{
    ApprovalDecision, ApprovalId, BotId, BotStatus, ConversationId, ConversationKind, EditId,
    HbotError, InboundId, MessageId, OutboundId, OutboundState, ProjectId, SandboxMode, SearchId,
    SessionId, Timestamp, ToolCall, ToolCallStatus,
};
use serde_json::json;

const PROJECT_UUID: &str = "11111111-1111-4111-8111-111111111111";
const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";
const CONVERSATION_UUID: &str = "33333333-3333-4333-8333-333333333333";
const SESSION_UUID: &str = "44444444-4444-4444-8444-444444444444";
const MESSAGE_UUID: &str = "55555555-5555-4555-8555-555555555555";
const INBOUND_UUID: &str = "66666666-6666-4666-8666-666666666666";
const OUTBOUND_UUID: &str = "77777777-7777-4777-8777-777777777777";
const APPROVAL_UUID: &str = "88888888-8888-4888-8888-888888888888";
const EDIT_UUID: &str = "99999999-9999-4999-8999-999999999999";
const SEARCH_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[test]
fn ids_round_trip_as_plain_strings() {
    let project = ProjectId::from_str(PROJECT_UUID).expect("valid uuid");
    let bot = BotId::from_str(BOT_UUID).expect("valid uuid");
    let conversation = ConversationId::from_str(CONVERSATION_UUID).expect("valid uuid");
    let session = SessionId::from_str(SESSION_UUID).expect("valid uuid");
    let message = MessageId::from_str(MESSAGE_UUID).expect("valid uuid");

    // Display 与线上形状一致（§9.1 里 id 是 String）。
    assert_eq!(project.to_string(), PROJECT_UUID);
    assert_eq!(bot.to_string(), BOT_UUID);
    assert_eq!(conversation.to_string(), CONVERSATION_UUID);
    assert_eq!(session.to_string(), SESSION_UUID);
    assert_eq!(message.to_string(), MESSAGE_UUID);

    // serde 透明：JSON 是字符串，不是 `{"0": "..."}` 或字节数组。
    assert_eq!(
        serde_json::to_value(project).expect("ser"),
        json!(PROJECT_UUID)
    );
    assert_eq!(
        serde_json::to_value(session).expect("ser"),
        json!(SESSION_UUID)
    );

    let parsed: SessionId = serde_json::from_value(json!(SESSION_UUID)).expect("de");
    assert_eq!(parsed, session);
}

#[test]
fn ids_reject_invalid_uuid() {
    assert!(SessionId::from_str("not-a-uuid").is_err());
    assert!(SessionId::from_str("").is_err());
    assert!(serde_json::from_value::<BotId>(json!("not-a-uuid")).is_err());
}

#[test]
fn ids_convert_to_and_from_uuid() {
    let session = SessionId::from_str(SESSION_UUID).expect("valid uuid");
    let uuid = *session.as_uuid();
    assert_eq!(SessionId::from_uuid(uuid), session);

    let via_from: SessionId = uuid.into();
    assert_eq!(via_from, session);

    let back: uuid::Uuid = session.into();
    assert_eq!(back, uuid);
}

#[test]
fn error_codes_are_stable() {
    // 码值是 UI 契约；文档只给了区间，具体值以 errors.rs 为唯一真源。
    let table: [(HbotError, i32); 6] = [
        (HbotError::not_found("project", PROJECT_UUID), -32001),
        (HbotError::invalid("path escapes project root"), -32002),
        (HbotError::conflict("base_hash mismatch"), -32003),
        (HbotError::timeout("run_command", 5_000), -32004),
        (HbotError::denied("sandbox mode read_only"), -32005),
        (HbotError::internal("invariant violated"), -32006),
    ];

    for (error, code) in table {
        assert_eq!(error.code(), code, "{error:?}");
        let message = error.to_string();
        assert!(!message.is_empty(), "错误必须有可展示信息：{error:?}");
        // 脱敏/可读性：消息里不应出现 Rust 的 Debug 痕迹。
        assert!(!message.contains("HbotError"), "{message}");
    }
}

#[test]
fn error_display_carries_context() {
    let not_found = HbotError::not_found("bot", BOT_UUID);
    assert!(not_found.to_string().contains("bot"));
    assert!(not_found.to_string().contains(BOT_UUID));

    let timeout = HbotError::timeout("llm_request", 30_000);
    assert!(timeout.to_string().contains("30000"));
    assert!(timeout.to_string().contains("llm_request"));
}

#[test]
fn timestamp_is_unix_millis_integer() {
    let ts = Timestamp::from_unix_millis(1_700_000_000_000);
    assert_eq!(ts.unix_millis(), 1_700_000_000_000);
    assert_eq!(
        serde_json::to_value(ts).expect("ser"),
        json!(1_700_000_000_000i64)
    );

    let parsed: Timestamp = serde_json::from_value(json!(1_700_000_000_000i64)).expect("de");
    assert_eq!(parsed, ts);

    // 默认值是纪元（0），方便 serde default 与 DB 缺省。
    assert_eq!(Timestamp::default().unix_millis(), 0);
}

#[test]
fn sandbox_mode_defaults_and_string_form() {
    assert_eq!(SandboxMode::default(), SandboxMode::WorkspaceWrite);
    assert_eq!(SandboxMode::ReadOnly.as_str(), "read_only");
    assert_eq!(SandboxMode::WorkspaceWrite.as_str(), "workspace_write");
    assert_eq!(SandboxMode::Danger.as_str(), "danger");

    // 枚举线上是 snake_case，且与 as_str() 一致（DB / 审计 / 审批规则键共用）。
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::Danger,
    ] {
        assert_eq!(
            serde_json::to_value(mode).expect("ser"),
            json!(mode.as_str())
        );
    }
}

#[test]
fn domain_enums_serialize_snake_case() {
    assert_eq!(
        serde_json::to_value(ApprovalDecision::Approve).expect("ser"),
        json!("approve")
    );
    assert_eq!(
        serde_json::to_value(ApprovalDecision::Deny).expect("ser"),
        json!("deny")
    );
    assert_eq!(
        serde_json::to_value(BotStatus::Stopped).expect("ser"),
        json!("stopped")
    );
    assert_eq!(
        serde_json::to_value(ConversationKind::Channel).expect("ser"),
        json!("channel")
    );
    assert_eq!(
        serde_json::to_value(OutboundState::Dead).expect("ser"),
        json!("dead")
    );
    assert_eq!(
        serde_json::to_value(ToolCallStatus::AwaitingApproval).expect("ser"),
        json!("awaiting_approval")
    );
    // 默认值必须是"safest"档：未指定时按 workspace_write，未知会话类型 unknown。
    assert_eq!(ConversationKind::default(), ConversationKind::Unknown);
    assert_eq!(OutboundState::default(), OutboundState::Pending);
    assert_eq!(ToolCallStatus::default(), ToolCallStatus::Pending);
}

#[test]
fn tool_call_uses_camel_case_fields() {
    let session = SessionId::from_str(SESSION_UUID).expect("valid uuid");
    let call = ToolCall {
        id: "call_1".to_string(),
        session_id: session,
        name: "read_file".to_string(),
        args: json!({ "path": "src/main.rs" }),
        result: None,
        status: ToolCallStatus::Running,
        approval: Some(ApprovalDecision::Approve),
        created_at: Timestamp::from_unix_millis(1_700_000_000_000),
        finished_at: None,
    };

    let value = serde_json::to_value(&call).expect("ser");
    let object = value.as_object().expect("object");
    // §9.2 事件载荷字段是 camelCase（ToolCall 会内嵌进 agent.tool.call）。
    for key in [
        "id",
        "sessionId",
        "name",
        "args",
        "result",
        "status",
        "approval",
        "createdAt",
        "finishedAt",
    ] {
        assert!(object.contains_key(key), "缺少字段 {key}：{value}");
    }
    assert!(
        !object.contains_key("session_id"),
        "不应出现 snake_case：{value}"
    );
    assert_eq!(value["status"], json!("running"));
    assert_eq!(value["approval"], json!("approve"));
    assert_eq!(value["createdAt"], json!(1_700_000_000_000i64));

    // 往返：None 字段显式是 null，不是被省略。
    assert!(value["result"].is_null());
    assert!(value["finishedAt"].is_null());
    let parsed: ToolCall = serde_json::from_value(value).expect("de");
    assert_eq!(parsed, call);
}

#[test]
fn message_ids_distinguish_direction() {
    // 入站与出站共用 MessageId 类型，但语义不同；这里只锁 serde 形状。
    let id = MessageId::from_str(MESSAGE_UUID).expect("valid uuid");
    assert_eq!(serde_json::to_value(id).expect("ser"), json!(MESSAGE_UUID));
}

#[test]
fn row_primary_keys_round_trip_like_message_ids() {
    // InboundId / OutboundId 是表行主键类型，线上形状必须与 MessageId 完全一致
    // （字符串 UUID），否则 store 与 RPC 之间要额外转换。
    let inbound = InboundId::from_str(INBOUND_UUID).expect("valid uuid");
    let outbound = OutboundId::from_str(OUTBOUND_UUID).expect("valid uuid");

    assert_eq!(inbound.to_string(), INBOUND_UUID);
    assert_eq!(outbound.to_string(), OUTBOUND_UUID);
    assert_eq!(
        serde_json::to_value(inbound).expect("ser"),
        json!(INBOUND_UUID)
    );
    assert_eq!(
        serde_json::to_value(outbound).expect("ser"),
        json!(OUTBOUND_UUID)
    );

    let inbound_back: InboundId = serde_json::from_value(json!(INBOUND_UUID)).expect("de");
    let outbound_back: OutboundId = serde_json::from_value(json!(OUTBOUND_UUID)).expect("de");
    assert_eq!(inbound_back, inbound);
    assert_eq!(outbound_back, outbound);

    // 非法 UUID 必须报错。
    assert!(InboundId::from_str("not-a-uuid").is_err());
    assert!(OutboundId::from_str("not-a-uuid").is_err());

    // 与 MessageId 同值同形：同一行用任一类型表示，线上字符串相同。
    let message = MessageId::from_str(INBOUND_UUID).expect("valid uuid");
    assert_eq!(message.to_string(), inbound.to_string());
}

#[test]
fn request_ids_round_trip_like_other_ids() {
    // ApprovalId / EditId / SearchId 是"请求-应答"关联键：值本身没有业务含义，
    // 但线上必须是同一个 UUID 字符串，否则 UI 回传时对不上。
    let approval = ApprovalId::from_str(APPROVAL_UUID).expect("valid uuid");
    let edit = EditId::from_str(EDIT_UUID).expect("valid uuid");
    let search = SearchId::from_str(SEARCH_UUID).expect("valid uuid");

    assert_eq!(approval.to_string(), APPROVAL_UUID);
    assert_eq!(edit.to_string(), EDIT_UUID);
    assert_eq!(search.to_string(), SEARCH_UUID);

    assert_eq!(
        serde_json::to_value(approval).expect("ser"),
        json!(APPROVAL_UUID)
    );
    assert_eq!(serde_json::to_value(edit).expect("ser"), json!(EDIT_UUID));
    assert_eq!(
        serde_json::to_value(search).expect("ser"),
        json!(SEARCH_UUID)
    );

    let approval_back: ApprovalId = serde_json::from_value(json!(APPROVAL_UUID)).expect("de");
    let edit_back: EditId = serde_json::from_value(json!(EDIT_UUID)).expect("de");
    let search_back: SearchId = serde_json::from_value(json!(SEARCH_UUID)).expect("de");
    assert_eq!(approval_back, approval);
    assert_eq!(edit_back, edit);
    assert_eq!(search_back, search);

    // 非法 UUID（含旧的手写短 ID）必须报错，不允许退化成字符串直传。
    for bad in ["appr_1", "edit_1", "search_1", "not-a-uuid", ""] {
        assert!(ApprovalId::from_str(bad).is_err(), "应拒绝 {bad:?}");
        assert!(EditId::from_str(bad).is_err(), "应拒绝 {bad:?}");
        assert!(SearchId::from_str(bad).is_err(), "应拒绝 {bad:?}");
    }
}
