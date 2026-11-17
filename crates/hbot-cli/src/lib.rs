//! `hbot` 二进制：`version` / `serve` / `check` / `replay`。
//!
//! 边界（`docs/03-directory.md` §2）：这里只做参数解析、进程生命周期与输出，
//! 协议与传输在 [`hbot_rpc`]。
//!
//! 依赖面刻意收窄：**不**依赖 `hbot-bot` / `hbot-agent`（那些实现由 `hbot-rpc` 的
//! `AppState` 装配，CLI 只拿到已装好的 `Dispatcher`）。两个例外：
//!
//! - `hbot-code`：`check` 要真解析 `hbot.json` 并真跑一次 SWC 编译，`replay` 还要真起
//!   isolate 跑代码；
//! - `hbot-store`：`replay` 要读项目库 `.hbot/hbot.db` 里的历史入站/出站（**只读查询**
//!   `list_inbound_history_for_bot` / `list_outbound_by_reply_to`）。它不进 `hbot-rpc` 的
//!   `AppState`——回放是"离线跑一遍"，不该顺手把 bot 拉起来。
//!
//! 输出约定：
//!
//! - stdout **只写数据**：`version` 的文本、`serve` 的 ready JSON、`check` 的
//!   `{"ok":…,"errors":[…]}`、`replay` 的报告（文本或单行 JSON）；
//! - 日志与非致命告警一律走 stderr（`tracing`），不污染 stdout。

#![warn(missing_docs)]

pub mod check;
pub mod cli;
pub mod replay;
pub mod serve;
pub mod version;

pub use check::{CheckReport, check_project};
pub use cli::{CheckArgs, Cli, Command, ReplayArgs, ServeArgs};
// 回放的实现与报告类型都在 `hbot-replay`（与 IDE 的 `replay_run` 共用一份）；这里重导出是为了
// 不改 `hbot-cli` 已有的公开面（`hbot_cli::ReplayReport` 这些路径继续可用）。
pub use hbot_replay::{InboundReplay, ReplayOptions, ReplayReport, replay};
pub use serve::{ServePlan, TokenSource, generate_token, ready_line, resolve_token};
pub use version::{VersionInfo, capabilities, version_info, version_text};

use std::io::Write as _;

use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

/// 一次命令运行的结局：决定进程退出码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 退出码 0：命令跑完了，结论是肯定的（`replay`：全部一致）。
    Success,
    /// 退出码 1：命令跑完了，结论是否定的（`check` 不通过；`replay` 有差异）。
    Failure,
    /// 退出码 2：命令**没跑起来**（`replay`：库缺失、bot 不存在、编译失败…）。
    ///
    /// 与 [`Outcome::Failure`] 分开是刻意的：`1` 回答"代码现在的行为变了没有"，
    /// `2` 回答"这次回放本身能不能做"。把两者混成一个码，CI 就分不清"功能回归"与
    /// "环境坏了"。
    Error,
}

/// 初始化 stderr 日志。
///
/// 默认只开 `info`，`RUST_LOG` 可覆盖；重复调用不会 panic（`try_init` 返回 Err 时忽略）。
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// 往 stdout 写一行（带换行并 flush），失败带上下文。
pub fn write_line(line: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(line.as_bytes())
        .context("写 stdout 失败")?;
    stdout.write_all(b"\n").context("写 stdout 失败")?;
    stdout.flush().context("刷新 stdout 失败")
}

/// 顶层分发。
pub async fn run(cli: Cli) -> anyhow::Result<Outcome> {
    hbot_store::startup_trace::point("run_enter", "");
    match cli.command {
        Command::Version => {
            write_line(&version_text())?;
            Ok(Outcome::Success)
        }
        Command::Check(args) => {
            let report = check_project(&args.project_dir);
            write_line(&report.to_json()?)?;
            if report.ok {
                Ok(Outcome::Success)
            } else {
                Ok(Outcome::Failure)
            }
        }
        Command::Serve(args) => {
            serve::run(args).await?;
            Ok(Outcome::Success)
        }
        Command::Replay(args) => replay::run(args).await,
    }
}
