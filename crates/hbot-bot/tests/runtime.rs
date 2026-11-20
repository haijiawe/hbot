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

/// 直接往库里写一条**未处理**的入站行，模拟"上一个进程收了消息、还没处理就死了"。
///
/// 走 store 而不是 adapter：重放要覆盖的正是"库里已经有行、但 `processed_at` 还是
/// NULL"这个状态，adapter 那条路会先经过 dedupe（那是另一条不变量的测试）。
fn seed_unprocessed(
    fixture: &Fixture,
    seq: u128,
    external_id: &str,
    conversation: &str,
    received_at: i64,
    text: &str,
) -> InboundId {
    let id = InboundId::from_uuid(Uuid::from_u128(
        0x5eed_0000_0000_0000_0000_0000_0000_0000 | seq,
    ));
    fixture
        .store
        .insert_inbound_if_new(NewInbound {
            id,
            bot_id: bot_id(),
            external_id: external_id.to_owned(),
            conversation_id: conversation_id_for(&bot_id(), conversation),
            payload_json: serde_json::json!({ "text": text }).to_string(),
            received_at,
        })
        .expect("seed unprocessed inbound");
    id
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

// ---------------------------------------------------------------------------
// D5：启动时重放未处理的 inbound（崩溃/关停窗口里的消息不能丢）
// ---------------------------------------------------------------------------

/// 库里留着 `processed_at IS NULL` 的行：重启后必须被执行，并补上 `processed_at`。
///
/// 这修的是"消息永久丢掉"：读循环在 `queues.enqueue` 上遇到 `Closed` 直接 break，
/// 那时的消息已经落库但永远不会再被读——只有启动重放能把它们捡回来。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unprocessed_inbound_is_replayed_on_start() {
    // 声明会话：重放要用 `inbound.conversation_id` 指向一行真实会话（外键）。
    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-replay",
        ConversationKind::Group,
    )]);
    // 先起停一次：把 `bots` / `conversations` 行准备好（模拟"进程重启"）。
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("first start");
    supervisor.stop().await.expect("stop");

    seed_unprocessed(&fixture, 1, "evt-replay", "chat-replay", 1_000, "hello");
    assert_eq!(
        fixture
            .store
            .list_unprocessed_inbound(10)
            .expect("list")
            .len(),
        1,
        "前提：库里确实有一行未处理"
    );

    supervisor.start().await.expect("restart");
    // 等两个条件，不是只等执行：handler 链路是「先 executor.on_message，再
    // mark_processed」（`RecordingHandler`），所以 seen 有记录那一刻
    // `processed_at` 可能还没落库。只等 seen 会让下面的断言偶发为假（CI 的 windows 上就红过）。
    wait_until("replayed message executed and marked processed", || {
        fixture.executor.seen().len() == 1
            && fixture
                .store
                .list_unprocessed_inbound(10)
                .expect("list")
                .is_empty()
    })
    .await;

    let seen = fixture.executor.seen();
    assert_eq!(event_ids(&seen), vec!["evt-replay".to_owned()]);
    assert_eq!(seen[0].text.as_deref(), Some("hello"));
    assert!(
        fixture
            .store
            .list_unprocessed_inbound(10)
            .expect("list")
            .is_empty(),
        "重放成功后必须写 processed_at，否则下次启动会重复执行"
    );
    supervisor.stop().await.expect("stop");
}

/// 多条未处理行按 `received_at` 升序执行（不是按 UUID，也不是按插入顺序）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_inbound_runs_in_received_order() {
    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-order",
        ConversationKind::Group,
    )]);
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("first start");
    supervisor.stop().await.expect("stop");

    // 插入顺序与 received_at 顺序**故意相反**：只有真按时间排才会得到 1,2,3。
    seed_unprocessed(&fixture, 11, "evt-third", "chat-order", 3_000, "third");
    seed_unprocessed(&fixture, 12, "evt-first", "chat-order", 1_000, "first");
    seed_unprocessed(&fixture, 13, "evt-second", "chat-order", 2_000, "second");

    supervisor.start().await.expect("restart");
    wait_until("all replayed messages executed", || {
        fixture.executor.seen().len() == 3
    })
    .await;
    assert_eq!(
        event_ids(&fixture.executor.seen()),
        vec![
            "evt-first".to_owned(),
            "evt-second".to_owned(),
            "evt-third".to_owned()
        ],
        "重放必须按产生顺序，否则用户看到的是打乱的对话"
    );
    supervisor.stop().await.expect("stop");
}

