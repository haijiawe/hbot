//! Bot 命令：`bot_start`、`bot_stop`、`bot_inject`、`logs_tail`。
//!
//! 启动要装配三样东西（适配器 / 执行器 / supervisor），所以真正的装配在
//! [`crate::app::runtime`]；这里只做命令层的校验、登记与事件推送。

use super::unimplemented;
use crate::app::AppState;
use crate::dto::{Command, Response};

/// `bot_start`。
pub async fn start(state: &AppState, bot_id: hbot_core::BotId) -> Response {
    let _ = (state, bot_id);
    unimplemented("bot_start")
}

/// `bot_stop`。
pub async fn stop(state: &AppState, bot_id: hbot_core::BotId) -> Response {
    let _ = (state, bot_id);
    unimplemented("bot_stop")
}

/// `bot_inject`。
pub async fn inject(
    state: &AppState,
    bot_id: hbot_core::BotId,
    conversation_id: hbot_core::ConversationId,
    text: String,
) -> Response {
    let _ = (state, bot_id, conversation_id, text);
    unimplemented("bot_inject")
}

/// `logs_tail`。
pub async fn logs_tail(state: &AppState, bot_id: hbot_core::BotId, limit: usize) -> Response {
    let _ = (state, bot_id, limit);
    unimplemented("logs_tail")
}

/// 路由入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::BotStart { bot_id } => start(state, bot_id).await,
        Command::BotStop { bot_id } => stop(state, bot_id).await,
        Command::BotInject {
            bot_id,
            conversation_id,
            text,
        } => inject(state, bot_id, conversation_id, text).await,
        Command::LogsTail { bot_id, limit } => logs_tail(state, bot_id, limit).await,
        other => unimplemented(other.kind()),
    }
}
