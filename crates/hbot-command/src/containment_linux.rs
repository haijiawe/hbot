//! Linux：`setsid` 建新会话 + 杀进程组。
//!
//! 为什么是会话/进程组而不是 `kill(pid)`：进程组是一棵树的内核侧归属，
//! `kill(-pgid, SIGKILL)` 一次覆盖整组（`cmd → npm → node` 全在内），
//! 比逐个 pid 杀可靠，也不依赖 `/proc` 遍历。
//!
//! 为什么没有 landlock：写限制层还没接（见 ADR-0003）。**探测会如实回答不可用**，
//! 于是 `workspace-write` 档在这台机器上仍然拒绝执行——本原型不假装支持。
//!
//! 不引 `libc`/`nix`：只需要 `setsid` / `setpgid` / `kill` 三个 POSIX 符号，
//! 手写声明即可（`std` 已经把 libc 链进来了）。

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

unsafe extern "C" {
    fn setsid() -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

/// `SIGKILL`。隔离层只做"终止"，不尝试优雅退出：超时之后没有谈判空间。
const SIGKILL: i32 = 9;

/// `ESRCH`（进程组已空）。
const ESRCH: i32 = 3;

/// Linux 上没有需要额外持有的句柄：会话由子进程自己 `setsid` 建，组号等于子进程 pid。
pub(crate) struct Containment;

impl Containment {
    pub(crate) fn create() -> Result<Self, String> {
        // `setsid` 不需要任何前置资源，失败的可能是"当前进程已是组长"——
        // 那在子进程里由 `configure` 的兜底处理，这里没有可预检的东西。
        Ok(Self)
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        // SAFETY: `pre_exec` 的闭包在 fork 之后、exec 之前运行，只允许 async-signal-safe
        // 的调用。`setsid` / `setpgid` 都是纯系统调用，不分配内存、不加锁，符合要求。
        // 放进闭包（而不是父进程里做）是为了让"新会话 + 新进程组"这一步发生在子进程里，
        // 父进程的会话不受影响。
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    // 万一子进程已经是组长（例如 pid 恰好等于继承来的 pgid），
                    // setsid 会 EPERM。此时 pgid 已经等于 pid，`killpg` 照样覆盖整棵树。
                    if setpgid(0, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    pub(crate) fn attach(&self, _child: &Child) -> io::Result<()> {
        // 会话与进程组在 exec 之前就建好了（见 `configure`），没有额外挂载动作。
        Ok(())
    }

    pub(crate) fn kill_tree(&self, child: &mut Child) -> io::Result<()> {
        let pid = child.id() as i32;
        // 负 pid = 对整个进程组发信号；组号等于组长（子进程）的 pid。
        // SAFETY: 纯系统调用，参数是有效 pid 与合法信号号。
        let rc = unsafe { kill(-pid, SIGKILL) };
        let group_result = if rc == -1 {
            let err = io::Error::last_os_error();
            // 组已经不在了（子进程自己退干净）不算失败；其它错误如实返回。
            if err.raw_os_error() == Some(ESRCH) {
                Ok(())
            } else {
                Err(err)
            }
        } else {
            Ok(())
        };
        // 双保险：组长自己也要杀（组为空时上面已 ESRCH）。
        let _ = child.kill();
        group_result
    }
}

pub(crate) fn probe() -> Result<(), String> {
    // `setsid` / `killpg` 是 POSIX 基元，任何能跑 Rust 的 Linux 都有；
    // 真要预检只能 fork 一个探针进程，代价大于收益。这里如实报告"可用"，
    // 失败会在真正 spawn 时以 SpawnFailed 暴露。
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn containment_probe_is_available_on_linux() {
        assert!(super::probe().is_ok());
    }
}