/// 超过一个取批上限也不许丢：重放要一直追到没有未处理行为止。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_continues_past_one_batch_without_losing_messages() {
    // 比 `REPLAY_BATCH_SIZE`（256）多几条：确保真的走了第二批。
    const EXTRA: u64 = 8;
    let total =
        usize::try_from(hbot_bot::supervisor::REPLAY_BATCH_SIZE).expect("fits") + EXTRA as usize;

    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-batch",
        ConversationKind::Group,
    )]);
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("first start");
    supervisor.stop().await.expect("stop");

    for seq in 0..total {
        seed_unprocessed(
            &fixture,
            u128::try_from(seq).expect("fits") + 100,
            &format!("evt-batch-{seq:04}"),
            "chat-batch",
            i64::try_from(seq).expect("small") + 1_000,
            &format!("message {seq}"),
        );
    }

    supervisor.start().await.expect("restart");
    // 等两个条件，不是只等执行：`RecordingHandler` 是「先 executor.on_message，再
    // mark_processed」，`seen` 满额那一刻最后一条的 `processed_at` 可能还没落库。
    // 只等 seen 会让下面那条"剩余行必须继续重放"偶发为假（CI 的 ubuntu 上就红过）。
    wait_until(
        "every replayed message executed and marked processed",
        || {
            fixture.executor.seen().len() == total
                && fixture
                    .store
                    .list_unprocessed_inbound(1_000)
                    .expect("list")
                    .is_empty()
        },
    )
    .await;
    assert!(
        fixture
            .store
            .list_unprocessed_inbound(1_000)
            .expect("list")
            .is_empty(),
        "一批装不下的剩余行必须在后续批次里继续重放"
    );
    assert_eq!(
        fixture.executor.seen().len(),
        total,
        "不许因为分批而丢掉消息"
    );
    supervisor.stop().await.expect("stop");
}

/// 已经处理过的行绝不能被重放（否则每次重启都重复回复一遍历史消息）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processed_inbound_is_not_replayed() {
    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-done",
        ConversationKind::Group,
    )]);
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("first start");
    supervisor.stop().await.expect("stop");

    let id = seed_unprocessed(&fixture, 21, "evt-done", "chat-done", 1_000, "old");
    assert!(
        fixture
            .store
            .mark_inbound_processed(&id, 2_000)
            .expect("mark processed"),
        "前提：这一行要被标记为已处理"
    );

    supervisor.start().await.expect("restart");
    // 没有实时消息，重放也不该捡它：给重放留出跑完的窗口。
    wait_until("startup finished", || {
        supervisor.snapshot().startup_steps.len() >= 6
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        fixture.executor.seen().is_empty(),
        "processed_at 非 NULL 的行不许重放，实际执行了 {:?}",
        event_ids(&fixture.executor.seen())
    );
    supervisor.stop().await.expect("stop");
}

