//! 资源上限的**跨平台纯逻辑**：合法性校验、单位换算、平台支持矩阵、违约构造。
//!
//! 这里刻意不碰任何系统调用。理由很实在：Linux 的限额实现（`setrlimit` / `getrusage`）
//! 只能交叉编译、本机跑不了，而"0 是不是合法值""毫秒怎么折成秒""哪些平台该 fail-closed"
//! 恰恰是最容易写错的地方。把这些判据抽成纯函数后，`cargo test -p hbot-command` 在
//! Windows 上跑的就是 Linux 分支**同一份**逻辑；平台实现只剩"把算好的数字塞进内核接口"
//! 与"把内核计数折成 [`hbot_core::ResourceUsage`]"两件没有分支的活。
//!
//! 语义分三类（`crates/hbot-command/src/lib.rs` 顶部「资源上限的语义与代价」有完整表）：
//!
//! - **内核硬上限**：进程数（Windows `ActiveProcessLimit`）、内存（Windows job 提交上限 /
//!   Linux `RLIMIT_AS`）、Linux CPU（`RLIMIT_CPU`，秒级）；超了是**内核**拒绝分配或直接杀。
//! - **轮询探测**：Windows 的 CPU 累计时间与 I/O 字节数（作业对象只有记账、没有"超了就杀"
//!   的内核开关），由既有的 10 ms 循环读记账并 `TerminateJobObject`，所以**可能略微超出**。
//! - **做不到**：Linux 的 I/O 上限与按树的进程数上限。请求了它们会得到
//!   [`hbot_core::CommandDenied::IsolationUnavailable`]（见 [`unsupported_reason`]）。

use hbot_core::{CommandDenied, IoDirection, Platform, ResourceLimits, ResourceUsage};

// 下面这几个 Windows 作业对象专有的换算/夹取函数与常量（`MAX_PROCESS_LIMIT`、
// `cpu_ms_to_100ns`、`process_limit_clamp`、`memory_limit_usize`）在 Linux 的 `setrlimit`
// 路径上没有任何调用点：`rlimit` 本身就是 64 位，并发进程数上限在 Linux 上又是直接拒绝。
// 但**判据不许 `#[cfg]` 掉** —— 它们正是最容易写错、又只能靠交叉 check 验证的那几个函数，
// 所以只加 `allow(dead_code)`，不裁剪定义。
#[cfg_attr(not(windows), allow(dead_code))]
/// Windows 作业对象 `ActiveProcessLimit` 的上限。
///
/// 内核把 `0` 当作"不限"（保留值），所以真正生效的区间是 `1..=MAX_PROCESS_LIMIT`；
/// 这个 64 同时是既有的默认值（"够 `npm install` 的子树、但拦住 fork 炸弹"）。
pub(crate) const MAX_PROCESS_LIMIT: u32 = 64;

/// CPU 上限的硬顶：24 小时（毫秒）。
///
/// 不是安全边界，是**溢出闸**：再大就没有意义（同步命令最长只允许 30 分钟墙钟超时），
/// 而 Windows 的 `PerJobUserTimeLimit` 是 100 ns 单位的 `i64`，换算前必须先确认不溢出。
pub(crate) const MAX_CPU_MS: u64 = 24 * 60 * 60 * 1_000;

/// `RLIMIT_CPU` 的硬上限比软上限多留的秒数。
///
/// 软上限到点内核发 `SIGXCPU`（默认动作：终止），硬上限到点发 `SIGKILL`。**两者相等时
/// `SIGKILL` 会抢先**——2026-11-20 的 CI 实测：`cpu_ms = 200` 的死循环得到的是
/// `Exited { code: None, signal: Some(9) }`，`SIGXCPU` 一次都没发出来，于是
/// [`crate::containment::Containment::cpu_kill_hint`] 认不出这次死亡来自 CPU 上限，
/// 调用方看到的是「被信号 9 杀掉的普通退出」而不是 `HBOT_COMMAND_CPU_LIMIT`。
/// 留 1 秒余量后：正常程序在软上限那一刻就被 `SIGXCPU` 收掉；万一它自己处理/忽略了
/// `SIGXCPU`，硬上限也会在 1 秒后兜底，不会变成"无限跑"。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const CPU_HARD_GRACE_SECONDS: u64 = 1;

