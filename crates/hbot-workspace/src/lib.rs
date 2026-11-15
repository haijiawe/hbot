//! 项目文件系统：路径守卫、读写、目录操作与文本搜索。
//!
//! 这一层是工作区的**安全边界**，不包含 UI 或 Agent 决策（见 `docs/03-directory.md` §2.1）。
//!
//! - [`Workspace`]：项目根 + 策略 + 路径守卫；所有文件 API 都是它的方法。
//! - `project`：根解析与路径守卫（拒绝绝对路径、`..`、符号链接逃逸、`.git/`、`.hbot/`）。
//! - `files`：读取、原子写入（同目录 tmp → fsync → rename）、列目录、建目录、
//!   重命名、移入 `.hbot/trash`（不真删）。
//! - `search`：`ignore` + `grep-searcher` 的流式搜索，跳过二进制/非 UTF-8 并计数。
//!
//! 约定：错误统一用 [`hbot_core::HbotError`]；库代码不使用 `unwrap`/`expect`。

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod files;
mod project;
mod search;

#[cfg(test)]
pub(crate) mod test_support;

pub use files::{DirEntryInfo, FileContent, hash_bytes};
pub use project::{MAX_READ_BYTES, Workspace, WorkspaceConfig};
pub use search::{
    DEFAULT_MAX_FILE_SIZE, DEFAULT_MAX_RESULTS, MAX_SEARCH_FILE_BYTES, SearchHit, SearchOptions,
    SearchSummary,
};
