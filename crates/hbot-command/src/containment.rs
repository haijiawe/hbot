//! 进程树隔离：平台差异全关在这一个模块后面。
//!
//! 对外只有五件事：`create`（建隔离范围）、`configure`（spawn 前设预执行钩子）、
//! `attach`（把子进程放进范围）、`kill_tree`（杀整棵）、以及 `probe`（能不能建）。
//! 调用方（[`crate::PlatformRunner`]）不需要知道 Windows 是作业对象、Linux 是会话。

use std::io;
use std::process::{Child, Command};

#[cfg(windows)]
#[path = "containment_win.rs"]
mod imp;

#[cfg(target_os = "linux")]
#[path = "containment_linux.rs"]
mod imp;

#[cfg(not(any(windows, target_os = "linux")))]
#[path = "containment_unsupported.rs"]
mod imp;

/// 一个隔离范围（Windows 上是作业对象句柄；Linux 上没有句柄，靠 `setsid` 建的会话）。
///
/// `Drop` 有副作用：Windows 上关句柄会**连带杀掉**范围内还活着的进程
/// （`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）——这是"不留孤儿"的最后一道保险，
/// 不是资源清理。
pub(crate) struct Containment {
    inner: imp::Containment,
}

impl Containment {
    /// 建一个隔离范围；建不出来就返回原因（调用方转成
    /// [`hbot_core::CommandDenied::IsolationUnavailable`]，**绝不降级放行**）。
    pub(crate) fn create() -> Result<Self, String> {
        Ok(Self {
            inner: imp::Containment::create()?,
        })
    }

    /// spawn 之前的平台设置（Linux 挂 `setsid`；Windows 设 `CREATE_NO_WINDOW`）。
    pub(crate) fn configure(&self, command: &mut Command) {
        self.inner.configure(command);
    }

    /// 把刚 spawn 出来的子进程放进隔离范围。
    ///
    /// 失败等于"没隔离"：调用方必须杀掉这个子进程并拒绝执行。
    pub(crate) fn attach(&self, child: &Child) -> io::Result<()> {
        self.inner.attach(child)
    }

    /// 杀掉整棵进程树（不是只杀直接子进程）。
    pub(crate) fn kill_tree(&self, child: &mut Child) -> io::Result<()> {
        self.inner.kill_tree(child)
    }
}

/// 这一层隔离在本机建得起来吗（逐平台实现在 `imp`）。
pub(crate) fn probe() -> Result<(), String> {
    imp::probe()
}