/// CPU 毫秒 → `setrlimit(RLIMIT_CPU)` 的秒数：**向上取整，最小 1 秒**。
///
/// `RLIMIT_CPU` 的单位是整秒，且 `0` 的含义是"不限"（不是"立刻杀"）——所以
/// `cpu_ms = 1..=1000` 全部折成 `1` 秒。这就是 Linux 上 CPU 上限的真实精度：
/// **秒级、向上取整**，比请求值宽松（请求 100 ms 实际拿到 1 s）。
///
/// 只有 Linux 实现会调用它，但判据本身必须在**所有平台**上可测（Linux 分支本机跑不了，
/// 这条换算是它最容易写错的地方之一），所以不做 `#[cfg]` 隔离。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const fn cpu_seconds_ceiling(cpu_ms: u64) -> u64 {
    let seconds = cpu_ms.saturating_add(999) / 1_000;
    if seconds == 0 { 1 } else { seconds }
}

/// CPU 毫秒 → Windows `PerJobUserTimeLimit` 的 100 ns 单位。
///
/// 返回 `None` 表示换算后超出 `i64`（正常路径到不了：先过 [`validate`] 的 `MAX_CPU_MS`）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn cpu_ms_to_100ns(cpu_ms: u64) -> Option<i64> {
    i64::try_from(cpu_ms.checked_mul(10_000)?).ok()
}

/// 请求的进程数上限 → 实际写进作业对象的值：夹到 `1..=MAX_PROCESS_LIMIT`。
///
/// `0` 在作业对象里是保留值（表示"不限"），原样写进去等于**把限制关掉**，所以必须夹。
/// （不是 `const fn`：`u32::clamp` 在 const 上下文里还没稳定，见 rust-lang#143874。）
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn process_limit_clamp(requested: u32) -> u32 {
    requested.clamp(1, MAX_PROCESS_LIMIT)
}

/// 内存字节数 → `usize`（32 位目标上装不下就饱和到 `usize::MAX`，等于"按能表示的最大值限"）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn memory_limit_usize(memory_bytes: u64) -> usize {
    usize::try_from(memory_bytes).unwrap_or(usize::MAX)
}

/// 限额本身的合法性（**不是**平台支持性——那是 [`unsupported_reason`]）。
///
/// 只拦"写下来就是错的"值：`Some(0)` 一律非法。原因各平台一致且不可绕过——
/// Windows 的 `ActiveProcessLimit = 0` / `PerJobUserTimeLimit = 0` 是"保留/不限"，
/// Linux 的 `RLIMIT_CPU = 0` / `RLIMIT_AS = 0`（后者会让子进程连 exec 都做不到）同理。
/// 想让"不限"，正确写法是 `None`，不是 `Some(0)`。
pub(crate) fn validate(limits: &ResourceLimits) -> Result<(), CommandDenied> {
    if limits.cpu_ms == Some(0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "CPU 上限必须大于 0 毫秒（要“不限”请留空，不要写 0）".to_owned(),
        });
    }
    if let Some(cpu_ms) = limits.cpu_ms
        && cpu_ms > MAX_CPU_MS
    {
        return Err(CommandDenied::InvalidSpec {
            reason: format!(
                "CPU 上限不能超过 {} 毫秒（24 小时；再长请改用后台任务）",
                MAX_CPU_MS
            ),
        });
    }
    if limits.memory_bytes == Some(0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "内存上限必须大于 0 字节（要“不限”请留空，不要写 0）".to_owned(),
        });
    }
    if limits.read_bytes == Some(0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "读取字节上限必须大于 0 字节（要“不限”请留空，不要写 0）".to_owned(),
        });
    }
    if limits.write_bytes == Some(0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "写出字节上限必须大于 0 字节（要“不限”请留空，不要写 0）".to_owned(),
        });
    }
    if limits.process_count == Some(0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "并发进程数上限必须大于 0（0 在作业对象里是保留值，写进去等于关掉限制；\
                     要“不限”请留空，不要写 0）"
                .to_owned(),
        });
    }
    Ok(())
}

