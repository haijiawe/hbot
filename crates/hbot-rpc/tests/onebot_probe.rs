//! `bot_start` 的**启动前连通性探测**（`OneBotAdapter::subscribe` 里的 `probe_connection`）。
//!
//! 这条链路此前完全缺失：地址写错/证书不对/token 过期时，用户只能看到"反复重连的
//! warn"——`bot_start` 返回成功，运行日志里只有噪音货。本文件的四例钉住接线之后的
//! 行为：
//!
//! 1. **连接被拒绝** → `bot_start` 返回 `-32002`，消息里说明"连接被拒绝"与地址；
//! 2. **握手正常** → `bot_start` 成功（探测不许把本来能连的判成失败）；
//! 3. **握手很慢但能连**（1.5 秒） → 仍然成功（探测超时是 10 秒，不是 1 秒）；
//! 4. **服务端回 401** → `-32002` + "认证失败"（token 配错要能一眼看出来）。
//!
//! 真实 socket：这台机器上 `127.0.0.1` 一定通，mock 服务器用
//! `tokio_tungstenite::accept_async` 起在随机端口。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hbot_adapters::SystemClock;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};
use tokio::net::TcpListener;

static SEQ: AtomicU64 = AtomicU64::new(0);

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

const HANDLER: &str = r#"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('pong');
  },
});
"#;

// ---------------------------------------------------------------------------
// 临时目录 + 夹具
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-probe-{label}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一个已打开项目 + `bots.json` 的 `AppState`（**用真的** OneBot 适配器工厂）。
struct Harness {
    _dir: TempDir,
    state: std::sync::Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str, ws_url: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("建项目目录");
        std::fs::write(project.join("src").join("main.ts"), HANDLER).expect("写 handler");
        let bots = serde_json::json!({
            "bots": [{ "id": BOT_UUID, "url": ws_url }]
        });
        std::fs::write(
            project.join("bots.json"),
            serde_json::to_string_pretty(&bots).expect("序列化 bots.json"),
        )
        .expect("写 bots.json");

        let state = std::sync::Arc::new(
            AppState::open(
                dir.path().join("data"),
                std::sync::Arc::new(SystemClock),
                std::sync::Arc::new(UuidIds),
            )
            .expect("AppState::open"),
        );
        state.open_project(&project).await.expect("open_project");
        let dispatcher = Dispatcher::new().with_state(std::sync::Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
        }
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: bot_id() })
            .await
    }

    async fn stop(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: bot_id() })
            .await
    }

    /// 库里记录的 bot 状态（`bot_start` 失败后必须是 `error`，不是 `running`）。
    fn stored_status(&self) -> Option<String> {
        self.state
            .store()
            .get_bot_status(&bot_id())
            .expect("读 bot 状态")
            .map(|row| row.status)
    }
}

fn bot_id() -> hbot_core::BotId {
    hbot_core::BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

fn error_of(response: &Response) -> (i32, String) {
    match response {
        Response::Error { error } => (error.code, error.message.clone()),
        other => panic!("期望 Error 响应，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// mock 服务器
// ---------------------------------------------------------------------------

/// 起一个"握手就成功、什么都不发"的 WebSocket 服务器，返回 `ws://` 地址。
///
/// 必须**循环 accept**：`OneBotClient::start` 自己会开一条连接，启动前探测是第二条。
/// 只 accept 一次的话，第二条连接永远排在 backlog 里，最后以 RST 收场
/// （表现为 `os error 10054`，而不是任何有意义的握手错误）。
async fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                if let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await {
                    // 挂着不主动发事件：探测之后 supervisor 会因为"没有事件"而安静等待。
                    use futures_util::StreamExt;
                    while let Some(Ok(_)) = socket.next().await {}
                }
            });
        }
    });
    format!("ws://{addr}")
}

/// 起一个"延迟 `delay` 之后才完成 WebSocket 握手"的服务器（模拟慢但能连的对端）。
async fn spawn_slow_server(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = tokio_tungstenite::accept_async(stream).await;
            });
        }
    });
    format!("ws://{addr}")
}

/// 起一个"回 HTTP 401"的服务器（token 配错的真实表现）。
async fn spawn_unauthorized_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                // 先把请求头读完整再回话。半条请求上直接关连接，Windows 会回 RST 而不是
                // FIN，tungstenite 看到的就是 `os error 10054` 而不是 `HTTP error: 401`。
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buf[..read]),
                    }
                }
                let body = "unauthorized";
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                // 关写端（发 FIN），再等一会儿让客户端把响应解析完。
                let _ = stream.shutdown().await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        }
    });
    format!("ws://{addr}")
}

// ---------------------------------------------------------------------------
// 1. 连接被拒绝：可读原因 + 正确的错误码
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_connection_fails_bot_start_with_a_readable_reason() {
    // 端口 1 在测试机上必然没人监听（也基本不会有权限去监听）。
    let harness = Harness::new("refused", "ws://127.0.0.1:1").await;
    let started = Instant::now();

    let (code, message) = error_of(&harness.start().await);

    assert_eq!(
        code, -32002,
        "配置类失败（地址写错）必须是 invalid，不是 internal：{message}"
    );
    assert!(
        message.contains("连接被拒绝"),
        "用户要能看懂是哪类失败，实际：{message}"
    );
    assert!(
        message.contains("127.0.0.1:1"),
        "原因里必须带上用户填的地址，实际：{message}"
    );
    assert!(
        message.contains("ms"),
        "原因里要写清超时上限，实际：{message}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "拒绝连接是立刻返回的，不该等满 10 秒超时"
    );

    // 启动失败之后不许留下"运行中"的僵尸：状态必须落到 error。
    let status = harness.stored_status();
    assert_eq!(
        status.as_deref(),
        Some(hbot_store::BOT_STATUS_ERROR),
        "探测失败要把 bot 状态标成 error，实际 {status:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. 正常握手：探测不许把能连的判成失败
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reachable_endpoint_starts_normally() {
    let url = spawn_server().await;
    let harness = Harness::new("reachable", &url).await;

    match harness.start().await {
        Response::Ok { .. } => {}
        other => panic!("能连上的地址必须启动成功，实际 {other:?}"),
    }
    let _ = harness.stop().await;
}

// ---------------------------------------------------------------------------
// 3. 慢握手（1.5 秒）：仍然算成功
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_but_reachable_endpoint_is_not_misjudged() {
    let url = spawn_slow_server(Duration::from_millis(1500)).await;
    let harness = Harness::new("slow", &url).await;
    let started = Instant::now();

    match harness.start().await {
        Response::Ok { .. } => {}
        other => panic!("慢但能连的对端不许被判成失败，实际 {other:?}"),
    }
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "应当确实等到了慢握手完成"
    );
    let _ = harness.stop().await;
}

// ---------------------------------------------------------------------------
// 4. 401：认证失败要一眼看得出来
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_handshake_reports_an_auth_failure() {
    let url = spawn_unauthorized_server().await;
    let harness = Harness::new("unauthorized", &url).await;

    let (code, message) = error_of(&harness.start().await);
    assert_eq!(code, -32002, "token 配错是配置问题：{message}");
    assert!(
        message.contains("认证失败"),
        "401 要被翻译成用户能懂的原因，实际：{message}"
    );
}
