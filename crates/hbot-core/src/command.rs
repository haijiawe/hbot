//! 命令执行的契约类型：[`crate::ports::CommandRunner`] 的入参、出参与拒绝原因。
//!
//! 为什么放在 `hbot-core` 而不是实现层：调用方（Agent 工具、后台任务、子代理）
//! 只应该认识这份契约；平台实现（作业对象 / 进程组 / 后面可能加的受限令牌）
//! 是可换的实现细节，见 `docs/adr/0003-command-isolation.md`。
//!
//! 设计约束（与 `docs/02-architecture.md` §11.2、`AGENTS.md` §4.4 对齐）：
//!
//! - 命令用 **argv 数组**表达，没有 shell 字符串，也没有 `sh -c`；
//! - 隔离装不上就拒绝（[`CommandDenied::IsolationUnavailable`]），**不回退名字白名单**；
//! - 拒绝原因必须能直接展示给用户（中文一句话，可操作），所以 [`CommandDenied`]
//!   自带中文 `Display`，而不是把英文错误码丢给 UI 自己拼。

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::SandboxMode;
use crate::redact::{RedactedArgs, RedactedEnv};

/// 运行平台。
///
/// 只在"能力矩阵"与拒绝原因里出现；实现层用它区分"没有隔离"与"隔离坏了"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Windows。
    Windows,
    /// Linux。
    Linux,
    /// macOS。
    MacOs,
    /// 其它（BSD 等）—— 本仓库不为它做命令隔离。
    Other,
}

impl Platform {
    /// 当前编译目标平台（`cfg!` 判定，不读环境、不做 IO）。
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }

    /// 稳定字符串（日志、审计、拒绝原因里用；不要用它做控制流）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次调用请求的资源上限（**整棵进程树**口径，不是单进程）。
///
/// 全 `None` = "不额外限制"，也就是本字段出现之前的行为。哪些平台真能兑现、
/// 兑现到什么精度，见 `hbot-command` 的 [`CapabilityReport`] 与 `limits()` 支持矩阵：
/// 请求了平台做不到的限额会得到 [`CommandDenied::IsolationUnavailable`]，
/// **不会**被静默忽略（静默忽略等于骗调用方）。
///
/// 单位：
///
/// - `cpu_ms`：**用户态 CPU 毫秒**（Windows job 的 `PerJobUserTimeLimit` 是 100ns 单位，
///   精度取决于内核记账；Linux `RLIMIT_CPU` 只有**秒级且向上取整**）；
/// - `memory_bytes`：**提交/地址空间**字节。Windows 是 job 提交内存总量；
///   Linux 是 `RLIMIT_AS`（虚拟地址空间，比 RSS 严——程序 `mmap` 未触碰的页也算）；
/// - `read_bytes`/`write_bytes`：整棵树累计的 I/O 传输字节（不是"瞬时速率"）；
/// - `process_count`：整棵树**同时存活**的进程数上限（不是累计创建数）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ResourceLimits {
    /// 用户态 CPU 毫秒上限。`Some(0)` 不合法（见 `CommandDenied::InvalidSpec`）。
    pub cpu_ms: Option<u64>,
    /// 内存字节上限。
    pub memory_bytes: Option<u64>,
    /// 累计读取字节上限。
    pub read_bytes: Option<u64>,
    /// 累计写出字节上限。
    pub write_bytes: Option<u64>,
    /// 同时存活的进程数上限（Windows 内核硬上限夹在 `1..=64`）。
    pub process_count: Option<u32>,
}

impl ResourceLimits {
    /// 是否**没有**请求任何限额（等价于本字段出现之前的行为）。
    ///
    /// 实现层用它跳过"平台不支持"检查：没请求就不该因为平台做不到而拒绝。
    pub const fn is_empty(&self) -> bool {
        self.cpu_ms.is_none()
            && self.memory_bytes.is_none()
            && self.read_bytes.is_none()
            && self.write_bytes.is_none()
            && self.process_count.is_none()
    }
}

/// I/O 方向（只用于拒绝原因与日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoDirection {
    /// 读取。
    Read,
    /// 写出。
    Write,
}