/// 一个平台能兑现哪几类限额。
///
/// 这是 [`unsupported_reason`]（fail-closed 检查）与
/// [`crate::CapabilityReport`]（给用户看的能力矩阵）**共用**的唯一一张表：
/// 两处各写一份的话，总有一天会出现"能力面板说支持、实际被拒"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimitSupport {
    /// CPU 时间上限。
    pub cpu: bool,
    /// 内存上限。
    pub memory: bool,
    /// I/O 字节上限（读 / 写）。
    pub io: bool,
    /// 并发进程数上限。
    pub processes: bool,
}

/// 各平台的限额支持矩阵（`true` 的语义与精度见 [`support_summary_zh`]）。
pub(crate) const fn support(platform: Platform) -> LimitSupport {
    match platform {
        // 作业对象：进程数/内存是内核硬上限；CPU 时间有内核上限 + 轮询兜底（为了拿到
        // "为什么被杀"的确切数字）；I/O 只有记账，靠 10 ms 轮询杀树。
        Platform::Windows => LimitSupport {
            cpu: true,
            memory: true,
            io: true,
            processes: true,
        },
        // setrlimit(RLIMIT_CPU / RLIMIT_AS) 随 fork 继承，逐进程生效；I/O 与按树的进程数
        // 需要 cgroup v2（root/委派），不做 —— 见 io_unsupported_reason / process_unsupported_reason。
        Platform::Linux => LimitSupport {
            cpu: true,
            memory: true,
            io: false,
            processes: false,
        },
        // macOS 上连进程树隔离都没有（sandbox-exec 已弃用），资源上限自然也没有。
        Platform::MacOs | Platform::Other => LimitSupport {
            cpu: false,
            memory: false,
            io: false,
            processes: false,
        },
    }
}

/// 请求里**平台兑现不了**的那些限额，拼成一句给人看的原因；全都能兑现就返回 `None`。
///
/// 这是 fail-closed 的入口：调用方明确要了约束，而实现层做不到时必须拒绝
/// （[`hbot_core::CommandDenied::IsolationUnavailable`]）。**静默忽略限额等于骗调用方**——
/// 调用方会以为"最多 100 ms CPU"已经生效，于是拿它当护栏。
pub(crate) fn unsupported_reason(limits: &ResourceLimits, platform: Platform) -> Option<String> {
    let supported = support(platform);
    let mut reasons: Vec<String> = Vec::new();
    if limits.cpu_ms.is_some() && !supported.cpu {
        reasons.push("CPU 时间上限（这个平台没有能按进程树统计 CPU 的机制）".to_owned());
    }
    if limits.memory_bytes.is_some() && !supported.memory {
        reasons.push("内存上限（这个平台没有能按进程树限内存的机制）".to_owned());
    }
    if (limits.read_bytes.is_some() || limits.write_bytes.is_some()) && !supported.io {
        reasons.push(io_unsupported_reason(platform));
    }
    if limits.process_count.is_some() && !supported.processes {
        reasons.push(process_unsupported_reason(platform));
    }
    if reasons.is_empty() {
        return None;
    }
    Some(format!(
        "本次调用请求了这台机器兑现不了的资源上限：{}；限额是调用方明确要的约束，\
         静默忽略等于骗调用方，所以按 fail-closed 拒绝执行",
        reasons.join("；"),
    ))
}

/// I/O 上限在 `platform` 上做不到的具体原因（Linux 的 cgroup v2 需要特权是主要一条）。
fn io_unsupported_reason(platform: Platform) -> String {
    match platform {
        Platform::Linux => "I/O 字节上限（Linux 要 cgroup v2 的 io.max，而它需要 root 或 systemd 委派，\
             本 crate 不假设运行环境有——也用不了 per-process 的替代品：POSIX 没有按进程限 I/O 的 rlimit）"
            .to_owned(),
        Platform::Windows => "I/O 字节上限（这个平台的作业对象没有 I/O 上限开关）".to_owned(),
        other => format!("I/O 字节上限（平台 {other} 上没有命令资源上限的实现）"),
    }
}

