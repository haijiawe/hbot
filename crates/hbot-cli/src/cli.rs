//! 命令行参数：`hbot version` / `hbot serve` / `hbot check` / `hbot replay`。
//!
//! 这里只做"字符串 → 配置"的解析与默认值，不启动任何服务；可测的纯逻辑放在
//! [`crate::serve`] / [`crate::check`] / [`crate::replay`] / [`crate::version`]。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// `hbot` 命令行入口。
#[derive(Debug, Parser)]
#[command(
    name = "hbot",
    version,
    about = "hbot：无头 RPC 服务、项目体检与版本信息",
    long_about = None,
)]
pub struct Cli {
    /// 子命令。
    #[command(subcommand)]
    pub command: Command,
}

/// 子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 打印版本、平台与能力位。
    Version,
    /// 启动后台 WebSocket RPC 服务（`docs/02-architecture.md` §9.3）。
    Serve(ServeArgs),
    /// 对项目目录做占位体检（只查目录与 `hbot.json`）。
    Check(CheckArgs),
    /// 用历史入站事件重跑当前代码，并对比历史出站（FR-27）。
    Replay(ReplayArgs),
}

/// `hbot serve` 的参数。
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// 监听地址；`:0` 由内核分配端口（真实地址写在就绪行里）。
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: SocketAddr,
    /// 认证 token；不给则读 `HBOT_TOKEN`，再不给则随机生成。**不会打印**。
    #[arg(long)]
    pub token: Option<String>,
    /// 数据目录（当前仅记录到日志，AppInfo 仍用默认目录）。
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// 允许的 `Origin`；可重复。为空表示只接受无 `Origin` 的本地客户端。
    #[arg(long = "origin")]
    pub origins: Vec<String>,
}

/// `hbot check` 的参数。
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// 项目目录。
    pub project_dir: PathBuf,
}

/// `hbot replay` 的参数。
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// 项目目录（库固定在 `<project-dir>/.hbot/hbot.db`）。
    pub project_dir: PathBuf,
    /// 要回放的 bot id（UUID 文本）。
    #[arg(long)]
    pub bot: String,
    /// 最多回放多少条入站事件；不给表示全量。
    #[arg(long)]
    pub limit: Option<u32>,
    /// 只回放该时刻之后（含）的事件，RFC3339（例：`2026-11-16T07:00:00Z`）。
    #[arg(long)]
    pub from: Option<String>,
    /// 输出机器可读报告（单行 JSON）。
    #[arg(long)]
    pub json: bool,
    /// 把这一批入站 payload 存成 fixture 文件（JSON 数组）。
    #[arg(long = "save-fixture", value_name = "PATH")]
    pub save_fixture: Option<PathBuf>,
}
