//! `ws.rs` 的端到端测试：进程内起服务（`127.0.0.1:0`），用 `tokio-tungstenite` 客户端连。
//!
//! 覆盖 `docs/02-architecture.md` §9.3 的安全边界：认证、Origin、帧上限、在飞上限、
//! 未知方法/非法参数的错误码，以及事件广播。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use hbot_core::{BotId, BotStatus};
use hbot_rpc::dispatch::Dispatcher;
use hbot_rpc::dto::{Command, Event};
use hbot_rpc::ws::{WsConfig, WsHandle, WsServer};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const TOKEN: &str = "test-token-0123456789abcdef";
const BOT_ID: &str = "6f1b8f9a-1c2d-4e3f-8a9b-0c1d2e3f4a5b";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 起一个测试服务：`auth_timeout` 收窄到 200ms，避免测试真的等 5 秒。
async fn start(mut cfg: WsConfig) -> (WsHandle, Arc<Dispatcher>) {
    cfg.bind = SocketAddr::from(([127, 0, 0, 1], 0));
    cfg.auth_timeout = Duration::from_millis(200);
    cfg.token = TOKEN.to_owned();

    let server = WsServer::new(cfg);
    let dispatcher = Arc::new(Dispatcher::new().with_events(server.event_sink()));
    let handle = server.bind(dispatcher.clone()).await.expect("绑定测试端口");
    (handle, dispatcher)
}

async fn connect(addr: SocketAddr) -> Client {
    let (client, _response) = connect_async(format!("ws://{addr}"))
        .await
        .expect("WS 握手应成功");
    client
}

async fn send(client: &mut Client, value: &Value) {
    client
        .send(Message::text(value.to_string()))
        .await
        .expect("发送帧");
}

/// 读下一个 JSON 帧（跳过 ping/pong）。
async fn next_json(client: &mut Client) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .expect("等响应超时")
            .expect("连接已结束")
            .expect("读帧失败");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).expect("响应应是 JSON"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("期望文本帧，收到 {other:?}"),
        }
    }
}

/// 等一个关闭帧，返回关闭码。
async fn expect_close(client: &mut Client) -> CloseCode {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .expect("等关闭帧超时");
        match message {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code,
            Some(Ok(Message::Close(None))) => return CloseCode::Normal,
            Some(Ok(Message::Text(_)))
            | Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_))) => {
                continue;
            }
            Some(Ok(other)) => panic!("期望关闭帧，收到 {other:?}"),
            Some(Err(_)) | None => return CloseCode::Abnormal,
        }
    }
}

async fn authenticate(client: &mut Client) -> Value {
    send(
        client,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "auth",
            "params": { "token": TOKEN },
        }),
    )
    .await;
    next_json(client).await
}

// ── 1. 未认证直接发 dispatch ────────────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_dispatch_closes_with_policy() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;

    send(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "dispatch",
            "params": { "type": "app_info" },
        }),
    )
    .await;

    assert_eq!(expect_close(&mut client).await, CloseCode::Policy);
    handle.shutdown().await;
}

// ── 2. 错误 token ──────────────────────────────────────────────────────────

#[tokio::test]
async fn wrong_token_closes_with_policy() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;

    send(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "auth",
            "params": { "token": "wrong-token" },
        }),
    )
    .await;

    assert_eq!(expect_close(&mut client).await, CloseCode::Policy);
    handle.shutdown().await;
}

// ── 2b. 认证超时 ────────────────────────────────────────────────────────────

#[tokio::test]
async fn authentication_timeout_closes_with_policy() {
    // `start` 把 auth_timeout 压到 200ms：不发任何帧，等它自己关。
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;

    assert_eq!(expect_close(&mut client).await, CloseCode::Policy);
    assert!(
        handle.metrics().auth_rejected >= 1,
        "超时应计入 auth_rejected：{:?}",
        handle.metrics()
    );
    handle.shutdown().await;
}

// ── 3. 正确 token + app_info ────────────────────────────────────────────────

#[tokio::test]
async fn authenticated_app_info_returns_name() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;

    let ack = authenticate(&mut client).await;
    assert_eq!(ack["result"]["ok"], json!(true), "认证应答：{ack}");

    send(
        &mut client,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "app_info" }),
    )
    .await;

    let response = next_json(&mut client).await;
    assert_eq!(response["jsonrpc"], json!("2.0"));
    assert_eq!(response["id"], json!(2));
    assert_eq!(
        response["result"]["name"],
        json!("hbot"),
        "响应：{response}"
    );
    assert!(
        response.get("error").is_none(),
        "成功响应不应带 error：{response}"
    );

    handle.shutdown().await;
}

// ── 4. 未知 method ─────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    send(
        &mut client,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "nope" }),
    )
    .await;

    let response = next_json(&mut client).await;
    assert_eq!(response["id"], json!(7));
    assert_eq!(response["error"]["code"], json!(-32601), "响应：{response}");

    handle.shutdown().await;
}

// ── 5. 非法 params ─────────────────────────────────────────────────────────

#[tokio::test]
async fn invalid_params_returns_invalid_params_with_hint() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    // `file_read` 缺 `path`：反序列化失败，message 要带字段线索。
    send(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "dispatch",
            "params": { "type": "file_read" },
        }),
    )
    .await;

    let response = next_json(&mut client).await;
    assert_eq!(response["id"], json!(8));
    assert_eq!(response["error"]["code"], json!(-32602), "响应：{response}");
    let message = response["error"]["message"]
        .as_str()
        .expect("error.message 应是字符串");
    assert!(
        message.contains("path"),
        "错误信息应带字段线索，实际：{message}"
    );

    handle.shutdown().await;
}

