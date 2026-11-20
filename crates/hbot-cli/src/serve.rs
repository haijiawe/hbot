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
use hbot_core::EventSink;
use hbot_rpc::ws::{WsConfig, WsServer};
use hbot_rpc::{Dispatcher, Event, EventBus};
use tokio::sync::broadcast;
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
    /// 用户是否确认「TLS 由前置反向代理终结」。
    pub allow_remote: bool,
}

impl ServePlan {
    /// 从命令行参数解析（会读环境变量 `HBOT_TOKEN`）。
    pub fn from_args(args: ServeArgs) -> Self {
        Self {
            bind: args.bind,
            token: resolve_token(args.token, token_from_env()),
            data_dir: args.data_dir,
            origins: args.origins,
            allow_remote: args.allow_remote,
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

/// 绑定地址的判据：非回环必须显式确认「TLS 在前面」。
///
/// 本服务**没有** TLS 实现（`docs/08` §6：无头宿主要么被反代保护、要么只绑本机）。
/// 于是这里只有两条路：绑回环，或者用户明确承认自己在前置代理后面。没有第三条
/// 「先跑起来再说」—— 把 token 明文发到公网是那种"出事才发现"的错误。
pub fn ensure_bind_allowed(bind: SocketAddr, allow_remote: bool) -> anyhow::Result<()> {
    if bind.ip().is_loopback() || allow_remote {
        return Ok(());
    }

    anyhow::bail!(
        "拒绝绑定非回环地址 {bind}：本服务不提供 TLS，直接暴露等于把 token 明文发出去。\n确实由前置反向代理终结 TLS 时，加 --allow-remote 显式确认。"
    )
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
    ensure_bind_allowed(plan.bind, plan.allow_remote)?;
    if plan.allow_remote && !plan.bind.ip().is_loopback() {
        warn!(
            bind = %plan.bind,
            "监听非回环地址：TLS 与访问控制由前置反向代理负责，本进程只认 token"
        );
    }
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

    // 事件桥：业务代码把事件发到 `AppState` 的 `EventBus` 上，而 WS 只认
    // `WsServer::event_sink()`，两边之间必须有人转一趟。桌面壳是每个窗口一条事件泵
    // （`crates/hbot-desktop/src/lib.rs` 的 `spawn_event_pump`），无头宿主就是这一条。
    //
    // 漏掉它的症状不是报错：客户端能连上、能发命令、能拿到命令的 `result`，但
    // **一个事件帧都收不到**；数据与日志都正常落库，所以表现为「面板永远不刷新」。
    // 压测就是这么抓到的：注入 30 条消息、库里 31 行回复，客户端 `run.log` 事件 0 条。
    let sink = server.event_sink();
    let bus = Arc::clone(state.events());
    let pump = tokio::spawn(forward_events(bus, sink));

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
    // WS 已经关了，桥没有出口可写；不 abort 会留一个空转任务到进程退出。
    pump.abort();
    // WS 关掉之后才停 bot：信号窗口里正在处理的命令先跑完，否则会在「命令执行到一半」
    // 时拆掉它依赖的执行器。`AppState::shutdown` 会给每个 `onStop` 传 `shutdown`。
    state.shutdown().await;
    info!("hbot serve 已退出");
    Ok(())
}

/// `EventBus` → WS 事件出口。
///
/// 语义与桌面壳的事件泵一致：广播不补发（订阅之前发生的事件拿不到，客户端连上后
/// 先用快照命令补齐状态）；订阅落后（`Lagged`）只记一条警告并继续，因为 WS 层自己还有
/// 每连接的关键/普通两级队列在决定哪些事件不能丢（见 `crates/hbot-rpc/src/ws.rs` 模块文档）。
async fn forward_events(bus: Arc<EventBus>, sink: Arc<dyn EventSink<Event>>) {
    let mut receiver = bus.subscribe();

    loop {
        match receiver.recv().await {
            Ok(event) => sink.emit(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "事件订阅落后，旧事件已丢弃");
            }
            // 总线没了（进程收尾时 sender 被释放）：没有出口可写，退出。
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    info!("事件桥结束");
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

    /// 事件桥：总线上的事件必须真的走到 WS 出口。
    ///
    /// 这条是压测抓到的 bug 的回归（2026-11-21）：[P]hbot serve[P] 以前只装配
    /// [P]Dispatcher::with_state[P]，没有把 [P]EventBus[P] 接到 [P]WsServer::event_sink()[P]，于是客户端
    /// 能连、能发命令、能拿 [P]result[P]，但**一个事件帧都收不到**——库里日志与回复都好好的，
    /// 面板却永远不刷新。漏接不会报错，只能靠这条测试钉住。
    #[tokio::test]
    async fn the_event_bridge_carries_bus_events_to_the_ws_sink() {
        struct Capturing {
            seen: tokio::sync::mpsc::UnboundedSender<Event>,
        }

        impl EventSink<Event> for Capturing {
            fn emit(&self, event: Event) {
                // 桥是同步 [P]emit[P]；测试这边用无界通道收，断言再异步等。
                let _ = self.seen.send(event);
            }
        }

        let bus = Arc::new(EventBus::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink: Arc<dyn EventSink<Event>> = Arc::new(Capturing { seen: tx });
        let pump = tokio::spawn(forward_events(Arc::clone(&bus), sink));

        // 订阅是桥启动时才建立的（广播不补发，与桌面壳的事件泵同一语义），
        // 所以先让出一拍再发，避免这条测试变成时序碰运气。
        tokio::task::yield_now().await;

        let sent = bus.send(Event::RunLog {
            bot_id: hbot_core::BotId::from_uuid(uuid::Uuid::new_v4()),
            level: hbot_rpc::LogLevel::Info,
            line: "soak-1".to_owned(),
            at: hbot_core::Timestamp::from_unix_millis(1),
        });
        assert_eq!(sent, 1, "桥上应当有一个订阅者");

        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("2 秒内应当收到转发的事件")
            .expect("通道不该关");
        match got {
            Event::RunLog { line, .. } => assert_eq!(line, "soak-1"),
            other => panic!("收到的事件类型不对：{other:?}"),
        }

        pump.abort();
    }

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