/// 进程数上限在 `platform` 上做不到的具体原因。
fn process_unsupported_reason(platform: Platform) -> String {
    match platform {
        // 关键事实：RLIMIT_NPROC 是**按用户（UID）**统计的，不是按进程树。在 pre_exec 里
        // 设它 = 给"跑命令的那个用户"设总进程数上限，会连带限制整个登录会话里的其它进程；
        // 而且同一用户在别的会话里的进程也在计数——既超限不准确，又要误伤别人。
        Platform::Linux => "并发进程数上限（Linux 的 RLIMIT_NPROC 按**用户**统计而不是按进程树，\
             设下去会连带限制整个登录会话；cgroup pids.max 同样需要 root/委派）"
            .to_owned(),
        Platform::Windows => {
            "并发进程数上限（这个平台的作业对象没有 ActiveProcessLimit 可用）".to_owned()
        }
        other => format!("并发进程数上限（平台 {other} 上没有命令资源上限的实现）"),
    }
}

/// 给 [`crate::CapabilityReport::summary_zh`] 用的一句话支持说明（单位与精度都在里面）。
pub(crate) const fn support_summary_zh(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => {
            "CPU 毫秒级（作业对象内核上限 + 10 ms 轮询报告）、内存与并发进程数为内核硬上限、\
             I/O 字节数为 10 ms 轮询（可能略微超出）"
        }
        Platform::Linux => {
            "CPU 仅秒级且**每进程**（RLIMIT_CPU 向上取整，整棵树叠加不封顶）、\
             内存为 RLIMIT_AS 虚拟地址空间；I/O 与并发进程数**不支持**（请求即拒绝）"
        }
        Platform::MacOs | Platform::Other => "**不支持**任何资源上限（请求即拒绝）",
    }
}

/// 一次观测到的限额突破（平台实现把它折成 [`hbot_core::CommandDenied`] 的同名变体）。
///
/// 单独一个enum（而不是直接造 `CommandDenied`）是为了让平台实现保持 `hbot-core` 之外的
/// 最小依赖面，也方便在纯函数里单测"哪一条先触发"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum LimitViolation {
    /// CPU 时间超了。
    Cpu {
        /// 观测用量（毫秒）。
        used_ms: u64,
        /// 上限（毫秒）。
        limit_ms: u64,
    },
    /// 内存峰值到顶。
    Memory {
        /// 上限（字节）。
        limit_bytes: u64,
    },
    /// I/O 传输量超了。
    Io {
        /// 方向。
        direction: IoDirection,
        /// 观测用量（字节）。
        used_bytes: u64,
        /// 上限（字节）。
        limit_bytes: u64,
    },
    /// 进程数到顶。
    Process {
        /// 上限。
        limit: u32,
    },
}

impl LimitViolation {
    /// 折成调用方认识的拒绝原因（中文 `Display` 与错误码都在 `hbot-core` 里）。
    pub(crate) fn into_denied(self) -> CommandDenied {
        match self {
            Self::Cpu { used_ms, limit_ms } => CommandDenied::CpuLimit { used_ms, limit_ms },
            Self::Memory { limit_bytes } => CommandDenied::MemoryLimit { limit_bytes },
            Self::Io {
                direction,
                used_bytes,
                limit_bytes,
            } => CommandDenied::IoLimit {
                direction,
                used_bytes,
                limit_bytes,
            },
            Self::Process { limit } => CommandDenied::ProcessLimit { limit },
        }
    }
}