impl IoDirection {
    /// 稳定字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl fmt::Display for IoDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "读取",
            Self::Write => "写出",
        })
    }
}

/// 一次命令执行**实际**消耗了多少资源（时间、内存、I/O、进程数）。
///
/// 全部是"整棵树"口径的观测值，`0` 表示**没有测量**（该平台没有这项计数器），
/// 不表示"用了 0"。具体每项的精度与盲区见 `hbot-command` 的模块文档
/// 「资源上限的语义与代价」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ResourceUsage {
    /// 用户态 CPU 毫秒（Windows：job `TotalUserTime`；Linux：`RUSAGE_CHILDREN` 差值）。
    pub cpu_ms: u64,
    /// 内存峰值字节（Windows：`PeakJobMemoryUsed`；Linux：`ru_maxrss`，**上界估计**）。
    pub peak_memory_bytes: u64,
    /// 累计读取字节（Linux 恒为 0：未测量）。
    pub read_bytes: u64,
    /// 累计写出字节（Linux 恒为 0：未测量）。
    pub write_bytes: u64,
    /// 观测到的同时存活进程数峰值（10 ms 采样，**短命进程可能漏计**）。
    pub peak_processes: u32,
}

/// 一次命令执行的完整规格。
///
/// **字段都是显式的**：没有"继承父进程"的默认值，没有 shell，没有通配。
/// 这是刻意的——`CommandSpec` 就是审批键里那个"完整动作"的一部分
/// （`AGENTS.md` §0.7：禁止按大类放行）。
///
/// 用 [`CommandSpec::new`] 起步，再改需要的字段；不要 `..Default::default()`
/// （默认超时 0 秒没有意义）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// 程序。绝对路径，或**裸文件名**（按父进程 `PATH` 解析，绝不搜当前目录）。
    ///
    /// 相对路径（`.\cargo`、`bin/run`）一律拒绝：那会引入"CWD 劫持"——项目目录是
    /// Agent 可写的，`PATH` 搜索顺序里混进 CWD 就等于让被约束的一方决定跑什么。
    pub program: String,

    /// 参数数组。原样传给子进程，**不经过 shell**（没有引号解析、没有通配展开、
    /// 没有 `&&`/`|`/重定向）。
    pub args: Vec<String>,

    /// 工作目录。相对路径按 [`CommandSpec::workspace_root`] 解析；
    /// 空路径表示"项目根"。无论哪种形式，规范化后必须仍在项目根内。
    pub cwd: PathBuf,

    /// 项目根的绝对路径（`hbot-workspace` 打开时已规范化）。工作目录守卫以它为准。
    pub workspace_root: PathBuf,

    /// 显式环境变量（键值对）。子进程环境**只包含**这里与 [`CommandSpec::inherit_env`]
    /// 的内容，其余一律不继承。
    pub env: Vec<(String, String)>,

    /// 允许从父进程继承的变量**名字**（白名单）。名字必须存在于父进程，
    /// 否则跳过（不报错）；加载器注入类变量（`LD_PRELOAD` 等）一律拒绝。
    pub inherit_env: Vec<String>,

    /// 是否要求继承父进程的**全部**环境变量。只允许 `false`；
    /// 传 `true` 会得到 [`CommandDenied::InheritEnvRejected`]——这是 fail-closed 的一部分，
    /// 不是"留个后门给测试用"。
    pub inherit_entire_env: bool,

    /// 超时。到点杀**整棵进程树**（不是只杀直接子进程）。
    pub timeout: Duration,

    /// 每个输出流（stdout / stderr 各自）的字节上限。超过即杀树并返回
    /// [`CommandDenied::TooLong`]；实现不得把无界输出读进内存。
    pub max_output_bytes: usize,

    /// 本次调用需要的沙箱档。映射关系（见 ADR-0003）：
    ///
    /// - [`SandboxMode::ReadOnly`] → 直接拒绝（[`CommandDenied::SandboxDenied`]）；
    /// - [`SandboxMode::WorkspaceWrite`] → 要求 **写隔离**（受限令牌 / landlock /
    ///   sandbox-exec 一类）；装不上就拒绝；
    /// - [`SandboxMode::Danger`] → 只要求**资源与进程树约束**（作业对象 / 进程组），
    ///   但"是否允许"的前置审批仍由 Agent 网关负责。
    pub sandbox: SandboxMode,

    /// 本次调用**允许**的程序清单（绝对路径或裸文件名，大小写按平台）。
    ///
    /// 空表 = 什么都不许跑。这不是隔离（隔离在操作系统里），而是"调用方显式声明
    /// 这一次打算跑什么"，用来把审批范围钉死在一个具体动作上；拿它当隔离用
    /// 就会重演旧项目的坑（`AGENTS.md` §7 第 7 条）。
    pub allowed_programs: Vec<String>,

    /// 本次调用请求的资源上限（CPU 毫秒 / 内存字节 / I/O 字节 / 进程数）。
    ///
    /// 默认 [`ResourceLimits::default`]（全 `None`）= 不额外限制。请求了平台做不到的
    /// 限额会被 [`CommandDenied::IsolationUnavailable`] 拒绝，**不会**静默降级。
    /// 纯新增字段：老调用方不设它，行为与本字段出现之前完全一致。
    #[serde(default)]
    pub limits: ResourceLimits,
}

