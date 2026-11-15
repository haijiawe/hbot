//! `hbot` 进程入口：解析参数、起运行时、把结局映射成退出码。
//!
//! 逻辑都在 [`hbot_cli`]；这里只做进程级收口，避免把 `tokio` 运行时塞进库里。

use std::process::ExitCode;

use clap::Parser as _;
use hbot_cli::{Cli, Outcome};

fn main() -> ExitCode {
    let cli = Cli::parse();
    hbot_cli::init_tracing();

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

    match runtime.block_on(hbot_cli::run(cli)) {
        Ok(Outcome::Success) => ExitCode::SUCCESS,
        Ok(Outcome::Failure) => ExitCode::FAILURE,
        Err(error) => {
            tracing::error!(error = format!("{error:#}"), "命令失败");
            ExitCode::FAILURE
        }
    }
}
