//! `http.get` 的端到端验收：从 `hbot.json` 到真 socket。
//!
//! 这个文件钉的是"宿主 API 真的会发请求"这件事，两段链路都走真的：
//!
//! * **生产链路**（[`bot_start`](hbot_rpc::dto::Command::BotStart) → `PlatformRouter` →
//!   `LocalAdapter` → 真 `SandboxHandler` → QuickJS → `hbot-adapters` 的 HTTP 客户端）——
//!   证明 `sandbox_handler.rs` 的构造点真的把 transport 装进了沙箱；
//! * **决策链路**（真 `Sandbox::spawn_with_http` + 真 `Sandbox::dispatch` + 真 transport +
//!   真 loopback server）—— 证明清单允许/拒绝、只读档、超限截断这些判据在完整链路上生效。
//!
//! **不联网**：服务端是 `127.0.0.1` 上 `TcpListener::bind(...,0)` 起的进程内 loopback，
//! 端口由内核分配。
//!
//! ## 一个必须说清楚的缝
//!
//! `hbot-adapters` 的 SSRF 守卫在生产路径上**拒绝回环与私网地址**（`is_blocked_ip`，
//! 一行没放宽）。所以"清单允许 127.0.0.1 就能连上"在生产传输下**并不成立**——
//! 这也是产品现状：清单是"允许模型代码访问谁"，适配层还会挡私网。
//!
//! 为了让"允许 → 成功"这条断言仍然能被真 socket 验证，成功类用例用
//! [`AdaptersHttpTransport::with_allowed_hosts`]（`#[doc(hidden)]`，只给测试）点名这一个
//! 回环主机 —— 与 `hbot-adapters` 自己的 loopback/TLS 测试用的是同一个既有逃生口
//! （`FetchOptions::allowed_hosts`）。**生产构造 `AdaptersHttpTransport::new()` 的名单为空**，
//! 由 `net::http_config` 使用；`the_production_transport_is_installed_and_still_blocks_loopback`
//! 这条用例专门钉住"生产传输确实装了、并且仍然拒回环"。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use hbot_adapters::SystemClock;
use hbot_code::{Limits, Sandbox, compile_project, manifest};
use hbot_core::domain::SandboxMode;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::net::{AdaptersHttpTransport, http_config_with};
use hbot_rpc::dto::{Command, Response};
use serde_json::json;

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";

// ---------------------------------------------------------------------------
// 进程内 loopback server（真 socket，真 HTTP/1.1）
// ---------------------------------------------------------------------------

/// 一个只会说 "200 OK + 固定正文" 的回环服务端，并记住被连过几次。
struct Loopback {
    addr: SocketAddr,
    count: Arc<AtomicUsize>,
}

impl Loopback {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let count = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                hits.fetch_add(1, Ordering::SeqCst);
                // 读到请求头结束就够（我们只需要一个合法的请求）
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 512];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let mut response = Vec::new();
                response.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
                response.extend_from_slice(b"Content-Type: text/plain\r\n");
                response.extend_from_slice(b"X-Server: loopback\r\n");
                response.extend_from_slice(
                    format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        Self { addr, count }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// 临时项目目录
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-http-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
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

/// 生成的 handler：调 `http.get`，成功回 `OK <status> <body>`，失败回 `ERR <message>`。
///
/// 捕获 rejection 是刻意的：拒绝对模型代码是 **JS 异常**（所以 `try`/`catch` 能用），
/// 消息里带 `HBOT_HTTP_DENIED` / `HBOT_HTTP_FAILED` 前缀，正好用来断言。
fn handler_source(url: &str) -> String {
    let template = r#"import { defineBot, http } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      const res = await http.get('__URL__');
      ctx.reply('OK ' + res.status + ' ' + res.body);
    } catch (error) {
      ctx.reply('ERR ' + (error && error.message ? error.message : String(error)));
    }
  },
});
"#;
    template.replace("__URL__", url)
}

/// 写一个真项目：`src/main.ts` + `hbot.json`（`permissions.http` 由参数给）。
fn write_project(root: &Path, source: &str, http: &[String]) {
    std::fs::create_dir_all(root.join("src")).expect("创建 src");
    std::fs::write(root.join("src").join("main.ts"), source).expect("写 handler");
    let manifest = serde_json::json!({
        "name": "http-e2e",
        "platform": { "kind": "local" },
        "permissions": { "http": http },
    });
    std::fs::write(
        root.join(manifest::MANIFEST_FILE),
        serde_json::to_string_pretty(&manifest).expect("manifest json"),
    )
    .expect("写 hbot.json");
}

