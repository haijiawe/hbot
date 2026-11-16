//! QQ 官方网关客户端的端到端测试：**真的**连一个进程内的 WebSocket 监听器。
//!
//! 这里刻意不 mock 客户端的连接/握手/心跳代码：假平台（`tests/qq_support.rs`）起的是
//! 真 `TcpListener` + `tokio_tungstenite::accept_async`，客户端走与生产同一条代码路径。

#[path = "qq_support.rs"]
mod support;

use std::time::Duration;

use serde_json::json;

use hbot_adapters::qq::QqClient;
use hbot_core::domain::ConversationKind;

use support::{FakePlatform, Options};

/// 建一个「握手阶段不会自己超时」的配置（心跳间隔由测试通过 Hello 指定）。
fn config_with_ack_timeout(
    platform: &FakePlatform,
    ack_timeout: Duration,
) -> hbot_adapters::qq::QqConfig {
    platform.config().with_heartbeat_timeout(Some(ack_timeout))
}

#[tokio::test]
async fn handshake_then_group_message_is_normalized_with_openids() {
    let mut platform = FakePlatform::start(Options::default()).await;
    let config = config_with_ack_timeout(&platform, Duration::from_secs(5));
    let mut client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    assert_eq!(conn.index, 1);
    conn.send_json(&support::hello_frame(Duration::from_millis(300)))
        .await;

    // ① Identify：token 形如 `QQBot …`，intents / shard 与配置一致。
    let identify = conn.recv_json().await;
    assert_eq!(identify["op"], 2, "first client frame must be identify");
    let token = identify["d"]["token"]
        .as_str()
        .expect("identify must carry a token string");
    assert!(
        token.starts_with("QQBot "),
        "identify token must be prefixed with `QQBot `: {token}"
    );
    assert_eq!(token, format!("QQBot {}", support::TOKEN_ONE));
    assert_eq!(identify["d"]["intents"], 33_554_432_u64);
    assert_eq!(identify["d"]["shard"], json!([0, 1]));

    // 取 token 只发生一次（网关发现与 WS 握手共用同一个缓存）。
    assert_eq!(
        platform.token_calls(),
        1,
        "token must be cached, not refetched"
    );
    let requests = platform.requests().await;
    let gateway = requests
        .iter()
        .find(|request| request.path == "/gateway")
        .expect("client must discover the gateway over http");
    assert_eq!(
        gateway.header("authorization"),
        Some(format!("QQBot {}", support::TOKEN_ONE).as_str())
    );

    // ② READY + 真实形状的群消息。
    conn.send_json(&support::ready_frame("session-abc", 1))
        .await;
    conn.send_json(&support::group_message_frame(
        2,
        support::GROUP_MESSAGE_ID,
        "<@196AAC0A1E7A06AC0D5EEB7FCF778303> 你好",
    ))
    .await;

    let message = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("an event must arrive in time")
        .expect("the event channel must stay open");

    assert_eq!(message.external_id, support::GROUP_MESSAGE_ID);
    assert_eq!(message.conversation_external_id, support::GROUP_OPENID);
    assert_eq!(message.kind, ConversationKind::Group);
    // 会话 ID 是 openid，不是 QQ 号；发送者是 member_openid。
    assert_eq!(message.sender_id, support::USER_OPENID);
    assert_eq!(message.text, "你好", "`<@…>` mention must be stripped");
    assert_eq!(message.reply_to.as_deref(), Some(support::GROUP_MESSAGE_ID));
    assert_eq!(message.platform_message_id, None);
    assert_eq!(message.bot_platform_id, None);
    assert_eq!(
        message.received_at.unix_millis(),
        1_794_787_599_000,
        "received_at must come from `d.timestamp` (2026-11-16T08:06:39+08:00)"
    );

    // ③ `auth_token` 当密钥：raw 里必须已被脱敏。
    let raw = message.raw.to_string();
    assert!(
        !raw.contains(support::FRAME_AUTH_TOKEN),
        "raw must not contain the auth_token value: {raw}"
    );
    assert!(
        raw.contains("<redacted>"),
        "raw must mark the auth_token as redacted: {raw}"
    );

    client.stop().await;
}

#[tokio::test]
async fn heartbeats_follow_the_server_interval_and_carry_the_last_seq() {
    let mut platform = FakePlatform::start(Options::default()).await;
    let config = config_with_ack_timeout(&platform, Duration::from_secs(2));
    let mut client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_millis(150)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);

    // 两条事件把 last_seq 推到 7。
    conn.send_json(&support::ready_frame("session-hb", 5)).await;
    conn.send_json(&support::group_message_frame(
        7,
        support::GROUP_MESSAGE_ID,
        "hi",
    ))
    .await;

    // 第一条心跳：按 Hello 给的间隔发出，`d` 是最近一次事件的 `s`。
    let beat = conn.recv_json().await;
    assert_eq!(beat["op"], 1, "heartbeat must be op:1");
    assert_eq!(beat["d"], 7, "heartbeat payload must be the latest seq");

    // 回 ACK 后继续心跳（收到 op:11 不退出会话）。
    conn.send_json(&support::heartbeat_ack_frame()).await;
    let beat = conn.recv_json().await;
    assert_eq!(beat["op"], 1);
    assert_eq!(beat["d"], 7);
    conn.send_json(&support::heartbeat_ack_frame()).await;

    // 会话还活着：先收掉第一条消息（它的 `s` 已经把 last_seq 推到 7），再推一条新的。
    let first = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("an event must arrive in time")
        .expect("the event channel must stay open");
    assert_eq!(first.external_id, support::GROUP_MESSAGE_ID);

    conn.send_json(&support::group_message_frame(
        8,
        "ROBOT1.0_second",
        "第二条",
    ))
    .await;
    let message = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("an event must arrive in time")
        .expect("the event channel must stay open");
    assert_eq!(message.external_id, "ROBOT1.0_second");

    client.stop().await;
}

