//! 测试用的临时项目目录（只在 `cfg(test)` 下编译）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::project::Workspace;

/// 一个临时目录；`Drop` 时删除。用 uuid 命名，避免并行测试互相踩。
pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// 在系统临时目录下建一个 `hbot-ws-test-{tag}-{uuid}` 目录。
    pub(crate) fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("hbot-ws-test-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temporary test directory");
        Self { path }
    }

    /// 临时目录绝对路径。
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 以临时目录为根打开 [`Workspace`]。
    pub(crate) fn workspace(&self) -> Workspace {
        Workspace::open(self.path.clone()).expect("open test workspace")
    }

    /// 写一个文件（按需建父目录），返回绝对路径。
    pub(crate) fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, content).expect("write test file");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // 测试清理失败不应让测试失败；临时目录留在系统 temp 里即可。
        let _ = fs::remove_dir_all(&self.path);
    }
}
