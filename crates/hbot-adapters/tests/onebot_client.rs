//! 正向 WebSocket 客户端集成测试：用**进程内** mock OneBot 服务器（回环地址 +
//! 随机端口）验证收事件、发动作、心跳超时重连。
//!
//! 不连真实 NapCat：CI 里没有账号也不稳定；这里只验证协议层行为。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use hbot_adapters::AdapterError;
use hbot_adapters::platforms::onebot::backoff::Backoff;
use hbot_adapters::platforms::onebot::client::{OneBotClient, OneBotConfig, validate_ws_url};

/// 一条群消息事件（字段形状与真实 OneBot 11 一致）。
fn group_event(text: &str) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1731652800,
        "self_id": 10000,
        "message_id": 4321,
        "user_id": 20001,
        "group_id": 30001,
        "raw_message": text,
        "font": 0,
        "sender": { "user_id": 20001, "nickname": "阿远" },
        "message": [{ "type": "text", "data": { "text": text } }]
    })
    .to_string()
}

/// 启动 mock 服务器，返回 ws 地址。
///
/// 行为：接受连接 → 推送一条群消息 → 对收到的每个动作帧回 `echo` + `data.message_id`。
// `result_large_err` 是 tungstenite 握手回调的签名决定的（`Err` 是 `ErrorResponse`），
// 这里只做测试断言，不可能变小。
#[allow(clippy::result_large_err)]
async fn spawn_mock_server(
    text: &'static str,
    authorization: Arc<Mutex<Option<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    let url = format!("ws://{address}");

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let value = request
                    .headers()
                    .get("authorization")
                    .and_then(|raw| raw.to_str().ok())
                    .map(str::to_owned);
                *authorization.lock().expect("lock") = value;
                Ok(response)
            },
        )
        .await
        .expect("ws handshake");

        ws.send(WsMessage::text(group_event(text)))
            .await
            .expect("send event");

        while let Some(frame) = ws.next().await {
            let Ok(frame) = frame else { break };
            let Some(text) = frame.to_text().ok() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let Some(echo) = value.get("echo").and_then(Value::as_str) else {
                continue;
            };
            let action = value
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let data = if action == "send_group_msg" {
                json!({ "message_id": 424242 })
            } else {
                json!({ "message_id": 515151 })
            };
            let response = json!({
                "status": "ok",
                "retcode": 0,
                "data": data,
                "echo": echo,
            });
            ws.send(WsMessage::text(response.to_string()))
                .await
                .expect("send response");
        }
    });

    url
}

fn fast_config(url: String) -> OneBotConfig {
    OneBotConfig::new(url)
        .with_heartbeat_timeout(None)
        .with_backoff(Backoff::new(
            Duration::from_millis(5),
            Duration::from_millis(20),
        ))
        .with_capacities(16, 8)
}

#[tokio::test]
async fn receives_event_and_sends_actions_over_websocket() {
    let seen_auth = Arc::new(Mutex::new(None));
    let url = spawn_mock_server("来自 mock 的消息", seen_auth.clone()).await;

    let mut client =
        OneBotClient::start(fast_config(url).with_token("test-token")).expect("client starts");

    let event = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("event arrives in time")
        .expect("channel open");
    assert_eq!(event.text, "来自 mock 的消息");
    assert_eq!(event.sender_id, "20001");
    assert_eq!(event.conversation_external_id, "30001");
    assert_eq!(event.external_id, "4321");

    assert_eq!(
        seen_auth.lock().expect("lock").as_deref(),
        Some("Bearer test-token"),
        "access token 必须以 Authorization: Bearer 发送"
    );

    let data = client
        .send_group_msg(30001, "回复你", Some("4321"))
        .await
        .expect("send_group_msg succeeds");
    assert_eq!(data.get("message_id").and_then(Value::as_i64), Some(424242));

    let data = client
        .send_private_msg(20001, "私聊回复", None)
        .await
        .expect("send_private_msg succeeds");
    assert_eq!(data.get("message_id").and_then(Value::as_i64), Some(515151));

    client.shutdown().await;
}

#[tokio::test]
async fn heartbeat_timeout_triggers_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    let url = format!("ws://{address}");

    tokio::spawn(async move {
        // 第一条连接：握手成功后什么都不发，模拟"连接活着但心跳停了"。
        let (stream, _peer) = listener.accept().await.expect("first accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("first handshake");
        // 等到客户端心跳超时断开。
        while let Some(frame) = ws.next().await {
            if frame.is_err() {
                break;
            }
        }
        drop(ws);

        // 第二条连接：推送事件，证明客户端确实重连了。
        let (stream, _peer) = listener.accept().await.expect("second accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("second handshake");
        ws.send(WsMessage::text(group_event("重连之后的消息")))
            .await
            .expect("send after reconnect");
        // 保持连接，直到客户端停机。
        while let Some(frame) = ws.next().await {
            if frame.is_err() {
                break;
            }
        }
    });

    let config = OneBotConfig::new(url)
        .with_heartbeat_timeout(Some(Duration::from_millis(120)))
        .with_backoff(Backoff::new(
            Duration::from_millis(5),
            Duration::from_millis(20),
        ))
        .with_capacities(16, 8);
    let mut client = OneBotClient::start(config).expect("client starts");

    let event = tokio::time::timeout(Duration::from_secs(10), client.next_event())
        .await
        .expect("reconnected event arrives in time")
        .expect("channel open");
    assert_eq!(event.text, "重连之后的消息");

    client.shutdown().await;
}

#[tokio::test]
async fn action_fails_when_server_reports_error() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    let url = format!("ws://{address}");

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        while let Some(frame) = ws.next().await {
            let Ok(frame) = frame else { break };
            let Some(text) = frame.to_text().ok() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let Some(echo) = value.get("echo").and_then(Value::as_str) else {
                continue;
            };
            let response = json!({
                "status": "failed",
                "retcode": 1404,
                "msg": "群不存在",
                "echo": echo,
            });
            ws.send(WsMessage::text(response.to_string()))
                .await
                .expect("send error response");
        }
    });

    let client = OneBotClient::start(fast_config(url)).expect("client starts");
    let error = client
        .send_group_msg(99999, "hi", None)
        .await
        .expect_err("failed response must surface as error");
    assert!(
        error.to_string().contains("retcode=1404"),
        "错误信息要带平台 retcode: {error}"
    );
    assert!(matches!(error, AdapterError::Protocol { .. }), "{error:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn invalid_urls_are_rejected_before_starting() {
    assert!(validate_ws_url("wss://example.com/ws").is_err());
    assert!(OneBotClient::start(OneBotConfig::new("wss://example.com/ws")).is_err());
    assert!(
        OneBotClient::start(OneBotConfig::new("ws://127.0.0.1:1").with_capacities(0, 1)).is_err()
    );
    assert!(OneBotClient::start(OneBotConfig::new("http://127.0.0.1:1")).is_err());
}
