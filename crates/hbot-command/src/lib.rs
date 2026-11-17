//! hbot-command：在受控条件下启动子进程、限制它，并在超时/超限时杀掉**整棵进程树**。
//!
//! 这个 crate 只有一个目的，即实现 [`hbot_core::ports::CommandRunner`]。它
//! **不认识** Agent、工具、审批——那些在 `hbot-agent` 的网关里；这里只回答一个问题：
//! "在这台机器的这个平台上、用这套机制，这条命令能不能跑；能跑的话怎么保证跑完或被杀干净"。
//!
//! # 诚实的能力矩阵（截至 `docs/adr/0003-command-isolation.md`）
//!
//! 两层隔离是分开的两件事：[`IsolationLevel::Containment`] 管"跑飞了要能杀干净"，
//! [`IsolationLevel::WriteConfinement`] 管"写不出工作区"。档位映射见
//! [`CapabilityReport::required_level`]：`read-only` 一律拒绝（这一层不提供只读视图）、
//! `workspace-write` 要写隔离、`danger` 只要 Containment。
//!
//! | 平台 | `Containment`（进程树 + 资源上限） | `WriteConfinement`（写隔离） |
//! |---|---|---|
//! | Windows | 作业对象：`KILL_ON_JOB_CLOSE` + 活跃进程数/内存上限 | 受限令牌：`WRITE_RESTRICTED` + 工作区 write SID + 该目录上的 ACE |
//! | Linux | `setsid` + `killpg`（整个进程组） | Landlock：`pre_exec` 里 `landlock_restrict_self`；按 ABI 1–5 收窄权限位，可写 = 工作区 + 私有缓存目录；**只交叉编译过，未在真实 Linux 内核上运行** |
//! | macOS | **未实现**（`sandbox-exec` 已弃用） → `IsolationUnavailable` | **未实现**（同上） → `IsolationUnavailable` |
//!
//! 探测是**运行时**的：同样一份代码，装了 Landlock 的 Linux 与没装的是两个结论。
//! **装不上就拒绝执行**，不允许"尽力跑一下再报告"；拒绝原因是一句中文、可展示给用户的
//! 话（见 [`hbot_core::CommandDenied::IsolationUnavailable`]），不是内部错误码。
//!
//! # 这一层挡住了什么、挡不住什么
//!
//! 挡住：命令跑飞了会连带整棵进程树被杀（超时、输出超限、正常退出后的残留后代）、
//! 单个进程/整个作业的内存与并发进程数有上限、工作目录逃不出项目根、父进程环境变量
//! 默认不透传、程序清单空时什么都不许跑；`workspace-write` 档下**写不出工作区**
//! （Windows：受限令牌的 restricting SID + 目录 ACE；Linux：Landlock 只允许工作区内的
//! 写类访问）。读不受影响——写限制只参与写类访问检查。
//!
//! 挡不住：**网络**（写隔离不管网络）、**注册表**（`HKCU` 的写不经过文件 DACL）、
//! **已在内存里的敏感数据**、**CPU 时间**；以及**可写句柄**——父进程继承过去的句柄不再
//! 走访问检查，所以原生 spawn 只继承 stdin/stdout/stderr 三个句柄（`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`），
//! 别的句柄一律不外泄。
//!
//! # Windows 上实测到的启动率代价（本机实测，不是推测）
//!
//! `WRITE_RESTRICTED` 挡的不只是文件写：它对**所有**写类访问检查都只认 restricting SID 列表，
//! 而窗口站、LSA/SSPI、命名对象这些系统对象的 DACL 里没有本 crate 的 write SID。后果是
//! **相当一部分真实程序在 DLL 初始化阶段就死**（退出码 `0xC0000142` = `STATUS_DLL_INIT_FAILED`）。
//! 实测（同一 spawn 路径、同一份父环境、cwd 固定、非管道 stdio 以外条件一致）：
//!
//! - 起得来：`cmd.exe` 的内建命令、`ping.exe`、`reg.exe`、`ipconfig.exe`、`xcopy.exe`、
//!   `icacls.exe`、`findstr.exe`、`hostname.exe`，以及静态 CRT 的 Rust 二进制（本仓库的测试二进制）。
//! - 起不来：`whoami.exe`、`where.exe`、`curl.exe`、`tar.exe`、`powershell.exe`、`pwsh.exe`、
//!   `node.exe`、`python.exe`、`git.exe`、`cargo.exe`（rustup 代理）、`rustup.exe`、`net1.exe`、
//!   `certutil.exe`、`tasklist.exe`、`robocopy.exe`。
//!
//! 已逐一排除的原因：cwd 的 `\\?\` 前缀、cwd 落在 `System32`/`Windows`、自建环境块与继承父环境、
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`、`DETACHED_PROCESS`、`lpDesktop`、restricting SID 的具体集合
//! （自建 write SID / Everyone / Users / INTERACTIVE 都试过）、`SANDBOX_INERT`、`LUA_TOKEN`。
//! 令牌旗标二分把范围锁在 `WRITE_RESTRICTED` 本身：同一 spawn 路径下，只加
//! `DISABLE_MAX_PRIVILEGE`、或者直接用不加限制的基令牌，全部正常。
//! **所以这不是 spawn 代码的 bug，是这套机制的代价**：`workspace-write` 目前适合"脚本级命令"，
//! 不适合跑 `node`/`cargo` 这类工具链；要跑它们得换机制（低完整性标签 / AppContainer /
//! 私有窗口站），见 ADR-0003 的后续项。
//!
//! 另一层现实代价：写隔离只是"写不出去"，所以需要可写缓存目录的命令（`npm`/`pnpm`/`cargo`）
//! 容易失败。两平台处理不同，别混着记：
//!
//! - Windows：本 crate **不**替它们准备可写缓存目录（受限令牌的 ACE 只加在工作区上），
//!   这类命令在 `workspace-write` 档下仍可能失败；
//! - Linux：Landlock 规则里显式放行一个私有缓存根（`<tmpdir>/hbot-command-write-cache`），
//!   并把 `npm_config_cache` / `COREPACK_HOME` / `PNPM_HOME` / `YARN_CACHE_FOLDER` /
//!   `XDG_CACHE_HOME` 默认指过去（显式值优先）。规则与变量用同一个路径，不会漂移。
//!   缓存目录的属主会被核对（它落在世界可写的 tmp 下），不是当前用户的目录直接拒绝。
//!
//! # 用法
//!
//! ```no_run
//! use hbot_command::PlatformRunner;
//! use hbot_core::{CommandRunner, CommandSpec, SandboxMode};
//!
//! let runner = PlatformRunner::system();
//! let mut spec = CommandSpec::new("cargo", std::env::temp_dir());
//! spec.args = vec!["--version".to_owned()];
//! spec.sandbox = SandboxMode::Danger;
//! spec.allowed_programs = vec!["cargo".to_owned()];
//! match runner.run(spec) {
//!     Ok(output) => println!("{:?}\\n{}", output.status, output.stdout),
//!     Err(denied) => println!("{denied}"),
//! }
//! ```
//!
//! 同步、阻塞：async 调用方必须把它放进 `spawn_blocking`（见
//! [`hbot_core::ports::CommandRunner`] 的文档）。
//!
//! Linux 分支的**写隔离**（Landlock）只做到"能交叉编译、纯判据有跨平台单测"，
//! **未在任何真实 Linux 内核上运行过**（本机没有 WSL 发行版）；ABI 探测、规则集构造与
//! `pre_exec` 装载都只经过类型检查与交叉编译（见 ADR-0003 §验证）。它的判据（按 ABI
//! 收窄权限位、工作区/缓存/越界的可写判定、子进程失败码识别）抽在 `landlock.rs` 里，
//! 由 `cargo test -p hbot-command` 在 Windows 上也可以覆盖。

#![warn(missing_docs)]

mod cache;
mod child;
mod confinement;
mod containment;
mod env;
mod landlock;
mod output;
mod path;
mod probe;
mod program;
mod runner;
#[cfg(windows)]
mod sha256;

pub use probe::{CapabilityReport, IsolationLevel, IsolationProbe, ProbeOutcome, SystemProbe};
pub use runner::PlatformRunner;

// 调用方只需要认识这两个 crate 的类型，不必再手写 `hbot_core::…` 前缀。
pub use hbot_core::{
    CommandDenied, CommandOutput, CommandSpec, CommandStatus, OutputStream, Platform,
};