/// 重放与实时事件并存：同一个 `external_id` 不会被执行两次（去重仍由库保证）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_and_live_event_do_not_execute_the_same_message_twice() {
    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-both",
        ConversationKind::Group,
    )]);
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("first start");
    supervisor.stop().await.expect("stop");

    seed_unprocessed(&fixture, 31, "evt-both", "chat-both", 1_000, "from db");

    supervisor.start().await.expect("restart");
    wait_until("replayed", || fixture.executor.seen().len() == 1).await;

    // 平台把同一条消息又推了一遍：必须被 dedupe 挡掉，不能执行第二次。
    let outcome = supervisor
        .inject(event("chat-both", "evt-both", "from adapter"))
        .await
        .expect("duplicate acknowledged");
    assert!(
        matches!(outcome, InjectOutcome::Duplicate { .. }),
        "重放过的消息再来一次必须是 Duplicate，got {outcome:?}"
    );
    assert_eq!(
        fixture.executor.seen().len(),
        1,
        "同一 external_id 只许执行一次"
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
            // D5：未处理入站的重放发生在"读事件循环已经起来"之后、"Reading"之前——
            // 这样补偿消息和实时消息走同一条队列，实时流量不会因为重放而丢。
            StartupStep::ReplayInbound.as_str(),
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
        // 累计预算远大于本次运行预算：本用例只关心"两次失败后第三次发出去"，
        // 不想让它在这个距离上撞到死信线（见 `tests/outbox_exhaustion.rs`）。
        max_send_attempts: 10,
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
            reply_to: None,
        })
        .await
        .expect("persisted and queued");

    // 两个条件都要等：`adapter.sent()` 是假适配器自己的记录（`send` 一返回就有），
    // 而 `snapshot().outbox.sent` 是 outbox 在 `mark_outbound_sent` 落库**之后**才加的
    // 计数（`crates/hbot-bot/src/outbox.rs` 的 `Ok(())` 分支）。只等前者，下一段
    // `assert_eq!(stats.sent, 1)` 会在「已经发出去、但还没记账」那一瞬间读到 0 ——
    // 2026-11-20 的 CI 上就是这样红的（坑 121 的同族）。
    wait_until("outbound delivered after two failures", || {
        fixture.adapter.sent().len() == 1
            && supervisor
                .snapshot()
                .outbox
                .is_some_and(|stats| stats.sent == 1)
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
        // 累计预算还没到（2 < 10），所以这一轮用完就停在 pending，留给下次启动续发 ——
        // 这正是本用例要钉的"本次运行预算"语义。真的转死信在 `tests/outbox_exhaustion.rs`。
        max_send_attempts: 10,
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
            reply_to: None,
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

/// 适配器订阅被关掉：读循环必须报「意外结束」，而不是安静地不再收消息。
///
/// 这条钉的是 `docs/07` §4.6 的前置：`onStop(crash)` 之前必须有人能看出
/// 「这个 bot 已经瞎了」。订阅关闭不影响任何计数器，从外面看与「平台很安静」完全一样，
/// 所以判据只能由读循环自己给出。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_subscription_is_reported_as_an_unexpected_exit() {
    let fixture = Fixture::new(vec![ConversationRef::new(
        "chat-closed",
        ConversationKind::Group,
    )]);
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("start");
    assert_eq!(supervisor.status(), BotStatus::Running);

    // 平台侧断线 = 发送端被 drop。
    fixture.adapter.close_subscription();

    let exit = tokio::time::timeout(Duration::from_secs(5), supervisor.wait_read_loop_exit())
        .await
        .expect("读循环必须在订阅关闭后退出，而不是挂着");
    assert_eq!(
        exit,
        hbot_bot::ReadLoopExit::SubscriptionClosed,
        "订阅关闭必须报成 SubscriptionClosed，实际 {exit:?}"
    );
    assert!(exit.is_unexpected(), "订阅关闭是意外结束，不是正常停机");
    assert_eq!(
        supervisor.status(),
        BotStatus::Degraded,
        "读循环意外结束后不能再对外报 Running：它已经收不到任何消息了"
    );

    // 收尾仍可正常进行（`crash` 路径就是先拿到上面这个原因，再调 stop）。
    supervisor.stop().await.expect("stop");
    assert_eq!(supervisor.status(), BotStatus::Stopped);
}

/// 正常停机报 `StopRequested`，调用方不得据此触发 `crash` 收尾。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_normal_stop_is_not_reported_as_an_unexpected_exit() {
    let fixture = Fixture::new(Vec::new());
    let supervisor = fixture.supervisor(bot_id(), fixture.deps());
    supervisor.start().await.expect("start");

    // 先取走读循环的退出原因：`stop()` 会等读循环结束，所以顺序不能反过来。
    let waiter = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.wait_read_loop_exit().await })
    };
    supervisor.stop().await.expect("stop");
    let exit = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("waiter 必须在停机后返回")
        .expect("join");
    assert_eq!(exit, hbot_bot::ReadLoopExit::StopRequested);
    assert!(!exit.is_unexpected());
}

/// 测试脚手架自身的契约：`TestExecutor` 的并发计数不会因失败路径泄漏。
#[test]
fn executor_fake_does_not_leak_running_counter() {
    let executor = TestExecutor::new();
    assert_eq!(executor.running(), 0);
    assert_eq!(executor.max_running(), 0);
}