/// 手写 `Debug`（**不要**换回 `#[derive(Debug)]`）：RT-06。
///
/// 派生实现会把 `env` 的值（`OPENAI_API_KEY=sk-…`）与 `args` 里的
/// `--token=sk-…` / `Authorization: Bearer …` 原样打进 `tracing::warn!(?spec)`、
/// `thiserror` 的 `#[source]` 链和工具输出 —— 一次打印就是一条永久落进 `logs` 表的密钥。
///
/// 形状与派生实现保持一致（12 个字段名都在、非敏感值原样），只有敏感值变成
/// [`crate::redact::REDACTED`]：`args` 走 [`RedactedArgs`]（含 `--flag value` 分开的形态），
/// `env` 走 [`RedactedEnv`]（变量名保留、按键名判值）。判据见 [`crate::redact`]。
impl fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args", &RedactedArgs(&self.args))
            .field("cwd", &self.cwd)
            .field("workspace_root", &self.workspace_root)
            .field("env", &RedactedEnv(&self.env))
            .field("inherit_env", &self.inherit_env)
            .field("inherit_entire_env", &self.inherit_entire_env)
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("sandbox", &self.sandbox)
            .field("allowed_programs", &self.allowed_programs)
            .field("limits", &self.limits)
            .finish()
    }
}

impl CommandSpec {
    /// 新建规格：30 s 超时、每流 256 KiB、`workspace-write` 档、空程序清单。
    ///
    /// 默认值刻意偏保守：`allowed_programs` 为空意味着**什么都跑不了**，
    /// 调用方必须显式写上要跑的程序。
    pub fn new(program: impl Into<String>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: PathBuf::new(),
            workspace_root: workspace_root.into(),
            env: Vec::new(),
            inherit_env: Vec::new(),
            inherit_entire_env: false,
            timeout: Duration::from_secs(30),
            max_output_bytes: 256 * 1024,
            sandbox: SandboxMode::WorkspaceWrite,
            allowed_programs: Vec::new(),
            limits: ResourceLimits::default(),
        }
    }

    /// 把一组系统基线变量名加进继承白名单（`PATH`/`SystemRoot`/`TEMP` 等）。
    ///
    /// 为什么需要它：环境默认是**清空**的，而多数程序（包管理器、`cmd.exe`）没有
    /// `PATH`/`SystemRoot`/`TEMP` 就起不来或行为异常。这是一个"显式选择"的便利方法，
    /// 不是隐式继承：调用方仍然要自己点名。
    pub fn inherit_system_env(&mut self) {
        for name in ["PATH", "SystemRoot", "SystemDrive", "windir", "TEMP", "TMP"] {
            if !self
                .inherit_env
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                self.inherit_env.push(name.to_owned());
            }
        }
    }
}

/// 命令结束方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// 正常退出；`code` 为退出码（Unix 上被信号杀死时为 `None`，`signal` 给信号号）。
    Exited {
        /// 退出码；被信号杀死时为 `None`。
        code: Option<i32>,
        /// Unix 终止信号；Windows 恒为 `None`。
        signal: Option<i32>,
    },
    /// 超时：整棵进程树已被杀死，返回的是**已被截断**的部分输出。
    TimedOut,
}

