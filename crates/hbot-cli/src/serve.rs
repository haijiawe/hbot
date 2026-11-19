//! `hbot serve`：拉起 [`hbot_rpc::ws::WsServer`] 并等信号收尾。
//!
//! 契约（`docs/02-architecture.md` §9.3）：
//!
//! - 只监听本机（默认 `127.0.0.1:0`），跑之前先有 token；
//! - stdout 只打一行就绪 JSON `{"type":"ready","addr":"…","pid":N}`，
//!   **token 与 token 来源只走 stderr 日志**；
//! - Ctrl-C / SIGTERM 走同一条收尾路径（[`hbot_rpc::ws::WsHandle::shutdown`]）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use hbot_rpc::Dispatcher;
use hbot_rpc::ws::{WsConfig, WsServer};
use tracing::{info, warn};

use crate::cli::ServeArgs;

/// token 从哪来（只用于日志，**绝不打印 token 本身**）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenSource {
    /// `--token` 显式给。
    Args(String),
    /// 环境变量 `HBOT_TOKEN`。
    Env(String),
    /// 随机生成（32 字节）。
    Generated(String),
}

impl TokenSource {
    /// token 本身（交给 `WsConfig`，不落日志）。
    pub fn token(&self) -> &str {
        match self {
            Self::Args(token) | Self::Env(token) | Self::Generated(token) => token,
        }
    }

    /// 日志里能写的描述。
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Args(_) => "from-arg",
            Self::Env(_) => "from-env",
            Self::Generated(_) => "generated (32 bytes)",
        }
    }
}

/// 生成 32 字节 token：两段 UUIDv4 的 32 位十六进制拼成 64 个 hex 字符。
///
/// 不引 `rand`/`getrandom`：`uuid` 已经是工作区依赖，v4 自带 CSPRNG。
pub fn generate_token() -> String {
    let mut token = String::with_capacity(64);
    token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    token
}

/// 决定 token 的来源：参数 > 环境变量 > 随机生成。空字符串视作"没给"。
pub fn resolve_token(from_args: Option<String>, from_env: Option<String>) -> TokenSource {
    match (from_args, from_env) {
        (Some(token), _) if !token.is_empty() => TokenSource::Args(token),
        (_, Some(token)) if !token.is_empty() => TokenSource::Env(token),
        _ => TokenSource::Generated(generate_token()),
    }
}

/// 从环境变量读 `HBOT_TOKEN`。
fn token_from_env() -> Option<String> {
    match std::env::var("HBOT_TOKEN") {
        Ok(token) => Some(token),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!("HBOT_TOKEN 不是合法 UTF-8，忽略");
            None
        }
    }
}

/// 解析后的启动计划（纯数据，便于测试）。
#[derive(Clone, Debug)]
pub struct ServePlan {
    /// 监听地址。
    pub bind: SocketAddr,
    /// token 来源。
    pub token: TokenSource,
    /// 数据目录（当前仅日志记录）。
    pub data_dir: Option<PathBuf>,
    /// `Origin` 白名单。
    pub origins: Vec<String>,
}

impl ServePlan {
    /// 从命令行参数解析（会读环境变量 `HBOT_TOKEN`）。
    pub fn from_args(args: ServeArgs) -> Self {
        Self {
            bind: args.bind,
            token: resolve_token(args.token, token_from_env()),
            data_dir: args.data_dir,
            origins: args.origins,
        }
    }

    /// 转成传输层配置。
    pub fn ws_config(&self) -> WsConfig {
        WsConfig {
            bind: self.bind,
            token: self.token.token().to_owned(),
            allowed_origins: self.origins.clone(),
            ..WsConfig::default()
        }
    }
}

/// 就绪行（单行 JSON，写 stdout）。
pub fn ready_line(addr: SocketAddr, pid: u32) -> String {
    serde_json::json!({
        "type": "ready",
        "addr": addr.to_string(),
        "pid": pid,
    })
    .to_string()
}

