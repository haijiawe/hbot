//! hbot-command：在受控条件下启动子进程、限制它，并在超时/超限时杀掉**整棵进程树**。
//!
//! 这个 crate 只有一个目的，即实现 [`hbot_core::ports::CommandRunner`]。它
//! **不认识** Agent、工具、审批——那些在 `hbot-agent` 的网关里；这里只回答一个问题：
//! "在这台机器的这个平台上、用这套机制，这条命令能不能跑；能跑的话怎么保证跑完或被杀干净"。
//!
//! # 诚实的能力矩阵（截至 `docs/adr/0003-command-isolation.md`）
//!
//! | 平台 | `Containment`（进程树 + 资源上限） | `WriteConfinement`（写隔离） |
//! |---|---|---|
//! | Windows | 作业对象：`KILL_ON_JOB_CLOSE` + 活跃进程数/内存上限 | **未实现** → `IsolationUnavailable` |
//! | Linux | `setsid` + `killpg`（整个进程组） | **未实现**（landlock 未接入） → `IsolationUnavailable` |
//! | macOS | **未实现**（`sandbox-exec` 已弃用） → `IsolationUnavailable` | **未实现** → `IsolationUnavailable` |
//!
//! 于是当前唯一能真的跑起来的档位是 `danger`（只要 [`IsolationLevel::Containment`]）；
//! `workspace-write` 因为写隔离还没接入而**一律拒绝**。这是刻意的：**装不上隔离就拒绝执行**，
//! 不允许"尽力跑一下再报告"。拒绝原因是一句中文、可展示给用户的话
//! （见 [`hbot_core::CommandDenied`]），不是内部错误码。
//!
//! # 这一层挡住了什么、挡不住什么
//!
//! 挡住：命令跑飞了会连带整棵进程树被杀（超时、输出超限、正常退出后的残留后代）、
//! 单个进程/整个作业的内存与并发进程数有上限、工作目录逃不出项目根、父进程环境变量
//! 默认不透传、程序清单空时什么都不许跑。
//!
//! 挡不住：**写**。`Containment` 不含任何写限制——被跑起来的程序仍能写项目根之外的
//! 用户目录（受限令牌 / landlock 落地之前这是已知缺口，见 ADR-0003「残余风险」）。
//! 也不含网络、注册表、GPU、CPU 时间限制。
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
//! Linux 分支目前只做到"能编译、逻辑与 Windows 对称"，**未在 Linux 上真机运行**
//! （见 ADR-0003 §验证）。

#![warn(missing_docs)]

mod containment;
mod env;
mod output;
mod path;
mod probe;
mod program;
mod runner;

pub use probe::{CapabilityReport, IsolationLevel, IsolationProbe, ProbeOutcome, SystemProbe};
pub use runner::PlatformRunner;

// 调用方只需要认识这两个 crate 的类型，不必再手写 `hbot_core::…` 前缀。
pub use hbot_core::{
    CommandDenied, CommandOutput, CommandSpec, CommandStatus, OutputStream, Platform,
};