/// 用一份（**整棵树**口径的）用量快照判断有没有突破上限；没超就 `None`。
///
/// CPU / 内存 / 读写字节用 `>=`（到顶就算到顶）：内存那条尤其必须按"到顶"判——Windows 的
/// job 内存上限不是"超了就杀进程"，而是让**分配失败**；峰值可能正好等于上限而进程还活着。
///
/// **进程数用 `>`**，是唯一的例外，理由见下面那段的注释。
///
/// 判定顺序固定为 CPU → 内存 → 读取 → 写出 → 进程数：同时超了好几项时，先报最先成型的那条，
/// 这样同一份输入永远得到同一个拒绝原因（可测、可复现）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn violation_of(
    usage: &ResourceUsage,
    limits: &ResourceLimits,
) -> Option<LimitViolation> {
    if let Some(limit_ms) = limits.cpu_ms
        && usage.cpu_ms >= limit_ms
    {
        return Some(LimitViolation::Cpu {
            used_ms: usage.cpu_ms,
            limit_ms,
        });
    }
    if let Some(limit_bytes) = limits.memory_bytes
        && usage.peak_memory_bytes >= limit_bytes
    {
        return Some(LimitViolation::Memory { limit_bytes });
    }
    if let Some(limit_bytes) = limits.read_bytes
        && usage.read_bytes >= limit_bytes
    {
        return Some(LimitViolation::Io {
            direction: IoDirection::Read,
            used_bytes: usage.read_bytes,
            limit_bytes,
        });
    }
    if let Some(limit_bytes) = limits.write_bytes
        && usage.write_bytes >= limit_bytes
    {
        return Some(LimitViolation::Io {
            direction: IoDirection::Write,
            used_bytes: usage.write_bytes,
            limit_bytes,
        });
    }
    // 进程数是**唯一**用 `>` 而不是 `>=` 的一条：`process_count = 2` 的含义是
    // "根进程 + 1 个子进程"，此刻 `peak_processes == 2` 是**合法**状态，
    // 内核会在第 3 个进程创建时拒绝。按 `>=` 判会把一棵完全合规的树杀掉，
    // 于是"限制并发进程数"变成"禁止起子进程"。
    if let Some(limit) = limits.process_count
        && usage.peak_processes > process_limit_clamp(limit)
    {
        return Some(LimitViolation::Process { limit });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ResourceLimits {
        ResourceLimits::default()
    }

    #[test]
    fn default_limits_are_no_limits() {
        assert!(limits().is_empty(), "默认必须等于“不额外限制”");
        assert!(
            unsupported_reason(&limits(), Platform::MacOs).is_none(),
            "没请求限额就不该因为平台做不到而拒绝"
        );
    }

    #[test]
    fn cpu_soft_and_hard_limits_are_never_the_same_value() {
        // 软硬上限相等时内核直接 SIGKILL，SIGXCPU 发不出来，`cpu_kill_hint()` 就认不出
        // 「这是被我们设的 CPU 上限杀掉的」（CI 实测 `signal: Some(9)`）。这条钉住换算关系，
        // 让 Linux 分支那个本机跑不了的地方至少有一处可测的判据。
        for cpu_ms in [1_u64, 100, 200, 999, 1_000, 1_001, 2_500, MAX_CPU_MS] {
            let soft = cpu_seconds_ceiling(cpu_ms);
            let hard = soft.saturating_add(CPU_HARD_GRACE_SECONDS);
            assert!(
                hard > soft,
                "cpu_ms={cpu_ms} 的软上限 {soft} 秒与硬上限 {hard} 秒必须错开",
            );
            assert_eq!(hard - soft, CPU_HARD_GRACE_SECONDS);
        }
    }

    #[test]
    fn cpu_ms_rounds_up_to_whole_seconds() {
        // RLIMIT_CPU 只有秒，且 0 表示"不限"，所以最小 1 秒。
        assert_eq!(cpu_seconds_ceiling(0), 1);
        assert_eq!(cpu_seconds_ceiling(1), 1);
        assert_eq!(cpu_seconds_ceiling(999), 1);
        assert_eq!(cpu_seconds_ceiling(1_000), 1);
        assert_eq!(cpu_seconds_ceiling(1_001), 2);
        assert_eq!(cpu_seconds_ceiling(2_500), 3);
        assert_eq!(cpu_seconds_ceiling(MAX_CPU_MS), 86_400);
    }

    #[test]
    fn cpu_ms_maps_to_windows_100ns_ticks() {
        assert_eq!(cpu_ms_to_100ns(0), Some(0));
        assert_eq!(cpu_ms_to_100ns(1), Some(10_000));
        assert_eq!(cpu_ms_to_100ns(100), Some(1_000_000));
        assert_eq!(cpu_ms_to_100ns(MAX_CPU_MS), Some(86_400 * 10_000_000));
        // 溢出必须变成 None 而不是回绕成一个小数（那会把上限变成"立刻杀"）。
        assert_eq!(cpu_ms_to_100ns(u64::MAX), None);
    }

    #[test]
    fn process_limit_is_clamped_into_the_kernel_range() {
        assert_eq!(process_limit_clamp(0), 1, "0 是保留值（不限），必须夹到 1");
        assert_eq!(process_limit_clamp(1), 1);
        assert_eq!(process_limit_clamp(2), 2);
        assert_eq!(process_limit_clamp(64), 64);
        assert_eq!(process_limit_clamp(65), 64);
        assert_eq!(process_limit_clamp(u32::MAX), 64);
    }

    #[test]
    fn memory_limit_saturates_on_narrow_targets() {
        assert_eq!(memory_limit_usize(64 * 1024 * 1024), 64 * 1024 * 1024);
        assert_eq!(memory_limit_usize(u64::MAX), usize::MAX);
    }

    #[test]
    fn zero_is_never_a_valid_limit() {
        let cases: [fn(&mut ResourceLimits); 5] = [
            |l| l.cpu_ms = Some(0),
            |l| l.memory_bytes = Some(0),
            |l| l.read_bytes = Some(0),
            |l| l.write_bytes = Some(0),
            |l| l.process_count = Some(0),
        ];
        for case in cases {
            let mut limits = limits();
            case(&mut limits);
            let denied = validate(&limits).expect_err("Some(0) 必须被拒");
            assert!(
                matches!(denied, CommandDenied::InvalidSpec { .. }),
                "{denied:?}"
            );
            assert!(
                denied.to_string().contains("留空"),
                "拒绝消息要告诉调用方正确写法：{denied}"
            );
        }
    }

    #[test]
    fn cpu_limit_over_the_ceiling_is_invalid() {
        let limits = ResourceLimits {
            cpu_ms: Some(MAX_CPU_MS + 1),
            ..limits()
        };
        assert!(matches!(
            validate(&limits),
            Err(CommandDenied::InvalidSpec { .. })
        ));
    }

    #[test]
    fn valid_limits_pass_validation() {
        let limits = ResourceLimits {
            cpu_ms: Some(100),
            memory_bytes: Some(64 * 1024 * 1024),
            read_bytes: Some(1024),
            write_bytes: Some(2048),
            process_count: Some(2),
        };
        assert_eq!(validate(&limits), Ok(()));
    }

    #[test]
    fn support_matrix_matches_the_documented_reality() {
        let windows = support(Platform::Windows);
        assert!(windows.cpu && windows.memory && windows.io && windows.processes);

        let linux = support(Platform::Linux);
        assert!(linux.cpu, "Linux 用 RLIMIT_CPU（秒级、每进程）");
        assert!(linux.memory, "Linux 用 RLIMIT_AS");
        assert!(!linux.io, "Linux 的 io.max 需要 cgroup v2 + 特权，不做");
        assert!(!linux.processes, "RLIMIT_NPROC 按用户统计，不能用来限树");

        for platform in [Platform::MacOs, Platform::Other] {
            let other = support(platform);
            assert!(!other.cpu && !other.memory && !other.io && !other.processes);
        }
    }

    #[test]
    fn io_and_process_limits_fail_closed_on_linux() {
        let io = ResourceLimits {
            read_bytes: Some(1024),
            ..limits()
        };
        let reason = unsupported_reason(&io, Platform::Linux).expect("Linux 上读限额必须被拒");
        assert!(reason.contains("cgroup v2"), "{reason}");
        assert!(reason.contains("io.max"), "{reason}");

        let processes = ResourceLimits {
            process_count: Some(2),
            ..limits()
        };
        let reason =
            unsupported_reason(&processes, Platform::Linux).expect("Linux 上进程数必须被拒");
        assert!(reason.contains("RLIMIT_NPROC"), "{reason}");
        assert!(reason.contains("用户"), "{reason}");

        // CPU 与内存是支持的，不许跟着 I/O 一起被拒。
        assert!(
            unsupported_reason(
                &ResourceLimits {
                    cpu_ms: Some(100),
                    memory_bytes: Some(64 * 1024 * 1024),
                    ..limits()
                },
                Platform::Linux
            )
            .is_none()
        );
    }

    #[test]
    fn every_limit_fails_closed_on_platforms_without_support() {
        let limits = ResourceLimits {
            cpu_ms: Some(100),
            memory_bytes: Some(1024),
            read_bytes: Some(1024),
            write_bytes: Some(1024),
            process_count: Some(2),
        };
        for platform in [Platform::MacOs, Platform::Other] {
            let reason = unsupported_reason(&limits, platform).expect("没有实现的平台必须拒");
            for needle in ["CPU", "内存", "I/O", "进程数"] {
                assert!(
                    reason.contains(needle),
                    "{platform} 的原因缺 {needle}：{reason}"
                );
            }
        }
    }

    #[test]
    fn windows_supports_every_limit() {
        let limits = ResourceLimits {
            cpu_ms: Some(100),
            memory_bytes: Some(1024),
            read_bytes: Some(1024),
            write_bytes: Some(1024),
            process_count: Some(2),
        };
        assert_eq!(unsupported_reason(&limits, Platform::Windows), None);
    }

    #[test]
    fn violations_are_reported_in_a_stable_order() {
        let limits = ResourceLimits {
            cpu_ms: Some(100),
            memory_bytes: Some(1024),
            read_bytes: Some(10),
            write_bytes: Some(10),
            process_count: Some(2),
        };
        let all_over = ResourceUsage {
            cpu_ms: 500,
            peak_memory_bytes: 4096,
            read_bytes: 99,
            write_bytes: 99,
            peak_processes: 8,
        };
        assert_eq!(
            violation_of(&all_over, &limits),
            Some(LimitViolation::Cpu {
                used_ms: 500,
                limit_ms: 100
            })
        );

        // CPU 达标（未到顶）时轮到内存；`>=` 判据：到顶就算到顶。
        let memory_at_ceiling = ResourceUsage {
            cpu_ms: 99,
            peak_memory_bytes: 1024,
            ..all_over
        };
        assert_eq!(
            violation_of(&memory_at_ceiling, &limits),
            Some(LimitViolation::Memory { limit_bytes: 1024 })
        );

        // 只有 I/O 超：方向要分清。
        let io_over = ResourceUsage {
            cpu_ms: 1,
            peak_memory_bytes: 1,
            read_bytes: 10,
            write_bytes: 0,
            peak_processes: 1,
        };
        assert_eq!(
            violation_of(&io_over, &limits),
            Some(LimitViolation::Io {
                direction: IoDirection::Read,
                used_bytes: 10,
                limit_bytes: 10
            })
        );

        // 只有进程数超（I/O 归零，否则会先命中读取那一条——判据顺序是固定的）。
        let too_many = ResourceUsage {
            cpu_ms: 1,
            peak_memory_bytes: 1,
            read_bytes: 0,
            write_bytes: 0,
            peak_processes: 3,
        };
        assert!(matches!(
            violation_of(&too_many, &limits),
            Some(LimitViolation::Process { limit: 2 })
        ));

        // 全都没到顶。
        assert_eq!(
            violation_of(
                &ResourceUsage {
                    cpu_ms: 1,
                    peak_memory_bytes: 1,
                    read_bytes: 1,
                    write_bytes: 1,
                    peak_processes: 1,
                },
                &limits
            ),
            None
        );
    }

    #[test]
    fn reaching_the_process_limit_is_legal() {
        // `process_count = 2` 是"根进程 + 1 个子进程"：此刻正好 2 个**必须放行**。
        // 按 `>=` 判会把合规的树杀掉，等于把"限制并发"变成"禁止 spawn"。
        let two = ResourceLimits {
            process_count: Some(2),
            ..limits()
        };
        assert_eq!(
            violation_of(
                &ResourceUsage {
                    peak_processes: 2,
                    ..ResourceUsage::default()
                },
                &two
            ),
            None
        );
        // 第 3 个才是超。
        assert_eq!(
            violation_of(
                &ResourceUsage {
                    peak_processes: 3,
                    ..ResourceUsage::default()
                },
                &two
            ),
            Some(LimitViolation::Process { limit: 2 })
        );
        // 请求 100 会被夹到 64：第 65 个才算超（不是第 101 个）。
        let clamped = ResourceLimits {
            process_count: Some(100),
            ..limits()
        };
        assert_eq!(
            violation_of(
                &ResourceUsage {
                    peak_processes: 64,
                    ..ResourceUsage::default()
                },
                &clamped
            ),
            None
        );
        assert!(matches!(
            violation_of(
                &ResourceUsage {
                    peak_processes: 65,
                    ..ResourceUsage::default()
                },
                &clamped
            ),
            Some(LimitViolation::Process { .. })
        ));
    }

    #[test]
    fn violations_become_denials_with_the_documented_error_codes() {
        assert_eq!(
            LimitViolation::Cpu {
                used_ms: 500,
                limit_ms: 100
            }
            .into_denied()
            .code(),
            "HBOT_COMMAND_CPU_LIMIT"
        );
        assert_eq!(
            LimitViolation::Memory { limit_bytes: 1024 }
                .into_denied()
                .code(),
            "HBOT_COMMAND_MEMORY_LIMIT"
        );
        assert_eq!(
            LimitViolation::Io {
                direction: IoDirection::Write,
                used_bytes: 4096,
                limit_bytes: 1024
            }
            .into_denied()
            .code(),
            "HBOT_COMMAND_IO_LIMIT"
        );
        assert_eq!(
            LimitViolation::Process { limit: 2 }.into_denied().code(),
            "HBOT_COMMAND_PROCESS_LIMIT"
        );
    }

    #[test]
    fn denial_messages_state_which_limit_and_by_how_much() {
        let cpu = LimitViolation::Cpu {
            used_ms: 1_500,
            limit_ms: 100,
        }
        .into_denied()
        .to_string();
        assert!(cpu.contains("1500") && cpu.contains("100"), "{cpu}");
        assert!(cpu.contains("超出 1400"), "要说清超了多少：{cpu}");
        assert!(cpu.contains("进程树"), "要说明整棵树已被杀：{cpu}");

        let io = LimitViolation::Io {
            direction: IoDirection::Read,
            used_bytes: 8_192,
            limit_bytes: 1_024,
        }
        .into_denied()
        .to_string();
        assert!(io.contains("读取"), "{io}");
        assert!(io.contains("8192") && io.contains("1024"), "{io}");
        assert!(io.contains("超出 7168"), "{io}");

        let memory = LimitViolation::Memory {
            limit_bytes: 64 * 1024 * 1024,
        }
        .into_denied()
        .to_string();
        assert!(memory.contains("67108864"), "{memory}");
        assert!(memory.contains("64 MiB"), "{memory}");

        let process = LimitViolation::Process { limit: 2 }
            .into_denied()
            .to_string();
        assert!(process.contains('2'), "{process}");
        assert!(process.contains("进程数"), "{process}");
    }

    #[test]
    fn support_summary_mentions_the_real_precision_and_units() {
        let windows = support_summary_zh(Platform::Windows);
        assert!(windows.contains("10 ms"), "{windows}");
        assert!(windows.contains("硬上限"), "{windows}");

        let linux = support_summary_zh(Platform::Linux);
        assert!(linux.contains("秒级"), "{linux}");
        assert!(linux.contains("每进程"), "{linux}");
        assert!(linux.contains("不支持"), "{linux}");

        let other = support_summary_zh(Platform::MacOs);
        assert!(other.contains("不支持"), "{other}");
    }
}
