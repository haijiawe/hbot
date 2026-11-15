//! 文件与搜索命令：`file_*`、`search_*`、`diagnostics`。
//!
//! 全部经由 [`hbot_workspace::Workspace`]——**不允许**在这里直接 `std::fs`：路径守卫
//! （`docs/02-architecture.md` §11.1）只在 workspace 里有一份，绕过它就等于绕过守卫。

use super::unimplemented;
use crate::app::AppState;
use crate::dto::{Command, Response};

/// `file_read`。
pub async fn read(state: &AppState, path: String) -> Response {
    let _ = (state, path);
    unimplemented("file_read")
}

/// `file_write`。
pub async fn write(
    state: &AppState,
    path: String,
    content: String,
    base_hash: Option<String>,
) -> Response {
    let _ = (state, path, content, base_hash);
    unimplemented("file_write")
}

/// `file_list`。
pub async fn list(state: &AppState, path: String) -> Response {
    let _ = (state, path);
    unimplemented("file_list")
}

/// `file_create`。
pub async fn create(state: &AppState, path: String, content: String) -> Response {
    let _ = (state, path, content);
    unimplemented("file_create")
}

/// `file_rename`。
pub async fn rename(state: &AppState, from: String, to: String) -> Response {
    let _ = (state, from, to);
    unimplemented("file_rename")
}

/// `file_delete`。
pub async fn delete(state: &AppState, path: String, to_trash: bool) -> Response {
    let _ = (state, path, to_trash);
    unimplemented("file_delete")
}

/// `search_start`。
pub async fn search(state: &AppState, query: String, regex: bool, globs: Vec<String>) -> Response {
    let _ = (state, query, regex, globs);
    unimplemented("search_start")
}

/// `search_cancel`。
pub async fn search_cancel(state: &AppState, search_id: hbot_core::SearchId) -> Response {
    let _ = (state, search_id);
    unimplemented("search_cancel")
}

/// `diagnostics`。
pub async fn diagnostics(state: &AppState, path: Option<String>) -> Response {
    let _ = (state, path);
    unimplemented("diagnostics")
}

/// 路由入口：由 [`crate::Dispatcher`] 调用。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::FileRead { path } => read(state, path).await,
        Command::FileWrite {
            path,
            content,
            base_hash,
        } => write(state, path, content, base_hash).await,
        Command::FileList { path } => list(state, path).await,
        Command::FileCreate { path, content } => create(state, path, content).await,
        Command::FileRename { from, to } => rename(state, from, to).await,
        Command::FileDelete { path, to_trash } => delete(state, path, to_trash).await,
        Command::SearchStart {
            query,
            regex,
            globs,
        } => search(state, query, regex, globs).await,
        Command::SearchCancel { search_id } => search_cancel(state, search_id).await,
        Command::Diagnostics { path } => diagnostics(state, path).await,
        other => unimplemented(other.kind()),
    }
}
