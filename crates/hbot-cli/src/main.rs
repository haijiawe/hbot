//! `hbot` 进程入口：解析参数、起运行时、把结局映射成退出码。
//!
//! 逻辑都在 [`hbot_cli`]；这里只做进程级收口，避免把 `tokio` 运行时塞进库里。
//!
//! 启动分段：`HBOT_STARTUP_TRACE=1` 时在 `main` 里打几个时间点（默认零开销，
//! 见 `hbot_store::startup_trace`）。格式与 `scripts/headless-startup-probe.ps1`
//! 的解析器一致：`hbot-startup ev=<event> unix_ms=<u64>`。

use std::process::ExitCode;

use clap::Parser as _;
use hbot_cli::{Cli, Outcome};
// 复用 store 里的那份埋点实现，**不**在 CLI 里再抄第三份（desktop 已有一份，
// 因为它是 lib + bin 双入口且拿不到 store 的便捷 re-export）。
use hbot_store::startup_trace::point;

fn main() -> ExitCode {
    point("main_enter", "");
    let cli = Cli::parse();
    point("cli_parsed", "");
    hbot_cli::init_tracing();
    point("tracing_ready", "");

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "无法启动 tokio 运行时");
            return ExitCode::FAILURE;
        }
    };
    point("rt_built", "");

    match runtime.block_on(hbot_cli::run(cli)) {
        Ok(Outcome::Success) => ExitCode::SUCCESS,
        Ok(Outcome::Failure) => ExitCode::FAILURE,
        // `replay` 的"跑不起来"是 2，与"有差异"（1）分开：CI 要能分辨功能回归与环境坏了。
        Ok(Outcome::Error) => ExitCode::from(2),
        Err(error) => {
            tracing::error!(error = format!("{error:#}"), "命令失败");
            ExitCode::FAILURE
        }
    }
}