/// 一次成功返回（未超时、未超输出上限）的命令结果。
///
/// "成功"指**跑完了**，不指退出码为 0——退出码在 [`CommandStatus`] 里，由调用方
/// 决定非零退出算不算失败（`cargo test` 的非零是结果，不是执行错误）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutput {
    /// 结束方式。
    pub status: CommandStatus,
    /// stdout（UTF-8 有损解码；当前不做本地代码页转码）。
    pub stdout: String,
    /// stderr（同上）。
    pub stderr: String,
    /// 从 spawn 到结束（或超时杀树）的墙钟耗时。
    pub duration: Duration,
    /// 本次执行的资源消耗观测值（纯新增字段；未测量的项为 0）。
    ///
    /// 语义见 [`ResourceUsage`]：它是"整棵树"口径，且某些平台干脆没有对应计数器。
    #[serde(default)]
    pub usage: ResourceUsage,
}

/// 拒绝执行的原因。
///
/// 每个变体都要能：① 被测试 `match` 出来（可区分）；② 直接展示给用户
/// （中文一句话 + 可操作建议）。所以这里没有 `Other(String)` 这种垃圾桶变体。
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CommandDenied {
    /// 这台机器上装不出所需隔离。**这是正常返回值，不是内部错误**：
    /// 产品原则是"装不上就拒绝"，不是"尽力跑一下"。
    #[error("这台机器上无法建立命令隔离（平台：{platform}；原因：{reason}），已拒绝执行。")]
    IsolationUnavailable {
        /// 探测时的平台。
        platform: Platform,
        /// 具体原因（中文，短句）。
        reason: String,
    },

    /// 程序不在本次调用的允许清单里，或给了相对路径（不做 PATH 之外的解析）。
    #[error("程序 `{program}` 不在本次允许清单内（或不是绝对路径/裸文件名），已拒绝执行。")]
    ProgramNotAllowed {
        /// 调用方给的程序串。
        program: String,
    },

    /// 裸文件名在父进程 `PATH` 里解析不到。
    #[error("找不到可执行文件 `{program}`（只按父进程 PATH 解析，不搜索当前目录），已拒绝执行。")]
    ProgramNotFound {
        /// 调用方给的程序串。
        program: String,
    },

    /// 工作目录（或给出的路径）规范化后不在项目根内。
    #[error("路径 `{path}` 不在项目根内，已拒绝执行。")]
    OutsideWorkspace {
        /// 规范化前后用于定位的路径串。
        path: String,
    },

    /// 输出超过上限：整棵树已被杀死，**本次调用不返回半截输出**。
    #[error("命令输出超过 {limit_bytes} 字节上限（{stream}），已终止整棵进程树。")]
    TooLong {
        /// 触发的流。
        stream: OutputStream,
        /// 上限（字节）。
        limit_bytes: usize,
    },

    /// 要求继承父进程环境变量（整份继承），或点了加载器注入类变量。
    #[error("拒绝继承父进程环境变量（{vars}），只能用显式白名单。")]
    InheritEnvRejected {
        /// 被拒绝的变量名（整份继承时是 `*`）。
        vars: String,
    },

    /// 当前沙箱档不允许执行命令（`read-only`）。
    #[error("当前沙箱档（{mode:?}）不允许执行命令，已拒绝。")]
    SandboxDenied {
        /// 该次调用声明的档位。
        mode: SandboxMode,
    },

    /// 规格本身不合法（空程序名、0 超时、0 输出上限、超长参数等）。
    #[error("命令规格不合法：{reason}")]
    InvalidSpec {
        /// 具体原因。
        reason: String,
    },

    /// 拉不起进程（文件不可执行、工作目录消失、权限不足……）。
    #[error("启动程序 `{program}` 失败：{reason}")]
    SpawnFailed {
        /// 程序（已解析成绝对路径时的形式）。
        program: String,
        /// 系统错误文本。
        reason: String,
    },

    /// 等待 / 回收子进程时出错（不是命令本身失败）。
    #[error("等待命令结束失败（{context}）：{reason}")]
    IoFailed {
        /// 哪一步。
        context: String,
        /// 系统错误文本。
        reason: String,
    },

    /// CPU 时间超过调用方给的上限（[`ResourceLimits::cpu_ms`]）。
    #[error(
        "命令的 CPU 时间已用 {used_ms} 毫秒，超过 {limit_ms} 毫秒上限（超出 {} 毫秒），已终止整棵进程树。",
        .used_ms.saturating_sub(*limit_ms)
    )]
    CpuLimit {
        /// 实际用量（毫秒，观测值）。
        used_ms: u64,
        /// 调用方给的上限（毫秒）。
        limit_ms: u64,
    },

    /// 内存占用超过调用方给的上限（[`ResourceLimits::memory_bytes`]）。
    #[error(
        "命令内存占用超过 {limit_bytes} 字节（约 {} MiB）上限，已终止整棵进程树。",
        .limit_bytes / (1024 * 1024)
    )]
    MemoryLimit {
        /// 调用方给的上限（字节）。
        limit_bytes: u64,
    },

    /// I/O 传输量超过调用方给的上限（[`ResourceLimits::read_bytes`] /
    /// [`ResourceLimits::write_bytes`]）。
    #[error(
        "命令的{direction} I/O 已传 {used_bytes} 字节，超过 {limit_bytes} 字节上限（超出 {} 字节），已终止整棵进程树。",
        .used_bytes.saturating_sub(*limit_bytes)
    )]
    IoLimit {
        /// 触发的方向。
        direction: IoDirection,
        /// 实际用量（字节，观测值）。
        used_bytes: u64,
        /// 调用方给的上限（字节）。
        limit_bytes: u64,
    },

    /// 同时存活的进程数触到调用方给的上限（[`ResourceLimits::process_count`]）。
    ///
    /// Windows 上这条是**内核硬上限**：`ActiveProcessLimit` 让第 `limit + 1` 个进程
    /// **创建失败**，而不是我们事后杀树——所以命令可能是"自己 spawn 失败后退出"，
    /// 不一定变成这个拒绝原因（见 `hbot-command` 的 `containment_win` 注释）。
    #[error("命令的并发进程数触到 {limit} 个上限（整棵进程树，含根进程），已终止整棵进程树。")]
    ProcessLimit {
        /// 调用方给的上限。
        limit: u32,
    },
}

