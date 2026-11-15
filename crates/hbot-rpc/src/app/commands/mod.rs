//! 命令实现，按域分组。
//!
//! [`crate::Dispatcher::dispatch`] 只做一件事：把命令**路由**到这里的某一组。
//! 分组与 `crates/hbot-rpc/src/dto/command.rs` 的变体顺序一致（system → files →
//! agent → bots → debug），新增命令时两边一起改。
//!
//! 为什么分组而不写一个大 `match`：M0 之后每组还要接各自的服务（workspace /
//! agent 会话 / supervisor），塞在一个函数里会让"改文件命令"顺手读到 bot 表。
//!
//! 每个 `handle` 都是 `async fn(&AppState, ...) -> Response`：**永不返回 Err**，
//! 错误一律进 [`Response::Error`]（`AGENTS.md` §4.3）。

pub mod agent;
pub mod bots;
pub mod debug;
pub mod files;
pub mod system;

use crate::dto::{ApiError, Response};
use hbot_core::HbotError;

/// 未实现的命令：`-32006` + `尚未实现：<kind>`。
///
/// 与 [`crate::dispatch`] 里 M0 的口径相同：**不返回假数据**。UI 拿到这个错误应当
/// 显示"能力未装配"，而不是假装成功。
pub fn unimplemented(kind: &str) -> Response {
    Response::error(ApiError {
        code: -32006,
        message: format!("尚未实现：{kind}"),
        data: None,
    })
}

/// 把一个领域错误变成 wire 上的 [`Response`]。
///
/// 唯一错误码真源是 [`HbotError::code`]；这里不自己编数字，也不吞掉 `data`
/// （`not_found` 带 `{entity,id}`、`timeout` 带 `{operation,timeoutMs}`）。
pub fn failed(error: HbotError) -> Response {
    Response::error(ApiError::from(error))
}

/// 成功且无返回数据。
pub fn done() -> Response {
    Response::ok()
}
