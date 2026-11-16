//! 写隔离（`WorkspaceWrite` 档）：命令能跑，但写不出工作区。
//!
//! 与 `Containment`（进程树 + 资源上限）是两层独立的东西，ADR 0003 把它们分开写：
//! - `Containment`：进程树能杀干净、资源有上限 —— 任何档位都装。
//! - `WriteConfinement`：写盘被内核限制在工作区内 —— 只有 `WorkspaceWrite` 档需要，
//!   也是这个档位**不可用时一律拒绝**的原因（fail-closed：装不出隔离就不跑命令）。
//!
//! 平台实现：
//! - Windows：受限令牌 `WRITE_RESTRICTED` + 由工作区路径派生的 write SID + 该目录上的 ACE。
//! - Linux：Landlock（内核 5.13+），规则集在 `pre_exec` 里装。
//! - 其它（含 macOS，`sandbox-exec` 已弃用）：不实现，`create`/`probe` 都返回原因。

use std::path::Path;

#[cfg(windows)]
use std::path::PathBuf;

#[cfg(windows)]
#[path = "confinement_win.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "confinement_linux.rs"]
mod imp;
#[cfg(not(any(windows, target_os = "linux")))]
#[path = "confinement_unsupported.rs"]
mod imp;

#[cfg(windows)]
pub(crate) use crate::child::NativeChild;

/// Windows 原生启动参数：`std::process::Command` 表达不了「用哪个令牌」和句柄白名单。
#[cfg(windows)]
pub(crate) struct SpawnPlan {
    /// 可执行文件的绝对路径（同时给 `lpApplicationName`，绕开「NULL 不搜 PATH」的坑）。
    pub(crate) application: PathBuf,
    /// 参数（不含 argv[0]，那份由 `application` 补）。
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
}

/// 写隔离实例。
pub(crate) struct WriteConfinement {
    inner: imp::WriteConfinement,
}

impl WriteConfinement {
    /// 为工作区建立写隔离。失败即「这台机器（或这个目录）装不出隔离」，调用方必须拒绝执行。
    pub(crate) fn create(workspace_root: &Path) -> Result<Self, String> {
        imp::WriteConfinement::create(workspace_root).map(|inner| Self { inner })
    }

    /// 用受限令牌启动子进程（Windows）。
    #[cfg(windows)]
    pub(crate) fn spawn(&self, plan: &SpawnPlan) -> std::io::Result<NativeChild> {
        self.inner.spawn(plan)
    }

    /// 把写隔离挂到 `Command` 上（Linux：`pre_exec` 里装 Landlock）。
    #[cfg(not(windows))]
    pub(crate) fn configure(&self, command: &mut std::process::Command) {
        self.inner.configure(command);
    }
}

/// 只读探测：这台机器能不能装出写隔离。失败即不可用 —— 调用方必须因此拒绝执行，
/// **不许**降级成"尽力跑一下"（fail-closed，见 ADR 0003）。
pub(crate) fn probe() -> Result<(), String> {
    imp::probe()
}