impl CommandDenied {
    /// 稳定的错误码串（日志、审计、UI 分流用；**不要**用它做本地化）。
    ///
    /// 风格与 `hbot-code` 的 `CodeError::code()`（`HBOT_TIMEOUT` 一族）一致：
    /// 全大写、`HBOT_` 前缀、下划线分词。新增变体必须同时给码，否则这里编译不过
    /// （`match` 没有 `_` 兜底）。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::IsolationUnavailable { .. } => "HBOT_COMMAND_ISOLATION_UNAVAILABLE",
            Self::ProgramNotAllowed { .. } => "HBOT_COMMAND_PROGRAM_NOT_ALLOWED",
            Self::ProgramNotFound { .. } => "HBOT_COMMAND_PROGRAM_NOT_FOUND",
            Self::OutsideWorkspace { .. } => "HBOT_COMMAND_OUTSIDE_WORKSPACE",
            Self::TooLong { .. } => "HBOT_COMMAND_TOO_LONG",
            Self::InheritEnvRejected { .. } => "HBOT_COMMAND_INHERIT_ENV_REJECTED",
            Self::SandboxDenied { .. } => "HBOT_COMMAND_SANDBOX_DENIED",
            Self::InvalidSpec { .. } => "HBOT_COMMAND_INVALID_SPEC",
            Self::SpawnFailed { .. } => "HBOT_COMMAND_SPAWN_FAILED",
            Self::IoFailed { .. } => "HBOT_COMMAND_IO_FAILED",
            Self::CpuLimit { .. } => "HBOT_COMMAND_CPU_LIMIT",
            Self::MemoryLimit { .. } => "HBOT_COMMAND_MEMORY_LIMIT",
            Self::IoLimit { .. } => "HBOT_COMMAND_IO_LIMIT",
            Self::ProcessLimit { .. } => "HBOT_COMMAND_PROCESS_LIMIT",
        }
    }
}