#[tokio::test]
async fn reconnects_with_resume_then_identifies_again_after_invalid_session() {
    let mut platform = FakePlatform::start(Options::default()).await;
    let config = config_with_ack_timeout(&platform, Duration::from_secs(5));
    let client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_millis(1000)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);
    conn.send_json(&support::ready_frame("session-abc", 3))
        .await;

    // 网关掉线 → 重连必须带 Resume（session_id + seq）。
    conn.close().await;
    let mut second = platform.next_connection().await;
    assert_eq!(second.index, 2);
    second
        .send_json(&support::hello_frame(Duration::from_millis(1000)))
        .await;
    let resume = second.recv_json().await;
    assert_eq!(resume["op"], 6, "a reconnect after READY must resume");
    assert_eq!(resume["d"]["session_id"], "session-abc");
    assert_eq!(resume["d"]["seq"], 3);
    assert!(
        resume["d"]["token"]
            .as_str()
            .expect("resume must carry a token")
            .starts_with("QQBot ")
    );

    // 服务端说 invalid session → 下一条连接必须重新 Identify（新会话）。
    second.send_json(&support::invalid_session_frame()).await;
    let mut third = platform.next_connection().await;
    assert_eq!(third.index, 3);
    third
        .send_json(&support::hello_frame(Duration::from_millis(1000)))
        .await;
    let identify = third.recv_json().await;
    assert_eq!(
        identify["op"], 2,
        "after op:9 the client must identify from scratch"
    );

    client.stop().await;
}

#[tokio::test]
async fn missing_heartbeat_ack_reconnects_instead_of_hanging() {
    let mut platform = FakePlatform::start(Options::default()).await;
    // 心跳间隔 100ms（由 Hello 指定），ACK 超时 250ms → 没有 ACK 就必须重连。
    let config = config_with_ack_timeout(&platform, Duration::from_millis(250));
    let client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_millis(100)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);
    conn.send_json(&support::ready_frame("session-wd", 1)).await;

    // 第一条心跳照发（测试不回 ACK）。
    let beat = conn.recv_json().await;
    assert_eq!(beat["op"], 1);

    // 看门狗到期 → 客户端必须重连（而不是静默假死）。
    let second = tokio::time::timeout(Duration::from_secs(5), platform.next_connection())
        .await
        .expect("the client must reconnect after a missing heartbeat ack");
    assert_eq!(second.index, 2);

    client.stop().await;
}

#[tokio::test]
async fn server_requested_heartbeat_is_answered_immediately() {
    let mut platform = FakePlatform::start(Options::default()).await;
    // 心跳间隔很长，保证我们看到的下一帧一定是「被 op:1 触发」的那次。
    let config = config_with_ack_timeout(&platform, Duration::from_secs(5));
    let client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_secs(30)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);
    conn.send_json(&support::ready_frame("session-hb2", 4))
        .await;

    conn.send_json(&support::server_heartbeat_request()).await;
    let beat = conn.recv_json().await;
    assert_eq!(beat["op"], 1, "op:1 from the server must be answered");
    assert_eq!(beat["d"], 4);

    client.stop().await;
}

#[tokio::test]
async fn malformed_frames_are_skipped_without_killing_the_session() {
    let mut platform = FakePlatform::start(Options::default()).await;
    let config = config_with_ack_timeout(&platform, Duration::from_secs(5));
    let mut client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_millis(1000)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);
    conn.send_json(&support::ready_frame("session-bad", 1))
        .await;

    // 坏帧：不是 JSON，以及 op 不是数字。
    conn.send_raw_text("this is not json").await;
    conn.send_raw_text(r#"{"op":"nope","d":{}}"#).await;
    // 会话必须还活着：后面的正常消息照样能收到。
    conn.send_json(&support::group_message_frame(
        2,
        support::GROUP_MESSAGE_ID,
        "还活着",
    ))
    .await;

    let message = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("an event must arrive in time")
        .expect("the event channel must stay open");
    assert_eq!(message.text, "还活着");

    client.stop().await;
}

#[tokio::test]
async fn private_messages_map_to_the_users_conversation() {
    let mut platform = FakePlatform::start(Options::default()).await;
    let config = config_with_ack_timeout(&platform, Duration::from_secs(5));
    let mut client = QqClient::start(config).expect("client starts");

    let mut conn = platform.next_connection().await;
    conn.send_json(&support::hello_frame(Duration::from_millis(1000)))
        .await;
    assert_eq!(conn.recv_json().await["op"], 2);
    conn.send_json(&support::ready_frame("session-c2c", 1))
        .await;
    conn.send_json(&support::private_message_frame(
        2,
        "ROBOT1.0_c2c",
        "私聊内容",
    ))
    .await;

    let message = tokio::time::timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("an event must arrive in time")
        .expect("the event channel must stay open");
    assert_eq!(message.kind, ConversationKind::Private);
    assert_eq!(message.conversation_external_id, support::USER_OPENID);
    assert_eq!(message.sender_id, support::USER_OPENID);
    assert_eq!(message.text, "私聊内容");
    assert_eq!(message.reply_to.as_deref(), Some("ROBOT1.0_c2c"));

    client.stop().await;
}

#[tokio::test]
async fn start_rejects_an_empty_secret_before_any_network_io() {
    let config = hbot_adapters::qq::QqConfig::new("1024", "");
    let error = QqClient::start(config)
        .err()
        .expect("empty secret must fail");
    assert!(
        error.to_string().contains("client_secret"),
        "unexpected error: {error}"
    );
}
