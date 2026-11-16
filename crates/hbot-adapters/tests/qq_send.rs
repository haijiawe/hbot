//! 被动回复的端到端测试：真的打到进程内假平台的 HTTP 端点。
//!
//! 覆盖 `msg_id` / `msg_seq` 的语义、凭据脱敏、401 只续期一次的边界。

#[path = "qq_support.rs"]
mod support;

use std::time::Duration;

use serde_json::json;

use hbot_adapters::qq::{QqClient, QqConfig, QqSender};
use hbot_core::domain::{ConversationKind, Timestamp};

use support::{FakePlatform, Options};

/// 群聊入站消息（会话 ID 是 `group_openid`，不是群号）。
fn group_inbound(openid: &str, message_id: &str) -> hbot_adapters::qq::NormalizedInbound {
    hbot_adapters::qq::NormalizedInbound {
        bot_platform_id: None,
        external_id: message_id.to_owned(),
        conversation_external_id: openid.to_owned(),
        kind: ConversationKind::Group,
        sender_id: "336390D2FCA8472A739B0B1B2B6A3A90".to_owned(),
        text: "你好".to_owned(),
        reply_to: Some(message_id.to_owned()),
        platform_message_id: None,
        raw: json!({}),
        received_at: Timestamp::from_unix_millis(1_794_787_599_000),
    }
}

/// 单聊入站消息（会话 ID 是 `user_openid`）。
fn private_inbound(openid: &str, message_id: &str) -> hbot_adapters::qq::NormalizedInbound {
    let mut inbound = group_inbound(openid, message_id);
    inbound.kind = ConversationKind::Private;
    inbound
}

async fn sender_for(platform: &FakePlatform) -> (QqClient, QqSender) {
    let config: QqConfig = platform.config();
    let client = QqClient::start(config).expect("client starts");
    let sender = client.sender();
    (client, sender)
}

#[tokio::test]
async fn group_passive_reply_carries_msg_id_and_increments_msg_seq() {
    let platform = FakePlatform::start(Options::default()).await;
    let (client, sender) = sender_for(&platform).await;

    let first = sender
        .send_group_passive(support::GROUP_OPENID, support::GROUP_MESSAGE_ID, "第一条")
        .await
        .expect("first reply must succeed");
    assert_eq!(first.msg_seq, 1);
    assert_eq!(first.reply_to, support::GROUP_MESSAGE_ID);
    assert_eq!(
        first.message_id.as_deref(),
        Some("MSG-OK"),
        "the response `id` becomes the outbound message id"
    );

    let second = sender
        .send_group_passive(support::GROUP_OPENID, support::GROUP_MESSAGE_ID, "第二条")
        .await
        .expect("second reply must succeed");
    assert_eq!(
        second.msg_seq, 2,
        "the same msg_id must get an increasing msg_seq"
    );

    let requests = platform.send_requests().await;
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        format!("/v2/groups/{}/messages", support::GROUP_OPENID)
    );
    assert_eq!(
        request.header("authorization"),
        Some(format!("QQBot {}", support::TOKEN_ONE).as_str()),
        "the auth header must be `QQBot <token>`"
    );
    assert_eq!(
        request.header("content-type"),
        Some("application/json"),
        "the body must be declared as json"
    );
    let body = request.json();
    assert_eq!(body["msg_id"], support::GROUP_MESSAGE_ID);
    assert_eq!(body["msg_type"], 0);
    assert_eq!(body["msg_seq"], 1);
    assert_eq!(body["content"], "第一条");
    assert_eq!(requests[1].json()["msg_seq"], 2);

    // 凭据不出现在 Debug 输出里。
    let rendered = format!("{sender:?}");
    assert!(!rendered.contains(support::TEST_SECRET));
    assert!(!rendered.contains(support::TOKEN_ONE));

    client.stop().await;
}

#[tokio::test]
async fn c2c_passive_reply_uses_the_users_endpoint() {
    let platform = FakePlatform::start(Options::default()).await;
    let (client, sender) = sender_for(&platform).await;

    let receipt = sender
        .send_c2c_passive(support::USER_OPENID, "ROBOT1.0_c2c", "私聊回复")
        .await
        .expect("c2c reply must succeed");
    assert_eq!(receipt.msg_seq, 1);

    let requests = platform.send_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].path,
        format!("/v2/users/{}/messages", support::USER_OPENID)
    );
    assert_eq!(requests[0].json()["content"], "私聊回复");

    client.stop().await;
}

#[tokio::test]
async fn reply_routes_by_conversation_kind_and_refuses_channels() {
    let platform = FakePlatform::start(Options::default()).await;
    let (client, sender) = sender_for(&platform).await;

    sender
        .reply(&group_inbound(support::GROUP_OPENID, "MSG-G"), "群回复")
        .await
        .expect("group reply must succeed");
    sender
        .reply(&private_inbound(support::USER_OPENID, "MSG-P"), "私聊回复")
        .await
        .expect("c2c reply must succeed");

    let requests = platform.send_requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].path,
        format!("/v2/groups/{}/messages", support::GROUP_OPENID)
    );
    assert_eq!(
        requests[1].path,
        format!("/v2/users/{}/messages", support::USER_OPENID)
    );

    // 频道消息的发送路径没在真网关验过 —— 明确拒绝，不猜端点。
    let mut channel = group_inbound("CHANNEL-1", "MSG-C");
    channel.kind = ConversationKind::Channel;
    let error = sender
        .reply(&channel, "频道回复")
        .await
        .expect_err("channel replies are not implemented");
    assert!(matches!(
        error,
        hbot_adapters::AdapterError::Unsupported { .. }
    ));
    assert_eq!(
        platform.send_requests().await.len(),
        2,
        "a refused reply must not hit the network"
    );

    client.stop().await;
}

