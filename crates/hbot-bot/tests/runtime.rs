//! 端到端行为：真 SQLite + fake 平台适配器 + fake 执行器。
//!
//! 覆盖 `AGENTS.md` §4.1 的四条消息不变量（先落库、去重、每会话串行、有界背压），
//! 以及 §7.1 的启动顺序坑、§12 的关停排空与“连续失败降级且不自动重启”。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, TestExecutor, event, wait_until};
use hbot_bot::outbox::OutboxConfig;
use hbot_bot::queue::QueueConfig;
use hbot_bot::supervisor::StartupStep;
use hbot_bot::{ConversationRef, InjectOutcome, OutboundRequest, conversation_id_for};
use hbot_core::{BotId, BotStatus, ConversationKind, HbotError, InboundId};
use hbot_store::NewInbound;
use tokio::sync::Semaphore;
use uuid::Uuid;

fn bot_id() -> BotId {
    BotId::from_uuid(Uuid::from_u128(0xB07))
}

fn event_ids(seen: &[common::Seen]) -> Vec<String> {
    seen.iter().map(|item| item.external_id.clone()).collect()
}

/// 同一会话连发 100 条：执行顺序必须与入队顺序一致，且该会话内不并发。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_conversation_executes_in_arrival_order() {
    let fixture = Fixture::new(Vec::new());
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("start");

    for seq in 0..100_u32 {
        let outcome = supervisor
            .inject(event(
                "chat-1",
                &format!("evt-{seq:03}"),
                &format!("message {seq}"),
            ))
            .await
            .expect("inject");
        assert!(
            matches!(outcome, InjectOutcome::Queued { .. }),
            "first delivery must be queued, got {outcome:?}"
        );
    }

    wait_until("100 messages executed", || {
        supervisor.queue_depth() == 0 && fixture.executor.seen().len() == 100
    })
    .await;

    let expected: Vec<String> = (0..100_u32).map(|seq| format!("evt-{seq:03}")).collect();
    assert_eq!(event_ids(&fixture.executor.seen()), expected);
    assert_eq!(
        fixture.executor.max_running(),
        1,
        "one conversation must never run two handlers at once"
    );
    assert_eq!(
        fixture.executor.seen()[0].conversation_id,
        conversation_id_for(&bot_id(), "chat-1"),
        "queue key and inbound row must agree on the conversation id"
    );

    supervisor.stop().await.expect("stop");
}

/// 不同会话必须能并行（`max_in_flight` 内），否则长任务会互相饿死。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_conversations_execute_concurrently() {
    let fixture = Fixture::new(Vec::new());
    let gate = Arc::new(Semaphore::new(0));
    fixture.executor.set_gate(Arc::clone(&gate));
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("start");

    supervisor
        .inject(event("chat-a", "evt-a", "a"))
        .await
        .expect("inject a");
    supervisor
        .inject(event("chat-b", "evt-b", "b"))
        .await
        .expect("inject b");

    wait_until("both handlers blocked in the executor", || {
        fixture.executor.running() == 2
    })
    .await;

    gate.add_permits(2);
    wait_until("both handled", || fixture.executor.seen().len() == 2).await;
    assert_eq!(fixture.executor.max_running(), 2);
    supervisor.stop().await.expect("stop");
}

/// 队列满 → 背压（`Timeout`），但**消息已经落库**：同 `external_id` 重投得到 `Duplicate`，
/// 既不丢失也不重复执行。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_queue_backpressures_without_losing_messages() {
    let fixture = Fixture::new(Vec::new());
    let gate = Arc::new(Semaphore::new(0));
    fixture.executor.set_gate(Arc::clone(&gate));

    let mut deps = fixture.deps();
    deps.queue_config = QueueConfig {
        session_capacity: 2,
        max_in_flight: 4,
    };
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    for seq in 0..2_u32 {
        supervisor
            .inject(event("chat-full", &format!("evt-{seq}"), "queued"))
            .await
            .expect("queue has room");
    }

    let error = supervisor
        .inject(event("chat-full", "evt-2", "rejected"))
        .await
        .expect_err("third message exceeds capacity 2");
    assert!(
        matches!(error, HbotError::Timeout { .. }),
        "backpressure must be reported as Timeout, got {error:?}"
    );
    assert_eq!(error.code(), -32004);

    // 已落库：同一事件重投不会再次执行，而是被去重 ACK。
    let replay = supervisor
        .inject(event("chat-full", "evt-2", "rejected"))
        .await
        .expect("replay is acknowledged");
    assert!(
        matches!(replay, InjectOutcome::Duplicate { .. }),
        "backpressured message must already be persisted, got {replay:?}"
    );

    gate.add_permits(4);
    wait_until("queued messages executed", || {
        fixture.executor.seen().len() == 2
    })
    .await;
    assert_eq!(
        event_ids(&fixture.executor.seen()),
        vec!["evt-0".to_owned(), "evt-1".to_owned()],
        "the rejected message must not be executed twice"
    );
    supervisor.stop().await.expect("stop");
}

