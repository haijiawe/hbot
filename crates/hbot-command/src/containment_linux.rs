//! Linux：`setsid` 建新会话 + 杀进程组，外加 `setrlimit` 做 CPU / 地址空间上限。
//!
//! 为什么是会话/进程组而不是 `kill(pid)`：进程组是一棵树的内核侧归属，
//! `kill(-pgid, SIGKILL)` 一次覆盖整组（`cmd → npm → node` 全在内），
//! 比逐个 pid 杀可靠，也不依赖 `/proc` 遍历。
//!
//! 为什么没有 landlock：写限制层在 [`crate::confinement`]（同一套 `pre_exec` 里装，
//! 但那是另一层判断，两层各管各的：这里只管"杀得干净"）。
//!
//! 不引 `nix` 只为几个 POSIX 符号：手写声明即可（`std` 已经把 libc 链进来了）。
//! 写隔离那一层同样手写声明（`syscall` / `prctl` / `open` / `fcntl`），见
//! `confinement_linux.rs` —— 两个文件都不引 `libc` crate。
//!
//! # 资源上限在 Linux 上的真实能力（**比 Windows 弱，这里如实写**）
//!
//! | 限额 | 落点 | 单位 | 口径 | 精度 |
//! |---|---|---|---|---|
//! | `cpu_ms` | `setrlimit(RLIMIT_CPU)` | **秒**（毫秒向上取整） | **每个进程**，不是整棵树 | 秒级：`100 ms` 实际等于 `1 s`，`1500 ms` 等于 `2 s` |
//! | `memory_bytes` | `setrlimit(RLIMIT_AS)` | 字节 | **每个进程**的**虚拟地址空间**（不是 RSS、不是整棵树） | 内核硬上限：`mmap`/`brk` 直接失败 |
//! | `read_bytes` / `write_bytes` | 无 | — | — | **做不到**：`io.max` 需要 cgroup v2 + root/委派，`rlimit` 没有对应项 |
//! | `process_count` | 无 | — | — | **做不到**：`RLIMIT_NPROC` 是**每用户**（整个登录会话）而不是每棵树 |
//!
//! 做不到的三项由 [`crate::limits::unsupported_reason`] 在 `require_isolation` 阶段
//! 直接拒绝（fail-closed），绝不静默忽略。
//!
//! `usage()` 里的用量来自 `getrusage(RUSAGE_CHILDREN)`，有两条必须知道的失真：
//! ① 它统计的是**本进程**所有已 `wait` 的子进程（`pre_exec` 里的 `setrlimit` 是每进程的，
//! 这里也只能按子进程口径），所以 `cpu_ms` 用"开始时基线"做差来近似本命令；
//! 同一进程里有并发命令时会互相串味（本仓库的 runner 一次只跑一条，测试进程里并发跑时
//! 这个数会偏大）。② 孙进程只有被它的父进程 `wait` 过才会计入，被我们 `SIGKILL` 掉的
//! 子树里那些被 init 收养的部分不计入。

use crate::child::Spawned;
use crate::limits::{self, LimitViolation};
use hbot_core::{ResourceLimits, ResourceUsage};
use std::ffi::c_long;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

unsafe extern "C" {
    fn setsid() -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

/// `SIGKILL`。隔离层只做"终止"，不尝试优雅退出：超时之后没有谈判空间。
const SIGKILL: i32 = 9;

/// `SIGXCPU`：软 `RLIMIT_CPU` 到期时内核发的信号（默认动作是终止 + core）。
const SIGXCPU: i32 = 24;

/// `ESRCH`（进程组已空）。
const ESRCH: i32 = 3;

/// Linux `RLIMIT_CPU`（x86、x86_64、aarch64 上都是 0；MIPS/SPARC 另有编号，不在支持范围）。
const RLIMIT_CPU: i32 = 0;

/// Linux `RLIMIT_AS`（地址空间字节数）。
const RLIMIT_AS: i32 = 9;

/// `RUSAGE_CHILDREN`：只算已经 `wait` 过的子进程（及其被 wait 过的后代）。
const RUSAGE_CHILDREN: i32 = -1;

/// `struct rlimit`：`rlim_t` 在 Linux 上是 `u64`（32 位 glibc 也是）。
#[repr(C)]
struct RLimit {
    /// 软上限（超了发 `SIGXCPU`）。
    rlim_cur: u64,
    /// 硬上限（软上限不能超过它；超了发 `SIGKILL`）。
    rlim_max: u64,
}

/// `struct timeval`（与 `long` 等宽，所以用 `c_long` 保持 32/64 位都对）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimeVal {
    /// 秒。
    tv_sec: c_long,
    /// 微秒。
    tv_usec: c_long,
}