// ---------------------------------------------------------------------------
// A. 生产链路：bot_start → SandboxHandler → http.get
// ---------------------------------------------------------------------------

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str, source: &str, allow: &[String]) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        write_project(&project, source, allow);
        std::fs::write(
            project.join("bots.json"),
            format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"本地试跑"}}]}}"#),
        )
        .expect("写 bots.json");

        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
        }
    }

    fn bot(&self) -> hbot_core::BotId {
        hbot_core::BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
    }

    async fn dispatch(&self, command: Command) -> Response {
        self.dispatcher.dispatch(command).await
    }

    /// 启动 bot 并注入一条消息，最后返回 run log 里的回复文本。
    ///
    /// 走的是产品路径：`bot_start` 建 runtime，`bot_inject` 让消息真的进入
    /// `SandboxHandler`（第一次注入时才编译 + 起 isolate）。
    async fn say(&self, text: &str) -> Vec<String> {
        let bot = self.bot();
        assert_ok(&self.dispatch(Command::BotStart { bot_id: bot }).await);
        let conversation = self
            .state
            .store()
            .get_conversation_by_external(&bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .expect("bot_start 应建好 local:default 会话");
        assert_ok(
            &self
                .dispatch(Command::BotInject {
                    bot_id: bot,
                    conversation_id: conversation.id,
                    text: text.to_owned(),
                })
                .await,
        );
        wait_for("run log 里出现回复", || !self.replies(bot).is_empty()).await;
        let replies = self.replies(bot);
        assert_ok(&self.dispatch(Command::BotStop { bot_id: bot }).await);
        replies
    }

    fn replies(&self, bot: hbot_core::BotId) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&bot), 256)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }
}

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

