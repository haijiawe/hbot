//! webhook（`C1.webhook`）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_webhook`）、沙箱侧（`hbot-code` 的 `onWebhook`）、传输层
//! （`hbot-rpc::webhook`）各有单测；本文件补的是它们之间唯一没有别处覆盖的那一段：
//!
//! `AppState::start_bot` → `webhooks().register` → `WebhookServer` 收一条真 TCP 请求
//! → `authorize` → `SandboxHandler::dispatch_webhook` → `onWebhook` → **返回值就是 HTTP 应答**。
//!
//! 八条断言：
//!
//! 1. **没声明路由就是 401**（三种鉴权失败统一 401：路由没声明 / 环境变量没设 / 密钥不匹配）；
//! 2. **环境变量没设 → 401**，不退化成「没配就放行」；
//! 3. **密钥不匹配 → 401**；
//! 4. **正确的密钥 → handler 的返回值成为应答体**（200 + JSON）；
//! 5. **handler 返回 `null` → 204**（无应答体）；
//! 6. **信封 `{ status, body }` 改得了状态码**（418）；
//! 7. **非 POST → 405**；`bot_stop` 之后 → 404；
//! 8. **`Content-Length` 超上限 → 413**（在派发之前就拒）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, WEBHOOK_SECRET_HEADER};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};
use hbot_rpc::webhook::{WebhookConfig, WebhookServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BOT_UUID: &str = "66666666-6666-4666-8666-666666666666";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-webhookcap-{label}-{}-{seq}",
            std::process::id()
        ));
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

/// 三条路由各自一种返回形状：JSON、无应答体、改状态码。
fn hook_handler() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('unused');
  },
  async onWebhook(ctx) {
    log('hook:' + ctx.route + ':' + ctx.method + ':' + ctx.body.length);
    if (ctx.route === 'echo') {
      return { got: ctx.bodyJson };
    }
    if (ctx.route === 'empty') {
      return null;
    }
    return { status: 418, body: 'teapot', contentType: 'text/plain' };
  },
});
"#
}

/// 清单：`routes` 是 (path, secretEnv) 对。
fn manifest(routes: &[(&str, &str)]) -> String {
    let entries: Vec<String> = routes
        .iter()
        .map(|(path, secret_env)| format!("{{\"path\":\"{path}\",\"secretEnv\":\"{secret_env}\"}}"))
        .collect();
    if entries.is_empty() {
        "{\"name\":\"hook-bot\",\"platform\":{\"kind\":\"local\"}}\n".to_owned()
    } else {
        format!(
            "{{\"name\":\"hook-bot\",\"platform\":{{\"kind\":\"local\"}},\"capabilities\":{{\"webhook\":[{}]}}}}\n",
            entries.join(",")
        )
    }
}

fn bots_json() -> String {
    format!(
        "{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"webhook 能力\"}}]}}"
    )
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

/// 设置一个密钥环境变量。
///
/// `std::env::set_var` 在 edition 2024 里是 `unsafe`（进程级可变状态）。这里安全的前提是：
/// **每个用例用自己的变量名**（`HBOT_TEST_HOOK_<label>`），所以并行跑也不会互相覆盖；
/// 没有任何别的线程在读这些名字。
fn set_secret(name: &str, value: &str) {
    // SAFETY: 见函数文档 —— 变量名按用例唯一，且只在本进程里被 `authorize` 读。
    unsafe { std::env::set_var(name, value) };
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    async fn new(label: &str, manifest_json: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), hook_handler()).expect("写 handler");
        std::fs::write(project.join("hbot.json"), manifest_json).expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot: bot_id(),
        }
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: self.bot })
            .await
    }

    async fn stop(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: self.bot })
            .await
    }

    /// 起一个只服务这个 state 的 webhook 服务（端口交给系统挑）。
    async fn serve(&self) -> WebhookServer {
        WebhookServer::bind(WebhookConfig::on_port(0), Arc::clone(&self.state))
            .await
            .expect("起 webhook 服务")
    }

    fn logs(&self) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
            .into_iter()
            .map(|row| row.line)
            .collect()
    }
}

fn assert_ok(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!(
                "{what} 期望成功，实际失败 {}：{}",
                error.code, error.message
            );
        }
    }
}

/// 手写一条 HTTP/1.1 请求并读完整响应，返回 `(状态码, 体)`。
async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String) {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);

    let mut stream = TcpStream::connect(addr).await.expect("连 webhook 服务");
    stream.write_all(&bytes).await.expect("写请求");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("读响应");

    let text = String::from_utf8(response).expect("响应是 UTF-8");
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    (status, body.to_owned())
}

/// 带正确密钥调一次。
async fn call_with_secret(addr: SocketAddr, path: &str, secret: &str, body: &str) -> (u16, String) {
    call(
        addr,
        "POST",
        path,
        &[(WEBHOOK_SECRET_HEADER, secret)],
        body.as_bytes(),
    )
    .await
}

