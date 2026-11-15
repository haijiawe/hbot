//! Agent 命令：`agent_*`、`edit_*`。
//!
//! M1 接 [`hbot_agent`] 的单循环。M0 只留形状：这些命令**必须**在真正能跑之前返回
//! `-32006`，不能假装"已提交"（`AGENTS.md` §0.8）。

use super::unimplemented;
use crate::app::AppState;
use crate::dto::{Command, Response};

/// `agent_send`。
pub async fn send(state: &AppState, session_id: hbot_core::SessionId, text: String) -> Response {
    let _ = (state, session_id, text);
    unimplemented("agent_send")
}

/// `agent_cancel`。
pub async fn cancel(state: &AppState, session_id: hbot_core::SessionId) -> Response {
    let _ = (state, session_id);
    unimplemented("agent_cancel")
}

/// `agent_approve`。
pub async fn approve(
    state: &AppState,
    approval_id: hbot_core::ApprovalId,
    decision: hbot_core::ApprovalDecision,
    always: bool,
) -> Response {
    let _ = (state, approval_id, decision, always);
    unimplemented("agent_approve")
}

/// `edit_apply`。
pub async fn edit_apply(
    state: &AppState,
    edit_id: hbot_core::EditId,
    accept_hunks: Vec<u32>,
) -> Response {
    let _ = (state, edit_id, accept_hunks);
    unimplemented("edit_apply")
}

/// `edit_reject`。
pub async fn edit_reject(state: &AppState, edit_id: hbot_core::EditId) -> Response {
    let _ = (state, edit_id);
    unimplemented("edit_reject")
}

/// 路由入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::AgentSend { session_id, text } => send(state, session_id, text).await,
        Command::AgentCancel { session_id } => cancel(state, session_id).await,
        Command::AgentApprove {
            approval_id,
            decision,
            always,
        } => approve(state, approval_id, decision, always).await,
        Command::EditApply {
            edit_id,
            accept_hunks,
        } => edit_apply(state, edit_id, accept_hunks).await,
        Command::EditReject { edit_id } => edit_reject(state, edit_id).await,
        other => unimplemented(other.kind()),
    }
}