/// `struct rusage`（Linux 布局：2 个 `timeval` + 14 个 `long`）。
///
/// 只用到 `ru_utime` / `ru_stime` / `ru_maxrss`，但中间那些字段**必须原样占位**，
/// 否则后面的字段全部错位——所以整体 `#[allow(dead_code)]` 而不是删字段。
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct RUsage {
    /// 用户态时间。
    ru_utime: TimeVal,
    /// 内核态时间。
    ru_stime: TimeVal,
    /// 峰值常驻集，**单位 KB**（Linux 特有；别的 Unix 是页/字节）。
    ru_maxrss: c_long,
    /// 共享内存大小（未用，占位）。
    ru_ixrss: c_long,
    /// 非共享数据大小（未用，占位）。
    ru_idrss: c_long,
    /// 非共享栈大小（未用，占位）。
    ru_isrss: c_long,
    /// 轻缺页（未用，占位）。
    ru_minflt: c_long,
    /// 重缺页（未用，占位）。
    ru_majflt: c_long,
    /// 换出次数（未用，占位）。
    ru_nswap: c_long,
    /// 块输入次数（未用，占位）。
    ru_inblock: c_long,
    /// 块输出次数（未用，占位）。
    ru_oublock: c_long,
    /// 发出消息数（未用，占位）。
    ru_msgsnd: c_long,
    /// 收到消息数（未用，占位）。
    ru_msgrcv: c_long,
    /// 收到的信号数（未用，占位）。
    ru_nsignals: c_long,
    /// 主动上下文切换（未用，占位）。
    ru_nvcsw: c_long,
    /// 被动上下文切换（未用，占位）。
    ru_nivcsw: c_long,
}

impl RUsage {
    /// 用户态 + 内核态的 CPU 微秒数（负数按 0 处理，宁小勿大）。
    fn cpu_micros(&self) -> u64 {
        micros(self.ru_utime).saturating_add(micros(self.ru_stime))
    }

    /// 峰值常驻集字节数（`ru_maxrss` 是 KB）。
    fn peak_rss_bytes(&self) -> u64 {
        u64::try_from(self.ru_maxrss)
            .unwrap_or(0)
            .saturating_mul(1024)
    }
}

/// `timeval` → 微秒。
fn micros(part: TimeVal) -> u64 {
    let secs = u64::try_from(part.tv_sec).unwrap_or(0);
    let usecs = u64::try_from(part.tv_usec).unwrap_or(0);
    secs.saturating_mul(1_000_000).saturating_add(usecs)
}

/// 采样一次 `RUSAGE_CHILDREN`；失败返回全 0（只会发生在内核不给这个 who 的极端情形）。
fn sample_children() -> RUsage {
    let mut usage = RUsage::default();
    // SAFETY: `usage` 是可写的 `struct rusage`，长度与内核期望一致。
    let rc = unsafe { getrusage(RUSAGE_CHILDREN, &mut usage) };
    if rc == -1 {
        return RUsage::default();
    }
    usage
}

