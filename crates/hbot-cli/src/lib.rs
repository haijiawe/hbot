//! `hbot` 二进制：`version` / `serve` / `check`。
//!
//! 边界（`docs/03-directory.md` §2）：这里只做参数解析、进程生命周期与输出，
//! 协议与传输在 [`hbot_rpc`]。
//!
//! 依赖面刻意收窄：**不**依赖 `hbot-store` / `hbot-bot` / `hbot-agent`（那些实现
//! 由 `hbot-rpc` 的 `AppState` 装配，CLI 只拿到已装好的 `Dispatcher`）。唯一例外是
//! `hbot-code`——`check` 要真解析 `hbot.json` 并真跑一次 SWC 编译，借的是它的
//! `manifest` / `compile` 两个模块，不是沙箱运行时。
//!
//! 输出约定：
//!
//! - stdout **只写数据**：`version` 的文本、`serve` 的 ready JSON、`check` 的
//!   `{"ok":…,"errors":[…]}`；
//! - 日志与非致命告警一律走 stderr（`tracing`），不污染 stdout。

#![warn(missing_docs)]

pub mod check;
pub mod cli;
pub mod serve;
pub mod version;

pub use check::{CheckReport, check_project};
pub use cli::{CheckArgs, Cli, Command, ServeArgs};
pub use serve::{ServePlan, TokenSource, generate_token, ready_line, resolve_token};
pub use version::{VersionInfo, capabilities, version_info, version_text};

use std::io::Write as _;

use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

/// 一次命令运行的结局：决定进程退出码（`check` 失败是 1，其余是 0）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 退出码 0。
    Success,
    /// 退出码 1（体检不通过）。
    Failure,
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
    }
}