// ── 6. 超过 1 MiB 的帧 ─────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_frame_closes_with_size() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    // 合法 JSON、但正文超过 1 MiB：服务端按上限主动关闭 1009。
    let filler = "x".repeat(hbot_rpc::ws::DEFAULT_MAX_MESSAGE_BYTES + 1);
    let frame = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "dispatch",
        "params": { "type": "file_write", "path": "a.txt", "content": filler },
    });
    send(&mut client, &frame).await;

    assert_eq!(expect_close(&mut client).await, CloseCode::Size);
    handle.shutdown().await;
}

// ── 7. 超过 max_in_flight ──────────────────────────────────────────────────

#[tokio::test]
async fn too_many_in_flight_returns_conflict() {
    let cfg = WsConfig {
        max_in_flight: 2,
        ..WsConfig::default()
    };
    let (handle, _dispatcher) = start(cfg).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    // 一个 TCP 写里塞进 6 个请求：服务端读第 1、2 个时派出任务但还没机会跑，
    // 第 3 个起立刻撞上"在飞已满"，因此这条断言是确定的（不是靠 sleep 抢时序）。
    let burst: Vec<Value> = (0..6)
        .map(|index| json!({ "jsonrpc": "2.0", "id": 100 + index, "method": "app_info" }))
        .collect();
    for frame in &burst {
        client
            .feed(Message::text(frame.to_string()))
            .await
            .expect("投递请求");
    }
    client.flush().await.expect("冲刷请求");

    let mut ok = 0;
    let mut conflicts = 0;
    for _ in 0..burst.len() {
        let response = next_json(&mut client).await;
        match response["error"]["code"].as_i64() {
            Some(-32003) => {
                assert_eq!(
                    response["error"]["message"],
                    json!("too many in flight"),
                    "响应：{response}"
                );
                conflicts += 1;
            }
            Some(code) => panic!("非预期错误码 {code}：{response}"),
            None => {
                assert_eq!(
                    response["result"]["name"],
                    json!("hbot"),
                    "响应：{response}"
                );
                ok += 1;
            }
        }
    }

    assert!(ok >= 1, "至少一个请求应成功，实际 ok={ok}");
    assert!(
        conflicts >= 1,
        "至少一个请求应被 -32003 拒绝，实际 conflicts={conflicts}"
    );
    assert!(handle.metrics().in_flight_rejected >= 1);

    handle.shutdown().await;
}

// ── 8. 事件广播 ────────────────────────────────────────────────────────────

#[tokio::test]
async fn event_is_broadcast_to_authenticated_client() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    let bot_id: BotId = BOT_ID.parse().expect("bot id");
    handle.event_sink().emit(Event::BotStatus {
        bot_id,
        status: BotStatus::Running,
        error: None,
    });

    let frame = next_json(&mut client).await;
    assert_eq!(frame["jsonrpc"], json!("2.0"));
    assert_eq!(frame["method"], json!("event"), "事件帧：{frame}");
    assert_eq!(frame["params"]["type"], json!("bot.status"));
    assert_eq!(frame["params"]["status"], json!("running"));
    assert!(frame.get("id").is_none(), "事件帧不应带 id：{frame}");

    handle.shutdown().await;
}

// ── 9. Origin 白名单（补充：握手阶段拒绝）──────────────────────────────────

#[tokio::test]
async fn origin_outside_allowlist_is_rejected_at_handshake() {
    let cfg = WsConfig {
        allowed_origins: vec!["http://localhost:1420".to_owned()],
        ..WsConfig::default()
    };
    let (handle, _dispatcher) = start(cfg).await;

    let mut request = format!("ws://{}", handle.addr())
        .into_client_request()
        .expect("构造握手请求");
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_static("http://evil.example"));

    match connect_async(request).await {
        Ok(_) => panic!("Origin 不在白名单时握手必须失败"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("403") || text.to_lowercase().contains("forbidden"),
                "应是 403 拒绝，实际：{text}"
            );
        }
    }

    assert_eq!(handle.metrics().handshake_rejected, 1);
    handle.shutdown().await;
}

// ── dispatch 通道（裸 Command 与 {"command": …} 都接受）─────────────────────

#[tokio::test]
async fn dispatch_accepts_bare_command_and_wrapped_command() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    send(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "dispatch",
            "params": { "type": "app_info" },
        }),
    )
    .await;
    assert_eq!(
        next_json(&mut client).await["result"]["name"],
        json!("hbot")
    );

    send(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "dispatch",
            "params": { "command": Command::AppInfo },
        }),
    )
    .await;
    assert_eq!(
        next_json(&mut client).await["result"]["name"],
        json!("hbot")
    );

    handle.shutdown().await;
}

// ── 事件分类：`approval.request` 不算可丢 ───────────────────────────────────

#[tokio::test]
async fn metrics_count_authenticated_connections() {
    let (handle, _dispatcher) = start(WsConfig::default()).await;
    let mut client = connect(handle.addr()).await;
    authenticate(&mut client).await;

    let metrics = handle.metrics();
    assert_eq!(metrics.authenticated, 1);
    assert!(metrics.active_connections >= 1, "metrics：{metrics:?}");

    client.close(None).await.expect("关闭客户端");
    handle.shutdown().await;
}