// ---------------------------------------------------------------------------
// ① 路由没声明 → 401（不泄露路由存在性）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_route_is_401() {
    let harness = Harness::new("nodeclared", &manifest(&[])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/echo"), "whatever", "{}").await;
    assert_eq!(status, 401, "没声明路由必须是 401，实际体：{body}");
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 环境变量没设 → 401（fail-closed，不退化成「没配就放行」）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_secret_env_is_401() {
    let harness = Harness::new("noenv", &manifest(&[("echo", "HBOT_TEST_HOOK_NOENV")])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    // 故意不设 `HBOT_TEST_HOOK_NOENV`：这条路由必须一律拒绝。
    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/echo"), "anything", "{}").await;
    assert_eq!(status, 401, "环境变量没设必须 401，实际体：{body}");
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 密钥不匹配 → 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wrong_secret_is_401() {
    let env = "HBOT_TEST_HOOK_WRONG";
    set_secret(env, "correct-horse");
    let harness = Harness::new("wrong", &manifest(&[("echo", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/echo"), "battery", "{}").await;
    assert_eq!(status, 401, "密钥不匹配必须 401，实际体：{body}");
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 正确的密钥 → handler 的返回值成为应答体
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_request_returns_the_handler_value() {
    let env = "HBOT_TEST_HOOK_ECHO";
    set_secret(env, "s3cret");
    let harness = Harness::new("echo", &manifest(&[("echo", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) = call_with_secret(
        addr,
        &format!("/hook/{BOT_UUID}/echo"),
        "s3cret",
        "{\"ticket\":7}",
    )
    .await;
    assert_eq!(status, 200, "正确密钥必须 200，实际体：{body}");
    assert!(
        body.contains("ticket") && body.contains("7"),
        "应答体应当来自 handler 的返回值（含 bodyJson），实际：{body}"
    );

    // handler 里的 ctx.log 也要落库：外部系统看不到它，是唯一排障线索。
    let logs = harness.logs();
    assert!(
        logs.iter().any(|line| line.starts_with("hook:echo:POST:")),
        "handler 的 ctx.log 必须落库，实际日志：{logs:?}"
    );
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ handler 返回 null → 204
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_null_return_is_204() {
    let env = "HBOT_TEST_HOOK_EMPTY";
    set_secret(env, "s3cret");
    let harness = Harness::new("empty", &manifest(&[("empty", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/empty"), "s3cret", "{}").await;
    assert_eq!(status, 204, "null 应当变成 204，实际体：{body}");
    assert!(body.is_empty(), "204 不该有应答体，实际：{body}");
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 信封能改状态码
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_envelope_changes_the_status_code() {
    let env = "HBOT_TEST_HOOK_TEAPOT";
    set_secret(env, "s3cret");
    let harness = Harness::new("teapot", &manifest(&[("teapot", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/teapot"), "s3cret", "{}").await;
    assert_eq!(status, 418, "信封里的 status 必须生效，实际体：{body}");
    assert_eq!(body, "teapot");
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑦ 非 POST → 405；bot_stop 之后 → 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_get_is_405_and_a_stopped_bot_is_404() {
    let env = "HBOT_TEST_HOOK_METHOD";
    set_secret(env, "s3cret");
    let harness = Harness::new("method", &manifest(&[("echo", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    let (status, body) = call(
        addr,
        "GET",
        &format!("/hook/{BOT_UUID}/echo"),
        &[(WEBHOOK_SECRET_HEADER, "s3cret")],
        b"",
    )
    .await;
    assert_eq!(status, 405, "GET 必须 405，实际体：{body}");

    // 停掉 bot：登记表里没有执行器了，同一路径必须 404。
    assert_ok(&harness.stop().await, "bot_stop");
    let (status, body) =
        call_with_secret(addr, &format!("/hook/{BOT_UUID}/echo"), "s3cret", "{}").await;
    assert_eq!(status, 404, "停掉的 bot 必须 404，实际体：{body}");
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// ⑧ Content-Length 超上限 → 413（派发之前就拒）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_body_is_413() {
    let env = "HBOT_TEST_HOOK_BIG";
    set_secret(env, "s3cret");
    let harness = Harness::new("big", &manifest(&[("echo", env)])).await;
    assert_ok(&harness.start().await, "bot_start");
    let server = harness.serve().await;
    let addr = server.addr().expect("拿到端口");

    // 只把 Content-Length 写大、不真发那么多字节：服务应当在读体之前就拒。
    let request = format!(
        "POST /hook/{BOT_UUID}/echo HTTP/1.1\r\nHost: localhost\r\n{0}: s3cret\r\nContent-Length: {1}\r\n\r\n",
        WEBHOOK_SECRET_HEADER,
        hbot_core::MAX_WEBHOOK_BODY_BYTES + 1
    );
    let mut stream = TcpStream::connect(addr).await.expect("连 webhook 服务");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("写请求头");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("读响应");
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(
        text.starts_with("HTTP/1.1 413"),
        "超上限必须 413，实际响应：{text}"
    );
    server.shutdown().await;
    assert_ok(&harness.stop().await, "bot_stop");
}
