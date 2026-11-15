//! 调试命令：`debug_*`。
//!
//! 插桩调试（`docs/02-architecture.md` §6.5）在 M3 落地；在那之前这些命令返回
//! `-32006`，UI 据此把断点面板标成"未就绪"，而不是显示一个点了没反应的按钮。

use super::unimplemented;
use crate::app::AppState;
use crate::dto::{Command, Response, StepKind};

/// `debug_set_breakpoints`。
pub async fn set_breakpoints(state: &AppState, file: String, lines: Vec<u32>) -> Response {
    let _ = (state, file, lines);
    unimplemented("debug_set_breakpoints")
}

/// `debug_resume`。
pub async fn resume(state: &AppState, session_id: hbot_core::SessionId) -> Response {
    let _ = (state, session_id);
    unimplemented("debug_resume")
}

/// `debug_step`。
pub async fn step(state: &AppState, session_id: hbot_core::SessionId, kind: StepKind) -> Response {
    let _ = (state, session_id, kind);
    unimplemented("debug_step")
}

/// 路由入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::DebugSetBreakpoints { file, lines } => set_breakpoints(state, file, lines).await,
        Command::DebugResume { session_id } => resume(state, session_id).await,
        Command::DebugStep { session_id, kind } => step(state, session_id, kind).await,
        other => unimplemented(other.kind()),
    }
}