#[tokio::test]
async fn active_push_is_unsupported_and_makes_no_request() {
    let platform = FakePlatform::start(Options::default()).await;
    let (client, sender) = sender_for(&platform).await;

    let error = sender
        .send_group_active(support::GROUP_OPENID, "主动消息")
        .await
        .expect_err("active push must not be faked");
    assert!(matches!(
        error,
        hbot_adapters::AdapterError::Unsupported { .. }
    ));
    let error = sender
        .send_c2c_active(support::USER_OPENID, "主动消息")
        .await
        .expect_err("active push must not be faked");
    assert!(matches!(
        error,
        hbot_adapters::AdapterError::Unsupported { .. }
    ));

    // 回复缺 msg_id 也走同一条拒绝路径（不能退化成主动推送）。
    let mut no_reply = group_inbound(support::GROUP_OPENID, "MSG-N");
    no_reply.reply_to = None;
    let error = sender
        .reply(&no_reply, "野回复")
        .await
        .expect_err("a reply without msg_id must be refused");
    assert!(matches!(
        error,
        hbot_adapters::AdapterError::Unsupported { .. }
    ));

    assert_eq!(
        platform.api_calls(),
        0,
        "unsupported paths must not reach the API"
    );

    client.stop().await;
}

#[tokio::test]
async fn msg_seq_tracking_is_bounded_per_message_id() {
    let platform = FakePlatform::start(Options::default()).await;
    // 容量 2：A、B 之后 C 进来会把 A 挤掉，A 的计数回到 1。
    let config = platform.config().with_msg_seq_capacity(2);
    let client = QqClient::start(config).expect("client starts");
    let sender = client.sender();

    for (msg_id, content) in [
        ("MSG-A", "a1"),
        ("MSG-A", "a2"),
        ("MSG-B", "b1"),
        ("MSG-C", "c1"),
        ("MSG-A", "a3"),
    ] {
        sender
            .send_group_passive(support::GROUP_OPENID, msg_id, content)
            .await
            .expect("reply must succeed");
    }

    let seqs: Vec<i64> = platform
        .send_requests()
        .await
        .iter()
        .map(|request| request.json()["msg_seq"].as_i64().expect("msg_seq"))
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2, 1, 1, 1],
        "msg_seq is per msg_id and the table is bounded"
    );

    client.stop().await;
}

#[tokio::test]
async fn an_expired_token_is_refreshed_once_and_the_send_is_retried() {
    let mut options = Options::default();
    options = options
        .with_tokens(&[support::TOKEN_ONE, support::TOKEN_TWO])
        .fail_401_at(1);
    let platform = FakePlatform::start(options).await;
    let (client, sender) = sender_for(&platform).await;

    sender
        .send_group_passive(support::GROUP_OPENID, support::GROUP_MESSAGE_ID, "重试")
        .await
        .expect("the send must succeed after one refresh");

    let requests = platform.send_requests().await;
    assert_eq!(
        requests.len(),
        2,
        "exactly one retry: the adapter must not loop on 401"
    );
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("QQBot {}", support::TOKEN_ONE).as_str())
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some(format!("QQBot {}", support::TOKEN_TWO).as_str()),
        "the retry must use the refreshed token"
    );
    assert_eq!(
        platform.token_calls(),
        2,
        "one initial token fetch plus exactly one refresh"
    );

    client.stop().await;
}

#[tokio::test]
async fn a_token_rejected_after_refreshing_reports_a_credential_error_without_leaking_it() {
    let platform = FakePlatform::start(Options::default().always_401()).await;
    let (client, sender) = sender_for(&platform).await;

    let error = sender
        .send_group_passive(support::GROUP_OPENID, support::GROUP_MESSAGE_ID, "永远 401")
        .await
        .expect_err("a permanently rejected token must surface an error");

    assert_eq!(
        platform.api_calls(),
        2,
        "one attempt plus one retry, then give up"
    );
    assert_eq!(platform.token_calls(), 2, "one fetch plus one refresh");

    let rendered = error.to_string();
    assert!(
        rendered.contains("rejected the access token even after refreshing it"),
        "unexpected error: {rendered}"
    );
    assert!(
        !rendered.contains(support::TEST_SECRET),
        "the error must not leak the client secret: {rendered}"
    );
    assert!(
        !rendered.contains(support::TOKEN_ONE),
        "the error must not leak the access token: {rendered}"
    );
    let debug = format!("{error:?}");
    assert!(!debug.contains(support::TEST_SECRET));

    client.stop().await;
}

#[tokio::test]
async fn config_debug_output_redacts_the_secret() {
    let config = QqConfig::new(support::TEST_APP_ID, support::TEST_SECRET);
    let rendered = format!("{config:?}");
    assert!(
        !rendered.contains(support::TEST_SECRET),
        "Debug must not print the client secret: {rendered}"
    );
    assert!(
        rendered.contains("<redacted>"),
        "Debug must mark the secret as redacted: {rendered}"
    );
    let _ = Duration::from_secs(1);
}
