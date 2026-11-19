//! 命令行参数：`hbot version` / `hbot serve` / `hbot check` / `hbot replay` / `hbot fixture`。
//!
//! 这里只做"字符串 → 配置"的解析与默认值，不启动任何服务；可测的纯逻辑放在
//! [`crate::serve`] / [`crate::check`] / [`crate::replay`] / [`crate::fixture`] /
//! [`crate::version`]。

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
    /// 夹具：把历史存成夹具文件，或用夹具重跑当前代码。
    Fixture(FixtureArgs),
    /// 生成一个认证 token（可选直接写成 systemd 的 `EnvironmentFile`）。
    Token(TokenArgs),
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
    /// 确认「这个端口由前置反向代理终结 TLS」。
    ///
    /// 绑定非回环地址时**必须**显式给这一条：本服务自己不提供 TLS，直接把它暴露到
    /// 网络上等于把 token 明文发出去。不给就拒绝启动，不做"默认放宽"。
    #[arg(long)]
    pub allow_remote: bool,
}

/// `hbot token` 的参数。
#[derive(Debug, Args)]
pub struct TokenArgs {
    /// 写进这个文件（内容是 `HBOT_TOKEN=…` 一行，systemd 的 `EnvironmentFile` 直接读）。
    ///
    /// 给了就往文件里写、**stdout 只打路径**：token 本身不该出现在终端回滚里。
    #[arg(long, value_name = "PATH")]
    pub write: Option<std::path::PathBuf>,
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

/// `hbot fixture` 的参数：两个子命令，`save` 存、`run` 跑。
#[derive(Debug, Args)]
pub struct FixtureArgs {
    /// 子命令。
    #[command(subcommand)]
    pub command: FixtureCommand,
}

/// `hbot fixture` 的子命令。
#[derive(Debug, Subcommand)]
pub enum FixtureCommand {
    /// 把一批历史入站存成夹具文件（JSON 数组，元素就是 isolate 收到的事件）。
    Save(FixtureSaveArgs),
    /// 用当前代码把夹具里每条事件重跑一遍（**不查库**）。
    Run(FixtureRunArgs),
}

/// `hbot fixture save` 的参数。
///
/// 与 `hbot replay` 的区别：回放要**对比**历史出站，存夹具只把入站事件抄出来（不比对，
/// 也不编译），所以它比回放快得多，也不需要"当前代码能编译"。
#[derive(Debug, Args)]
pub struct FixtureSaveArgs {
    /// 项目目录（库固定在 `<project-dir>/.hbot/hbot.db`；夹具路径也按它解析）。
    pub project_dir: PathBuf,
    /// 要存哪个 bot 的历史（UUID 文本）。
    #[arg(long)]
    pub bot: String,
    /// 目标文件：相对项目根的路径，或项目内的绝对路径。
    #[arg(long, value_name = "PATH")]
    pub path: String,
    /// 最多存多少条入站事件；不给表示全量。
    #[arg(long)]
    pub limit: Option<u32>,
    /// 只存该时刻之后（含）的事件，RFC3339（例：`2026-11-16T07:00:00Z`）。
    #[arg(long)]
    pub from: Option<String>,
    /// 输出机器可读结果（单行 JSON）。
    #[arg(long)]
    pub json: bool,
}

/// `hbot fixture run` 的参数。
///
/// **不需要 `--bot`**：夹具里每条事件自带 `botId`，`fixture_run` 也不查库。项目目录仍然
/// 要，因为"用**当前**代码重跑"得先编译这个项目。
#[derive(Debug, Args)]
pub struct FixtureRunArgs {
    /// 项目目录（要编译里面的源码）。
    pub project_dir: PathBuf,
    /// 夹具文件：相对项目根的路径，或项目内的绝对路径。
    #[arg(long, value_name = "PATH")]
    pub path: String,
    /// 输出机器可读报告（单行 JSON）。
    #[arg(long)]
    pub json: bool,
}
