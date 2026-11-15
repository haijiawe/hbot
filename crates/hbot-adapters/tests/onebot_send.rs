//! 出站队列：串行顺序 + 有界重试 + attempts 记录。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use hbot_adapters::AdapterError;
use hbot_adapters::platforms::onebot::backoff::Backoff;
use hbot_adapters::platforms::onebot::send::{
    ActionSender, BoxFuture, OutboundSender, RetryPolicy,
};
use hbot_core::Clock;
use hbot_core::domain::Timestamp;

/// 固定时钟：让 `SendAttempt.at` 可预测。
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(1_700_000_000_000)
    }
}

/// 脚本化发送器：按顺序吐出预置结果，并记录每次调用的 action 名。
struct ScriptedSender {
    script: Mutex<VecDeque<Result<Value, AdapterError>>>,
    calls: AtomicU32,
    seen_actions: Mutex<Vec<String>>,
}

impl ScriptedSender {
    fn new(script: Vec<Result<Value, AdapterError>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: AtomicU32::new(0),
            seen_actions: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen_actions(&self) -> Vec<String> {
        self.seen_actions.lock().expect("lock").clone()
    }
}

impl ActionSender for ScriptedSender {
    fn send_action<'a>(
        &'a self,
        action: String,
        _params: Value,
    ) -> BoxFuture<'a, Result<Value, AdapterError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_actions.lock().expect("lock").push(action);
            let next = self.script.lock().expect("lock").pop_front();
            next.unwrap_or_else(|| Err(AdapterError::transport("script exhausted")))
        })
    }
}

/// 测试用策略：重试间隔压到 1ms，避免测试变慢。
fn fast_policy(attempts: u32) -> RetryPolicy {
    RetryPolicy::new(
        attempts,
        Backoff::new(Duration::from_millis(1), Duration::from_millis(2)),
    )
}

#[tokio::test]
async fn retries_twice_then_succeeds_and_records_attempts() {
    let sender = ScriptedSender::new(vec![
        Err(AdapterError::transport("connection reset")),
        Err(AdapterError::timeout(
            "send_group_msg",
            Duration::from_secs(5),
        )),
        Ok(json!({ "message_id": 424242 })),
    ]);
    let outbound = OutboundSender::start(sender.clone(), fast_policy(3), Arc::new(FixedClock), 8)
        .expect("sender starts");

    let receipt = outbound
        .enqueue("send_group_msg", json!({ "group_id": 1, "message": "hi" }))
        .await
        .expect("third attempt succeeds");

    assert_eq!(receipt.attempts, 3, "2 次失败 + 1 次成功");
    assert_eq!(sender.calls(), 3, "每次重试都要真的调用发送器");
    assert_eq!(receipt.message_id, Some(424242));
    assert_eq!(receipt.history.len(), 3);
    assert!(receipt.history[0].error.is_some());
    assert!(receipt.history[1].error.is_some());
    assert!(receipt.history[2].error.is_none());
    assert_eq!(
        receipt
            .history
            .iter()
            .map(|item| item.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "attempts 必须从 1 开始单调递增"
    );
    assert!(receipt.history.iter().all(|item| item.at.unix_millis() > 0));
}

#[tokio::test]
async fn gives_up_after_max_attempts() {
    let sender = ScriptedSender::new(vec![
        Err(AdapterError::transport("boom 1")),
        Err(AdapterError::transport("boom 2")),
        Err(AdapterError::transport("boom 3")),
        // 第 4 个脚本不应被消费（max_attempts = 3）。
        Ok(json!({ "message_id": 1 })),
    ]);
    let outbound = OutboundSender::start(sender.clone(), fast_policy(3), Arc::new(FixedClock), 8)
        .expect("sender starts");

    let error = outbound
        .enqueue("send_private_msg", json!({ "user_id": 2, "message": "hi" }))
        .await
        .expect_err("all attempts fail");
    assert_eq!(sender.calls(), 3, "不得超过 max_attempts");
    assert!(
        error.to_string().contains("3 attempt"),
        "错误里要写清尝试次数: {error}"
    );
    assert!(
        matches!(error, AdapterError::Transport { .. }),
        "重试耗尽按传输错误上报: {error:?}"
    );
}

#[tokio::test]
async fn jobs_are_processed_serially_in_enqueue_order() {
    let sender = ScriptedSender::new(vec![
        Ok(json!({ "message_id": 1 })),
        Ok(json!({ "message_id": 2 })),
        Ok(json!({ "message_id": 3 })),
    ]);
    let outbound = OutboundSender::start(sender.clone(), fast_policy(1), Arc::new(FixedClock), 8)
        .expect("sender starts");

    let first = outbound
        .enqueue("send_group_msg", json!({ "group_id": 1, "message": "a" }))
        .await
        .expect("first");
    let second = outbound
        .enqueue("send_group_msg", json!({ "group_id": 1, "message": "b" }))
        .await
        .expect("second");
    let third = outbound
        .enqueue("send_group_msg", json!({ "group_id": 1, "message": "c" }))
        .await
        .expect("third");

    assert_eq!(
        [first.message_id, second.message_id, third.message_id],
        [Some(1), Some(2), Some(3)],
        "同一队列必须按入队顺序处理（每会话串行）"
    );
    assert_eq!(
        sender.seen_actions(),
        vec!["send_group_msg"; 3],
        "action 名字要原样透传"
    );
}

#[tokio::test]
async fn zero_capacity_is_rejected() {
    let sender = ScriptedSender::new(Vec::new());
    let error = OutboundSender::start(sender, fast_policy(1), Arc::new(FixedClock), 0)
        .expect_err("capacity 0 must be rejected");
    assert!(
        matches!(error, AdapterError::Unsupported { .. }),
        "{error:?}"
    );
}