/// 重复投递只执行一次；去重状态跨重启仍然成立（靠 `inbound` 唯一约束，不靠内存表）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_delivery_executes_once_even_across_restart() {
    let fixture = Fixture::new(Vec::new());
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());

    supervisor.start().await.expect("first start");
    let first = supervisor
        .inject(event("chat-dup", "evt-dup", "hello"))
        .await
        .expect("inject");
    assert!(matches!(first, InjectOutcome::Queued { .. }));

    let again = supervisor
        .inject(event("chat-dup", "evt-dup", "hello"))
        .await
        .expect("duplicate acknowledged");
    assert!(matches!(again, InjectOutcome::Duplicate { .. }));
    wait_until("first delivery executed", || {
        fixture.executor.seen().len() == 1
    })
    .await;

    supervisor.stop().await.expect("stop");
    supervisor.start().await.expect("restart");

    let after_restart = supervisor
        .inject(event("chat-dup", "evt-dup", "hello"))
        .await
        .expect("duplicate acknowledged after restart");
    assert!(
        matches!(after_restart, InjectOutcome::Duplicate { .. }),
        "dedupe must survive a restart, got {after_restart:?}"
    );

    supervisor
        .inject(event("chat-dup", "evt-new", "world"))
        .await
        .expect("new event queued");
    wait_until("new event executed", || fixture.executor.seen().len() == 2).await;
    assert_eq!(
        event_ids(&fixture.executor.seen()),
        vec!["evt-dup".to_owned(), "evt-new".to_owned()]
    );
    supervisor.stop().await.expect("stop");
}

/// 启动顺序：bot 行 → conversations 行 → 订阅 → 恢复 outbox → 开始读取。
///
/// 反证：启动前写 `inbound` 会因外键失败；`conversations` 行就绪后同一条写入成功。
/// 这保证“订阅一建立就有可写的 FK 目标”，不会在启动窗口丢消息（`AGENTS.md` §7.1）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_orders_rows_before_subscription() {
    let declared = vec![ConversationRef::new("g1", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    let conversation_id = conversation_id_for(&bot_id(), "g1");
    let probe_id = InboundId::from_uuid(Uuid::from_u128(0x1234));
    let probe = NewInbound {
        id: probe_id,
        bot_id: bot_id(),
        external_id: "evt-probe".to_owned(),
        conversation_id,
        payload_json: "{}".to_owned(),
        received_at: 0,
    };

    let before = fixture
        .store
        .insert_inbound_if_new(probe.clone())
        .expect_err("no bots/conversations rows yet");
    assert!(
        before.to_string().contains("FOREIGN KEY"),
        "expected a foreign key failure before start, got {before}"
    );
    assert_eq!(fixture.adapter.subscribe_calls(), 0);

    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("start");

    let steps: Vec<&str> = supervisor
        .snapshot()
        .startup_steps
        .iter()
        .map(|step| step.as_str())
        .collect();
    assert_eq!(
        steps,
        vec![
            StartupStep::BotRow.as_str(),
            StartupStep::Conversations.as_str(),
            StartupStep::Subscribe.as_str(),
            StartupStep::RecoverOutbox.as_str(),
            StartupStep::Reading.as_str(),
        ],
        "startup order is a hard requirement"
    );
    assert_eq!(fixture.adapter.subscribe_calls(), 1);

    // conversations 行已在订阅之前写好：同一条入站写入现在能过外键。
    let after = fixture
        .store
        .insert_inbound_if_new(probe)
        .expect("foreign keys satisfied after startup");
    assert_eq!(after, Some(probe_id));

    // 适配器推送（而不是直接 inject）的路径也走同一条链路。
    assert!(
        fixture
            .adapter
            .push(event("g1", "evt-pushed", "from adapter")),
        "adapter subscription must be active"
    );
    wait_until("adapter event executed", || {
        fixture.executor.seen().len() == 1
    })
    .await;
    assert_eq!(event_ids(&fixture.executor.seen()), vec!["evt-pushed"]);

    supervisor.stop().await.expect("stop");
}

/// 停止先排空在途消息，再拒绝新消息。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_drains_in_flight_and_rejects_new_messages() {
    let fixture = Fixture::new(Vec::new());
    let mut deps = fixture.deps();
    deps.drain_timeout = Duration::from_secs(5);
    deps.shutdown_timeout = Duration::from_secs(5);
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    for seq in 0..5_u32 {
        supervisor
            .inject(event("chat-stop", &format!("evt-{seq}"), "work"))
            .await
            .expect("queued");
    }
    supervisor.stop().await.expect("drained stop");

    assert_eq!(
        fixture.executor.seen().len(),
        5,
        "stop must drain accepted messages"
    );
    assert_eq!(supervisor.status(), BotStatus::Stopped);

    let error = supervisor
        .inject(event("chat-stop", "evt-late", "too late"))
        .await
        .expect_err("stopped bot must reject injections");
    assert!(matches!(error, HbotError::Conflict { .. }));
    assert_eq!(error.code(), -32003);
}

