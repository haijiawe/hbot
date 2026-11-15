//! 命令测试用的夹具：临时 data 目录 + 装配好的 [`AppState`]。
//!
//! 只在测试里编译。刻意不引 `tempfile`：一个临时目录几行就能建，少一个 dev 依赖。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;

use super::AppState;
use super::ids::UuidIds;

/// 自动化进程内的临时目录，`Drop` 时递归删除。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 建一个空目录（名字带进程内自增序号，避免并发测试撞名）。
    pub fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-test-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self { path }
    }

    /// 目录路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 目录路径（拥有所有权，方便传给需要 `PathBuf` 的 API）。
    pub fn to_path_buf(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 建一个装配好的 [`AppState`]：数据目录在临时目录里，时钟真实、ID 随机。
pub fn temp_state() -> (TempDir, AppState) {
    let dir = TempDir::new("state");
    let state = AppState::open(
        dir.path().join("data"),
        Arc::new(SystemClock),
        Arc::new(UuidIds),
    )
    .expect("AppState::open");
    (dir, state)
}

/// 建一个已打开项目的 [`AppState`]：项目根是临时目录下的 `project/`。
///
/// 返回的 `TempDir` 同时包含项目与数据目录，drop 时一起删。
pub fn temp_state_with_project() -> (TempDir, AppState, PathBuf) {
    let dir = TempDir::new("project");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("建项目目录");
    let state = AppState::open(
        dir.path().join("data"),
        Arc::new(SystemClock),
        Arc::new(UuidIds),
    )
    .expect("AppState::open");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime
        .block_on(state.open_project(&project))
        .expect("open_project");
    (dir, state, project)
}