async fn wait_for(label: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..240 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 清单里 `permissions.http: []` = 全禁：拒在授权判断，server 一个请求都收不到。
#[tokio::test]
async fn an_empty_allow_list_denies_and_never_reaches_the_server() {
    let server = Loopback::start(b"should never be fetched".to_vec());
    let harness = Harness::new("empty", &handler_source(&server.url("/x")), &[]).await;

    let replies = harness.say("hi").await;

    assert_eq!(replies.len(), 1, "应该只有一条回复：{replies:?}");
    assert!(
        replies[0].contains("HBOT_HTTP_DENIED"),
        "空清单必须是 DENIED：{replies:?}"
    );
    assert!(
        replies[0].contains("permissions.http is empty"),
        "拒绝理由要指向空清单：{replies:?}"
    );
    assert_eq!(server.count(), 0, "被拒的请求不该连到 server");
}

/// 清单写的是**别的端口**：主机对得上、端口对不上 → 拒，server 0 请求。
#[tokio::test]
async fn a_wrong_port_is_denied_and_never_reaches_the_server() {
    let server = Loopback::start(b"should never be fetched".to_vec());
    let harness = Harness::new(
        "wrongport",
        &handler_source(&server.url("/x")),
        &["127.0.0.1:1".to_owned()],
    )
    .await;

    let replies = harness.say("hi").await;

    assert!(
        replies[0].contains("HBOT_HTTP_DENIED"),
        "端口不匹配必须被拒：{replies:?}"
    );
    assert_eq!(server.count(), 0, "被拒的请求不该连到 server");
}

/// **生产传输确实装上了**：清单写 `*`（授权通过）→ 请求真的进了传输层 →
/// 被 `hbot-adapters` 的 SSRF 守卫拒掉。
///
/// 这条是 `sandbox_handler.rs` 接线的关键证据：如果 transport 没装上，
/// 错误会变成 `no HTTP transport is installed in this sandbox`（也是 FAILED 文案），
/// 而这里必须是 DENIED 且理由来自适配层。
#[tokio::test]
async fn the_production_transport_is_installed_and_still_blocks_loopback() {
    let server = Loopback::start(b"should never be fetched".to_vec());
    let harness = Harness::new(
        "installed",
        &handler_source(&server.url("/x")),
        &["*".to_owned()],
    )
    .await;

    let replies = harness.say("hi").await;

    assert!(
        replies[0].contains("HBOT_HTTP_DENIED"),
        "适配层的 SSRF 拒绝要原样冒出来（不是泛泛的失败）：{replies:?}"
    );
    assert!(
        !replies[0].contains("no HTTP transport"),
        "transport 没装上就会是这句话；这里不该出现：{replies:?}"
    );
    assert_eq!(server.count(), 0, "回环地址在适配层就被拒，不该建连接");
}

// ---------------------------------------------------------------------------
// B. 决策链路：真 Sandbox + 真 dispatch + 真 transport + 真 loopback
// ---------------------------------------------------------------------------

/// 编译一个项目、按 `hbot.json` 建配置、跑一次 `dispatch`，返回 handler 的回复。
///
/// 组装走 [`http_config_with`] —— 也就是生产路径 `http_config` 用的**同一段**接线，
/// 只是换了一个传输（成功类用例必须点名回环主机，见文件头）。
fn dispatch_with(
    label: &str,
    mode: SandboxMode,
    allow: &[&str],
    transport: AdaptersHttpTransport,
    max_bytes: Option<usize>,
    url: &str,
) -> Vec<String> {
    let dir = TempDir::new(label);
    let allow: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
    write_project(dir.path(), &handler_source(url), &allow);

    let manifest = manifest::load(dir.path()).expect("hbot.json 可解析");
    let mut config = http_config_with(&manifest, mode, transport);
    if let Some(max_bytes) = max_bytes {
        config = config.with_max_bytes(max_bytes);
    }

    let compiled = compile_project(dir.path()).expect("项目可编译");
    let sandbox =
        Sandbox::spawn_with_http(compiled, Limits::default(), config).expect("isolate 起");
    sandbox
        .dispatch(&json!({ "payload": { "text": "hi" } }))
        .expect("handler 跑得完")
        .replies
}

/// 清单允许 + 传输放行这一个回环主机 → 真的拿到 status 与 body。
///
/// `127.0.0.1` 的放行是**测试专用**的（见文件头）：生产传输不点名就拒回环。
#[test]
fn an_allowed_loopback_host_returns_status_and_body() {
    let server = Loopback::start(b"hello from loopback".to_vec());
    let allow = format!("127.0.0.1:{}", server.port());

    let replies = dispatch_with(
        "allow",
        SandboxMode::WorkspaceWrite,
        &[&allow],
        AdaptersHttpTransport::with_allowed_hosts(vec!["127.0.0.1".to_owned()]),
        None,
        &server.url("/x"),
    );

    assert_eq!(replies, vec!["OK 200 hello from loopback".to_owned()]);
    assert_eq!(server.count(), 1, "成功路径应该真的发了一次请求");
}

/// 只读档：**即使清单允许**也在授权判断之前拒，且不建 socket。
#[test]
fn read_only_refuses_before_authorization_and_never_dials() {
    let server = Loopback::start(b"should never be fetched".to_vec());
    let allow = format!("127.0.0.1:{}", server.port());

    let replies = dispatch_with(
        "readonly",
        SandboxMode::ReadOnly,
        &[&allow],
        AdaptersHttpTransport::with_allowed_hosts(vec!["127.0.0.1".to_owned()]),
        None,
        &server.url("/x"),
    );

    assert!(
        replies[0].contains("HBOT_HTTP_DENIED") && replies[0].contains("read-only"),
        "只读档必须先拒：{replies:?}"
    );
    assert_eq!(server.count(), 0, "只读档不该建 socket");
}

/// 超 `max_bytes`：按 `hbot-code` 的既有语义**报错**，不是悄悄截断。
///
/// 传输层把适配层的 `truncated` 映射成 `TransportError::TooLarge`，
/// `hbot-code` 再翻成 `HBOT_HTTP_FAILED` —— 半截正文不会交给模型代码。
#[test]
fn an_oversized_body_fails_instead_of_truncating() {
    let server = Loopback::start(vec![b'x'; 4096]);
    let allow = format!("127.0.0.1:{}", server.port());

    let replies = dispatch_with(
        "oversized",
        SandboxMode::WorkspaceWrite,
        &[&allow],
        AdaptersHttpTransport::with_allowed_hosts(vec!["127.0.0.1".to_owned()]),
        Some(64),
        &server.url("/big"),
    );

    assert!(
        replies[0].contains("HBOT_HTTP_FAILED"),
        "超限必须是失败：{replies:?}"
    );
    assert!(
        !replies[0].contains("OK "),
        "不能把截断的正文当成功交出去：{replies:?}"
    );
    assert_eq!(server.count(), 1, "请求发出去了，只是正文超限");
}