/// 连续 3 次 handler 崩溃 → `Degraded`，且**不自动重启**（订阅只建立过一次）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consecutive_handler_failures_degrade_without_auto_restart() {
    let fixture = Fixture::new(Vec::new());
    fixture.executor.fail_next(u32::MAX);

    let mut deps = fixture.deps();
    deps.failure_threshold = 3;
    deps.drain_timeout = Duration::from_secs(2);
    deps.shutdown_timeout = Duration::from_secs(2);
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    for seq in 0..3_u32 {
        supervisor
            .inject(event("chat-degrade", &format!("evt-{seq}"), "boom"))
            .await
            .expect("queued");
    }

    wait_until("bot degraded after three consecutive failures", || {
        supervisor.status() == BotStatus::Degraded
    })
    .await;

    let snapshot = supervisor.snapshot();
    assert_eq!(snapshot.consecutive_failures, 3);
    assert!(snapshot.degraded);

    // 降级后不再自动拉起：订阅次数保持 1，状态稳定在 Degraded。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fixture.adapter.subscribe_calls(),
        1,
        "degraded bot must not auto-restart"
    );
    assert_eq!(supervisor.status(), BotStatus::Degraded);

    // Degraded 时不再接受读链路的新消息（显式 start 才是恢复路径）。
    let error = supervisor
        .inject(event("chat-degrade", "evt-more", "boom"))
        .await
        .expect_err("degraded bot is not reading");
    assert!(matches!(error, HbotError::Conflict { .. }));

    supervisor.stop().await.expect("stop from degraded");
    assert_eq!(supervisor.status(), BotStatus::Stopped);
}

/// 出站：先落库，再由 outbox 串行发送；失败按退避重试，上限后停手。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_is_persisted_then_retried_until_sent() {
    let declared = vec![ConversationRef::new("chat-out", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(2);

    let mut deps = fixture.deps();
    deps.outbox_config = OutboxConfig {
        max_attempts: 5,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        capacity: 16,
        send_timeout: Duration::from_secs(2),
    };
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    let outbound_id = supervisor
        .send(OutboundRequest {
            conversation: ConversationRef::new("chat-out", ConversationKind::Group),
            text: "reply".to_owned(),
        })
        .await
        .expect("persisted and queued");

    wait_until("outbound delivered after two failures", || {
        fixture.adapter.sent().len() == 1
    })
    .await;

    assert_eq!(
        fixture.adapter.send_calls(),
        3,
        "two failures then one success"
    );
    let sent = fixture.adapter.sent();
    assert_eq!(sent[0].id, outbound_id);
    assert_eq!(sent[0].text, "reply");
    assert_eq!(sent[0].conversation.external_id, "chat-out");

    let stats = supervisor.snapshot().outbox.expect("outbox running");
    assert_eq!(stats.sent, 1);
    assert_eq!(stats.retried, 2);
    assert_eq!(stats.failed, 0);

    supervisor.stop().await.expect("stop");
}

/// 适配器发送始终失败时，超过 `max_attempts` 后停止重试（行保持 pending 等下次续发），
/// 并且不会把错误吞掉：计数器记 `failed`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_gives_up_after_max_attempts() {
    let declared = vec![ConversationRef::new("chat-dead", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(u32::MAX);

    let mut deps = fixture.deps();
    deps.outbox_config = OutboxConfig {
        max_attempts: 2,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        capacity: 16,
        send_timeout: Duration::from_secs(2),
    };
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    supervisor
        .send(OutboundRequest {
            conversation: ConversationRef::new("chat-dead", ConversationKind::Group),
            text: "never delivered".to_owned(),
        })
        .await
        .expect("persisted");

    wait_until("give up after max attempts", || {
        supervisor
            .snapshot()
            .outbox
            .is_some_and(|stats| stats.failed == 1)
    })
    .await;

    assert_eq!(fixture.adapter.send_calls(), 2, "max_attempts = 2");
    assert!(fixture.adapter.sent().is_empty());
    let stats = supervisor.snapshot().outbox.expect("outbox running");
    assert_eq!(stats.sent, 0);
    assert_eq!(stats.failed, 1);

    supervisor.stop().await.expect("stop");
}

/// 测试脚手架自身的契约：`TestExecutor` 的并发计数不会因失败路径泄漏。
#[test]
fn executor_fake_does_not_leak_running_counter() {
    let executor = TestExecutor::new();
    assert_eq!(executor.running(), 0);
    assert_eq!(executor.max_running(), 0);
}