/// 输出流标识（只用于拒绝原因与日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// 标准输出。
    Stdout,
    /// 标准错误。
    Stderr,
}

impl OutputStream {
    /// 稳定字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl std::fmt::Display for OutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_current_matches_compile_target() {
        assert_eq!(Platform::current().as_str(), std::env::consts::OS);
    }

    #[test]
    fn new_spec_is_fail_closed() {
        let spec = CommandSpec::new("cargo", "C:\\proj");
        assert!(spec.allowed_programs.is_empty(), "默认什么都不许跑");
        assert!(!spec.inherit_entire_env);
        assert!(spec.inherit_env.is_empty(), "默认不继承任何变量");
        assert_eq!(spec.sandbox, SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn inherit_system_env_is_idempotent_and_case_insensitive() {
        let mut spec = CommandSpec::new("cargo", "C:\\proj");
        spec.inherit_env.push("Path".to_owned());
        spec.inherit_system_env();
        spec.inherit_system_env();
        let path_entries = spec
            .inherit_env
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("PATH"))
            .count();
        assert_eq!(path_entries, 1, "PATH 与 Path 不能各加一条");
    }

    #[test]
    fn denied_messages_are_actionable_chinese() {
        let denied = CommandDenied::IsolationUnavailable {
            platform: Platform::Linux,
            reason: "landlock 不可用".to_owned(),
        };
        let message = denied.to_string();
        assert!(message.contains("已拒绝执行"), "{message}");
        assert!(message.contains("landlock 不可用"), "{message}");
        assert!(message.contains("linux"), "{message}");
    }

    #[test]
    fn new_spec_requests_no_resource_limits() {
        let spec = CommandSpec::new("cargo", "C:\\proj");
        assert!(spec.limits.is_empty(), "默认不额外限制，保持既有行为");
        assert_eq!(spec.limits, ResourceLimits::default());
        assert_eq!(spec.limits.cpu_ms, None);
        assert_eq!(spec.limits.memory_bytes, None);
        assert_eq!(spec.limits.read_bytes, None);
        assert_eq!(spec.limits.write_bytes, None);
        assert_eq!(spec.limits.process_count, None);
    }

    #[test]
    fn any_single_limit_makes_limits_non_empty() {
        let base = ResourceLimits::default();
        assert!(
            !ResourceLimits {
                cpu_ms: Some(1),
                ..base
            }
            .is_empty()
        );
        assert!(
            !ResourceLimits {
                memory_bytes: Some(1),
                ..base
            }
            .is_empty()
        );
        assert!(
            !ResourceLimits {
                read_bytes: Some(1),
                ..base
            }
            .is_empty()
        );
        assert!(
            !ResourceLimits {
                write_bytes: Some(1),
                ..base
            }
            .is_empty()
        );
        assert!(
            !ResourceLimits {
                process_count: Some(1),
                ..base
            }
            .is_empty()
        );
    }

    #[test]
    fn resource_usage_default_means_unmeasured() {
        let usage = ResourceUsage::default();
        assert_eq!(usage.cpu_ms, 0);
        assert_eq!(usage.peak_memory_bytes, 0);
        assert_eq!(usage.read_bytes, 0);
        assert_eq!(usage.write_bytes, 0);
        assert_eq!(usage.peak_processes, 0);
    }

    #[test]
    fn io_direction_names_both_sides_in_chinese() {
        assert_eq!(IoDirection::Read.as_str(), "read");
        assert_eq!(IoDirection::Write.as_str(), "write");
        assert_eq!(IoDirection::Read.to_string(), "读取");
        assert_eq!(IoDirection::Write.to_string(), "写出");
    }

    #[test]
    fn resource_limit_denials_state_the_limit_and_the_overshoot() {
        let cpu = CommandDenied::CpuLimit {
            used_ms: 640,
            limit_ms: 100,
        };
        let cpu_message = cpu.to_string();
        assert!(cpu_message.contains("640"), "{cpu_message}");
        assert!(cpu_message.contains("100"), "{cpu_message}");
        assert!(cpu_message.contains("540"), "要说清超出多少：{cpu_message}");
        assert!(cpu_message.contains("CPU"), "{cpu_message}");

        let memory = CommandDenied::MemoryLimit {
            limit_bytes: 64 * 1024 * 1024,
        };
        let memory_message = memory.to_string();
        assert!(memory_message.contains("67108864"), "{memory_message}");
        assert!(memory_message.contains("64 MiB"), "{memory_message}");
        assert!(memory_message.contains("内存"), "{memory_message}");

        let io = CommandDenied::IoLimit {
            direction: IoDirection::Write,
            used_bytes: 4096,
            limit_bytes: 1024,
        };
        let io_message = io.to_string();
        assert!(io_message.contains("写出"), "{io_message}");
        assert!(io_message.contains("4096"), "{io_message}");
        assert!(io_message.contains("3072"), "要说清超出多少：{io_message}");

        let process = CommandDenied::ProcessLimit { limit: 3 };
        let process_message = process.to_string();
        assert!(process_message.contains('3'), "{process_message}");
        assert!(process_message.contains("进程数"), "{process_message}");
        assert!(process_message.contains("整棵进程树"), "{process_message}");
    }

    #[test]
    fn every_denial_has_a_stable_machine_readable_code() {
        let cases: Vec<(CommandDenied, &str)> = vec![
            (
                CommandDenied::IsolationUnavailable {
                    platform: Platform::Linux,
                    reason: String::new(),
                },
                "HBOT_COMMAND_ISOLATION_UNAVAILABLE",
            ),
            (
                CommandDenied::ProgramNotAllowed {
                    program: "cmd".to_owned(),
                },
                "HBOT_COMMAND_PROGRAM_NOT_ALLOWED",
            ),
            (
                CommandDenied::ProgramNotFound {
                    program: "cmd".to_owned(),
                },
                "HBOT_COMMAND_PROGRAM_NOT_FOUND",
            ),
            (
                CommandDenied::OutsideWorkspace {
                    path: "..".to_owned(),
                },
                "HBOT_COMMAND_OUTSIDE_WORKSPACE",
            ),
            (
                CommandDenied::TooLong {
                    stream: OutputStream::Stdout,
                    limit_bytes: 1,
                },
                "HBOT_COMMAND_TOO_LONG",
            ),
            (
                CommandDenied::InheritEnvRejected {
                    vars: String::new(),
                },
                "HBOT_COMMAND_INHERIT_ENV_REJECTED",
            ),
            (
                CommandDenied::SandboxDenied {
                    mode: SandboxMode::ReadOnly,
                },
                "HBOT_COMMAND_SANDBOX_DENIED",
            ),
            (
                CommandDenied::InvalidSpec {
                    reason: String::new(),
                },
                "HBOT_COMMAND_INVALID_SPEC",
            ),
            (
                CommandDenied::SpawnFailed {
                    program: "cmd".to_owned(),
                    reason: String::new(),
                },
                "HBOT_COMMAND_SPAWN_FAILED",
            ),
            (
                CommandDenied::IoFailed {
                    context: String::new(),
                    reason: String::new(),
                },
                "HBOT_COMMAND_IO_FAILED",
            ),
            (
                CommandDenied::CpuLimit {
                    used_ms: 1,
                    limit_ms: 1,
                },
                "HBOT_COMMAND_CPU_LIMIT",
            ),
            (
                CommandDenied::MemoryLimit { limit_bytes: 1 },
                "HBOT_COMMAND_MEMORY_LIMIT",
            ),
            (
                CommandDenied::IoLimit {
                    direction: IoDirection::Read,
                    used_bytes: 1,
                    limit_bytes: 1,
                },
                "HBOT_COMMAND_IO_LIMIT",
            ),
            (
                CommandDenied::ProcessLimit { limit: 1 },
                "HBOT_COMMAND_PROCESS_LIMIT",
            ),
        ];
        for (denied, expected) in cases {
            assert_eq!(denied.code(), expected, "{denied:?}");
            assert!(
                denied.code().starts_with("HBOT_COMMAND_"),
                "错误码要能按前缀成族：{}",
                denied.code()
            );
            assert!(
                !denied.to_string().is_empty(),
                "每个拒绝都要有人读得懂的中文：{denied:?}"
            );
        }
    }
}
