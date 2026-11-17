//! 进程树隔离：平台差异全关在这一个模块后面。
//!
//! 对外只有七件事：`create`（建隔离范围，顺带把资源上限设进内核）、`configure`
//! （spawn 前设预执行钩子）、`attach`（把子进程放进范围）、`kill_tree`（杀整棵）、
//! `usage`（读资源用量快照）、`violation`（有没有突破上限）、以及 `probe`（能不能建）。
//! 调用方（[`crate::PlatformRunner`]）不需要知道 Windows 是作业对象、Linux 是会话 + rlimit。
//!
//! 限额的**语义**（哪类是内核硬上限、哪类是轮询探测）写在 [`crate::limits`]；
//! 这里只负责"按平台把它落到实处"。

use crate::child::Spawned;
use crate::limits::LimitViolation;
use hbot_core::{ResourceLimits, ResourceUsage};
use std::io;
use std::process::Command;

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
    ///
    /// `limits` 在这里就落到内核上（Windows 写进作业对象、Linux 记下来等 `configure`
    /// 在子进程里 `setrlimit`）——**不是**等命令跑起来再补。装不上时整条命令不启动。
    pub(crate) fn create(limits: &ResourceLimits) -> Result<Self, String> {
        Ok(Self {
            inner: imp::Containment::create(limits)?,
        })
    }

    /// spawn 之前的平台设置（Linux 挂 `setsid`；Windows 设 `CREATE_NO_WINDOW`）。
    pub(crate) fn configure(&self, command: &mut Command) {
        self.inner.configure(command);
    }

    /// 把刚 spawn 出来的子进程放进隔离范围。
    ///
    /// 失败等于"没隔离"：调用方必须杀掉这个子进程并拒绝执行。
    pub(crate) fn attach(&self, child: &Spawned) -> io::Result<()> {
        self.inner.attach(child)
    }

    /// 杀掉整棵进程树（不是只杀直接子进程）。
    pub(crate) fn kill_tree(&self, child: &mut Spawned) -> io::Result<()> {
        self.inner.kill_tree(child)
    }

    /// 读一次资源用量快照（**整棵树**口径；该平台没测量的项为 0）。
    ///
    /// 必须在 `kill_tree` / `wait` 之后、`Drop`（Windows 关作业对象句柄）之前调用：
    /// 句柄一关，累计记账就没了。
    pub(crate) fn usage(&self) -> ResourceUsage {
        self.inner.usage()
    }

    /// 有没有突破 `limits` 请求的上限（突破 → 调用方杀树并返回对应的
    /// [`hbot_core::CommandDenied`]）。
    ///
    /// Windows 上这是**轮询探测**（作业对象只有记账、没有"超了自动杀"的开关）：
    /// 10 ms 一次的既有循环顺手读记账，所以可能略微超出请求值。
    /// Linux 上恒为 `None`——`setrlimit` 由内核直接作用于进程，没有可轮询的东西
    /// （超了就是 `SIGKILL` / 分配失败，体现在退出码与输出里）。
    pub(crate) fn violation(&self, limits: &ResourceLimits) -> Option<LimitViolation> {
        self.inner.violation(limits)
    }

    /// Linux 专有：进程被 `SIGXCPU` 杀掉时返回请求的 CPU 上限（毫秒），让调用方把
    /// `Exited { signal: 24 }` 翻译成 [`hbot_core::CommandDenied::CpuLimit`] 而不是
    /// 让调用方自己去查信号表。
    ///
    /// 别的平台没有这个方法：Windows 的 CPU 上限是作业对象记账 + 轮询，
    /// 不需要从信号反推。
    #[cfg(target_os = "linux")]
    pub(crate) fn cpu_kill_hint(&self, signal: Option<i32>) -> Option<u64> {
        self.inner.cpu_kill_hint(signal)
    }
}

/// 这一层隔离在本机建得起来吗（逐平台实现在 `imp`）。
pub(crate) fn probe() -> Result<(), String> {
    imp::probe()
}