/// 执行 `hbot serve`。
///
/// 启动分段（`HBOT_STARTUP_TRACE=1`，默认零开销）：本函数打 `serve_enter` /
/// `plan_ready` / `serve_dispatcher` / `serve_ws_bind` / `serve_ready_write` /
/// `ready_written` 六个点，配合 `AppState::open` 与 `Store::open` 里已有的埋点，
/// 就能把「进程创建 → ready 行」拆到每一段。`scripts/headless-startup-probe.ps1`
/// 按这个格式解析。
pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    use hbot_store::startup_trace::{Phase, point};

    point("serve_enter", "");
    let plan = ServePlan::from_args(args);
    let pid = std::process::id();

    let server = WsServer::new(plan.ws_config());

    // 真实应用状态：`hbot serve` 与桌面壳用**同一套** `Dispatcher::with_state`
    // （`docs/02-architecture.md` §2 的"一个组合根"）——后台能做的事，桌面不需要
    // 另写一遍，反之亦然。
    let data_dir = match plan.data_dir.clone() {
        Some(path) => path,
        None => default_data_dir(),
    };
    point("plan_ready", "");
    let state = hbot_rpc::AppState::open(
        &data_dir,
        Arc::new(hbot_adapters::SystemClock),
        Arc::new(hbot_rpc::app::ids::UuidIds),
    )
    .context("打开应用状态失败")?;

    // 留一份 `Arc` 给收尾用：`Dispatcher` 拿走之后还要能调 `AppState::shutdown()`，
    // 否则 Ctrl-C 只能关掉 WS，bot 的 `onStop` 永远不会跑。
    let state = Arc::new(state);
    let phase = Phase::start("serve_dispatcher");
    let dispatcher = Arc::new(Dispatcher::new().with_state(Arc::clone(&state)));
    phase.done();

    let phase = Phase::start("serve_ws_bind");
    let handle = server
        .bind(dispatcher)
        .await
        .context("启动 WebSocket 服务失败")?;
    phase.done();

    info!(
        addr = %handle.addr(),
        pid,
        token = plan.token.describe(),
        data_dir = plan
            .data_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<default>".to_owned()),
        "hbot serve 已就绪"
    );
    point("serve_log_ready", "");

    // 就绪契约 = 这一行的字节进了 stdout（`write_line` 内部 flush）。
    let phase = Phase::start("serve_ready_write");
    crate::write_line(&ready_line(handle.addr(), pid))?;
    phase.done();
    point("ready_written", "");

    wait_for_shutdown().await;
    info!("收到关闭信号，开始收尾");
    handle.shutdown().await;
    // WS 关掉之后才停 bot：信号窗口里正在处理的命令先跑完，否则会在「命令执行到一半」
    // 时拆掉它依赖的执行器。`AppState::shutdown` 会给每个 `onStop` 传 `shutdown`。
    state.shutdown().await;
    info!("hbot serve 已退出");
    Ok(())
}

/// 数据目录：算法只有一份，在 [`hbot_core::paths`]。
///
/// 以前这里抄了一份 `hbot-rpc::dispatch` 的算法（注释还写着"将来会一起改"），
/// 结果桌面壳走 Tauri 的 `app_data_dir()`，三端各有各的库。现在统一到 core。
fn default_data_dir() -> PathBuf {
    hbot_core::data_dir()
}

/// 等 Ctrl-C 或 SIGTERM（Unix）。两者走同一条收尾路径。
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result {
                            warn!(%error, "等待 Ctrl-C 失败");
                        }
                    }
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                warn!(%error, "无法监听 SIGTERM，只等 Ctrl-C");
                if let Err(error) = tokio::signal::ctrl_c().await {
                    warn!(%error, "等待 Ctrl-C 失败");
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "等待 Ctrl-C 失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_is_64_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 64, "token：{token}");
        assert!(
            token.chars().all(|char| char.is_ascii_hexdigit()),
            "token：{token}"
        );
        assert_ne!(token, generate_token(), "两次生成不应相同");
    }

    #[test]
    fn resolve_token_prefers_args_then_env() {
        assert_eq!(
            resolve_token(Some("a".to_owned()), Some("b".to_owned())),
            TokenSource::Args("a".to_owned())
        );
        assert_eq!(
            resolve_token(None, Some("b".to_owned())),
            TokenSource::Env("b".to_owned())
        );
        assert_eq!(
            resolve_token(Some(String::new()), Some("b".to_owned())),
            TokenSource::Env("b".to_owned()),
            "空 token 视作未给"
        );
        assert!(matches!(
            resolve_token(None, None),
            TokenSource::Generated(_)
        ));
    }

    #[test]
    fn token_description_never_contains_secret() {
        let source = TokenSource::Args("super-secret".to_owned());
        assert_eq!(source.describe(), "from-arg");
        assert!(!source.describe().contains("super-secret"));
    }
}