/// 在子进程里装一条 `rlimit`。**只能在 `pre_exec` 里调用**（fork 之后、exec 之前）。
///
/// `setrlimit` 是 async-signal-safe 的纯系统调用，满足 `pre_exec` 的约束。
///
/// `soft` 与 `hard` 必须分开传：`RLIMIT_CPU` 的软上限到点发 `SIGXCPU`，硬上限到点发
/// `SIGKILL`。两者设成**同一个值**时内核在同一个 tick 里既到软上限又到硬上限，实测结果是
/// 直接 `SIGKILL`（`Exited { signal: 9 }`）——`SIGXCPU` 永远发不出来，`cpu_kill_hint()`
/// 也就永远认不出「这是被我们设的 CPU 上限杀掉的」，调用方只会看到一条普通的被信号 9 杀死。
/// 所以 CPU 那条的硬上限要留出 [`limits::CPU_HARD_GRACE_SECONDS`] 的余量（见 `configure`）。
fn apply_limit(resource: i32, soft: u64, hard: u64) -> io::Result<()> {
    let limit = RLimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: 纯系统调用；`limit` 在调用期间存活。
    if unsafe { setrlimit(resource, &limit) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct Containment {
    /// `create` 时拿到的子进程 CPU 基线（`RUSAGE_CHILDREN` 是累计值，必须做差）。
    baseline: RUsage,
    /// 请求的 CPU 上限（毫秒）；`usage()` 与 `cpu_kill_hint()` 用得到。
    cpu_ms: Option<u64>,
    /// 请求的地址空间上限（字节）。
    memory_bytes: Option<u64>,
}

impl Containment {
    pub(crate) fn create(limits: &ResourceLimits) -> Result<Self, String> {
        // `RLIMIT_AS` 设小了会连进程自己的栈/加载器都放不下，子进程 exec 直接失败
        // （表现为 `SpawnFailed`）。这是内核行为，不是这里的 bug；`limits::validate`
        // 只挡 0，没挡"太小"，因为多小算太小取决于程序。
        Ok(Self {
            baseline: sample_children(),
            cpu_ms: limits.cpu_ms,
            memory_bytes: limits.memory_bytes,
        })
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        // 毫秒 → 秒，向上取整（`100 ms` 变成 `1 s`：**精度就到秒**，如实写在
        // `limits::support_summary_zh` 与 lib.rs 的能力矩阵里）。
        let cpu_seconds = self.cpu_ms.map(limits::cpu_seconds_ceiling);
        // `rlim_t` 在 Linux 上就是 `u64`，与 `ResourceLimits::memory_bytes` 同一单位（字节），
        // 不需要中间换算——少一步换算就少一处溢出可能。
        let address_space = self.memory_bytes;
        // SAFETY: `pre_exec` 的闭包在 fork 之后、exec 之前运行，只允许 async-signal-safe
        // 的调用。`setsid` / `setpgid` / `setrlimit` 都是纯系统调用，不分配内存、不加锁，
        // 符合要求。放进闭包（而不是父进程里做）是为了让"新会话 + 新进程组 + 上限"这几步
        // 发生在子进程里，父进程的会话与限额不受影响。
        unsafe {
            command.pre_exec(move || {
                // 先设上限再 exec：exec 之后就没有"趁程序还没起来先超限"的窗口。
                // CPU 的软/硬上限**必须错开**：相等时内核直接 `SIGKILL`，`SIGXCPU` 发不出来，
                // 于是 `cpu_kill_hint()` 认不出这次死亡是我们设的上限造成的（见 `apply_limit`）。
                if let Some(seconds) = cpu_seconds {
                    apply_limit(
                        RLIMIT_CPU,
                        seconds,
                        seconds.saturating_add(limits::CPU_HARD_GRACE_SECONDS),
                    )?;
                }
                // 地址空间没有信号这一层（超了是 `mmap`/`brk` 直接失败），软硬同值即可。
                if let Some(bytes) = address_space {
                    apply_limit(RLIMIT_AS, bytes, bytes)?;
                }
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

    pub(crate) fn attach(&self, _child: &Spawned) -> io::Result<()> {
        // 会话与进程组在 exec 之前就建好了（见 `configure`），没有额外挂载动作。
        Ok(())
    }

    pub(crate) fn kill_tree(&self, child: &mut Spawned) -> io::Result<()> {
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

    /// 本命令的用量（**必须在 `child.wait()` 之后调用**，否则子进程还没被回收，
    /// `RUSAGE_CHILDREN` 里没有它，读出来是 0）。
    pub(crate) fn usage(&self) -> ResourceUsage {
        let now = sample_children();
        ResourceUsage {
            // 累计值做差；`RUSAGE_CHILDREN` 里还有别的命令时这个数会偏大（见模块头）。
            cpu_ms: now.cpu_micros().saturating_sub(self.baseline.cpu_micros()) / 1_000,
            // `ru_maxrss` 是"所有已 wait 子进程里的最大值"，无法做差，所以它是
            // 本命令峰值内存的**上界**（同一进程里跑过更吃内存的命令时它会更高）。
            peak_memory_bytes: now.peak_rss_bytes(),
            // 下面三项 Linux 侧没有可读的内核记账（见模块头的表格），如实报 0
            // （`ResourceUsage` 的文档写明 "0 = 未测量"）。
            read_bytes: 0,
            write_bytes: 0,
            peak_processes: 0,
        }
    }

    /// Linux 不做轮询判定：CPU 与地址空间都是内核 `rlimit` 直接生效。
    pub(crate) fn violation(&self, _limits: &ResourceLimits) -> Option<LimitViolation> {
        None
    }

    /// 进程是被 `SIGXCPU` 杀掉的时候，把"其实是被 CPU 上限杀掉的"这件事说清楚。
    ///
    /// 不做这一步的后果是：调用方只看到 `Exited { signal: 24 }`，得自己去查信号表才知道
    /// 撞的是我们设的上限，而错误码本该是 `HBOT_COMMAND_CPU_LIMIT`。
    pub(crate) fn cpu_kill_hint(&self, signal: Option<i32>) -> Option<u64> {
        let cpu_ms = self.cpu_ms?;
        if signal == Some(SIGXCPU) {
            return Some(cpu_ms);
        }
        None
    }
}

pub(crate) fn probe() -> Result<(), String> {
    // `setsid` / `killpg` / `setrlimit` 是 POSIX 基元，任何能跑 Rust 的 Linux 都有；
    // 真要预检只能 fork 一个探针进程，代价大于收益。这里如实报告"可用"，
    // 失败会在真正 spawn 时以 SpawnFailed 暴露。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rusage_layout_matches_the_kernel_expectation() {
        // 2 个 timeval + 14 个 long。算错一个字段，`ru_maxrss` 就会读到垃圾。
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<RUsage>(), 144);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<RUsage>(), 72);
        assert_eq!(std::mem::size_of::<RLimit>(), 16);
    }

    #[test]
    fn containment_probe_is_available_on_linux() {
        assert!(super::probe().is_ok());
    }

    #[test]
    fn usage_without_a_child_is_all_zero_and_never_panics() {
        // 没起过子进程（或只起过不耗时的）时，差值应当很小；这里只钉"不 panic 且不溢出"。
        let guard = Containment::create(&ResourceLimits::default()).expect("create 不会失败");
        let usage = guard.usage();
        // 测试进程里可能已经跑过别的子进程，所以不断言 0，只断言"读到了数"。
        assert!(
            usage.cpu_ms < 60_000,
            "不该把别的子进程算成几十秒：{usage:?}"
        );
        assert_eq!(usage.read_bytes, 0, "Linux 侧没有 I/O 记账");
        assert_eq!(usage.write_bytes, 0);
        assert_eq!(usage.peak_processes, 0);
    }

    #[test]
    fn cpu_kill_hint_only_fires_for_sigxcpu() {
        let guard = Containment::create(&ResourceLimits {
            cpu_ms: Some(100),
            ..ResourceLimits::default()
        })
        .expect("create 不会失败");
        assert_eq!(guard.cpu_kill_hint(Some(SIGXCPU)), Some(100));
        assert_eq!(
            guard.cpu_kill_hint(Some(SIGKILL)),
            None,
            "SIGKILL 不是 CPU 上限的签名"
        );
        assert_eq!(guard.cpu_kill_hint(None), None);

        let bare = Containment::create(&ResourceLimits::default()).expect("create 不会失败");
        assert_eq!(
            bare.cpu_kill_hint(Some(SIGXCPU)),
            None,
            "没请求 CPU 上限时，SIGXCPU 来自别处的 rlimit，不能冒充我们的上限"
        );
    }

    #[test]
    fn violation_is_always_none_on_linux() {
        // 内核判决，没有轮询"事后发现"这一层。
        let guard = Containment::create(&ResourceLimits::default()).expect("create 不会失败");
        assert_eq!(guard.violation(&ResourceLimits::default()), None);
    }
}
