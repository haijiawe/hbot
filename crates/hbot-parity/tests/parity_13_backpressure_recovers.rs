//! parity #13 — 队列满时给背压（不丢消息），并且**放行之后能继续处理**。
//!
//! `docs/04-implementation-plan.md` §4 第 13 行「现状」= 部分：已有
//! `full_queue_backpressures_without_losing_messages`（只证了「满时报错」）。**缺**的是
//! 「背压之后还能继续跑」——这正好是 AGENTS.md §7.1 第 20 条记的那个坑：
//! `crates/hbot-bot/src/queue.rs` 曾经**只在全局在跑数归零时**唤醒等队列的读者，
//! 于是「别的会话一直忙着」的时候，被背压挡住的读者永远不会被唤醒（假死锁）。
//!
//! 两个用例分别守两段：
//! * `backpressure_on_full_session_reports_timeout_and_recovers` —— 入口那侧
//!   （`supervisor.inject` 返回 `-32004 timeout`，消息已落库不丢，空出位置后继续处理）；
//! * `blocked_enqueue_wakes_while_another_session_is_wedged` —— 读者那侧
//!   （另一个会话的 handler 永不返回 ⇒ 全局在跑数永不归零，被挡住的读必须照样被唤醒）。
//!
//! 被验证的层：真 supervisor + 真每会话有界队列 + 真 SQLite + 真 outbox。假货只有
//! 平台适配器和「用户代码」（`RecordingExecutor`）。**队列没有 mock**。

mod support;

use std::sync::Arc;

use hbot_bot::{ConversationRef, InjectOutcome, QueueConfig};
use hbot_core::{BotId, ConversationKind, HbotError, ProjectId};
use support::{
    DEADLINE, FakeAdapter, RecordingExecutor, ReplyRelay, TempDir, bot_spec, build_supervisor,
    group_event, open_store, seed_project, unprocessed_inbound_ids, wait_until,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const BOT_UUID: u128 = 0xB0D;
const PROJECT_UUID: u128 = 0x5151_000D;
const CONVERSATION_A: &str = "90013";
const CONVERSATION_B: &str = "90014";
/// 会话容量：小到两条就满，便于精确摆出背压。
const SESSION_CAPACITY: usize = 2;

#[tokio::test]
async fn backpressure_on_full_session_reports_timeout_and_recovers() {
    if tokio::time::timeout(DEADLINE, case_backpressure())
        .await
        .is_err()
    {
        panic!("parity_13/背压：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

#[tokio::test]
async fn blocked_enqueue_wakes_while_another_session_is_wedged() {
    if tokio::time::timeout(DEADLINE, case_liveness())
        .await
        .is_err()
    {
        panic!("parity_13/唤醒：{DEADLINE:?} 内没有跑完（读侧没被唤醒 = 假死锁回来了）");
    }
}

/// 入口侧：满了必须报 `-32004 timeout`（不是丢），空出位置后新消息继续被处理。
async fn case_backpressure() {
    let dir = TempDir::new("13-backpressure");
    let store = open_store(&dir);
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    seed_project(&store, project_id, "E:\\hbot-parity-13").await;

    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let adapter = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION_A,
        ConversationKind::Group,
    )]);
    let executor = RecordingExecutor::new();
    // 一条都不放行：让「在跑」的那条一直占着位置，把队列稳定地压满。
    let hold = Arc::new(Semaphore::new(0));
    executor.hold_with(Arc::clone(&hold));
    let (replies_tx, replies_rx) = tokio::sync::mpsc::channel(64);
    executor.with_replies(replies_tx);

    let supervisor = build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        executor.clone(),
        bot_spec(bot_id, project_id, "parity"),
        |deps| {
            deps.queue_config = QueueConfig {
                session_capacity: SESSION_CAPACITY,
                max_in_flight: 8,
            };
        },
    );
    let relay = ReplyRelay::spawn(replies_rx, Arc::clone(&supervisor));
    supervisor.start().await.expect("supervisor 启动");

    // ① 填满：第 1 条进 handler（卡住），第 2 条排着。容量 2 到顶。
    for index in 0..SESSION_CAPACITY {
        let outcome = supervisor
            .inject(group_event(
                CONVERSATION_A,
                &format!("evt-fill-{index}"),
                "fill",
            ))
            .await
            .expect("填充消息应当被接受");
        assert!(matches!(outcome, InjectOutcome::Queued { .. }));
    }
    wait_until("在跑数到位（第 1 条已经进 handler）", || {
        executor.running() == 1 && supervisor.queue_depth() == SESSION_CAPACITY
    })
    .await;

    // ② 第 3 条：必须是背压错误（-32004 timeout），不是「悄悄丢掉」也不是「无限等」。
    let overflow = supervisor
        .inject(group_event(CONVERSATION_A, "evt-overflow-002", "overflow"))
        .await
        .expect_err("队列满时 inject 必须报背压，而不是接受");
    match &overflow {
        HbotError::Timeout { .. } => {}
        other => panic!("背压应当报 HbotError::Timeout，实际是 {other:?}"),
    }
    assert_eq!(
        overflow.code(),
        -32004,
        "背压的错误码必须是 -32004（RPC 契约里的 timeout）：{overflow}"
    );
    assert!(
        overflow.to_string().contains("full"),
        "背压错误没说清是队列满：{overflow}"
    );

    // ③ 报错不等于丢：这条已经被落库（先落库再处理），重启补偿会（至少一次地）再处理它。
    let pending = unprocessed_inbound_ids(&store, bot_id).await;
    assert_eq!(
        pending,
        vec![
            "evt-fill-0".to_owned(),
            "evt-fill-1".to_owned(),
            "evt-overflow-002".to_owned(),
        ],
        "被背压拒绝的那条没有落库，或者落库顺序不对：{pending:?}"
    );

    // ④ 放行一条 → 位置空出来 → 后续消息必须能继续被处理（这就是「恢复」）。
    hold.add_permits(1);
    wait_until("放行后第 2 条进 handler", || {
        executor.seen().len() == 2
    })
    .await;

    let after = supervisor
        .inject(group_event(CONVERSATION_A, "evt-after-003", "after"))
        .await
        .expect("空出位置之后新消息必须被接受");
    assert!(
        matches!(after, InjectOutcome::Queued { .. }),
        "空出位置之后仍然拒绝入队（队列没恢复）"
    );

    // ⑤ 全部放行，队列跑空，正文顺序正确。
    hold.add_permits(16);
    wait_until("队列跑空", || supervisor.queue_depth() == 0).await;
    wait_until("三条被处理的消息都回复了", || {
        adapter.sent_texts().len() == 3
    })
    .await;

    assert_eq!(
        executor.external_ids(),
        vec![
            "evt-fill-0".to_owned(),
            "evt-fill-1".to_owned(),
            "evt-after-003".to_owned()
        ],
        "被背压拒绝的那条不该在本次运行里执行（它留给下次启动补偿），其余要按序执行"
    );
    assert_eq!(executor.max_running(), 1, "同一会话仍然只能有一条在跑");
    assert!(
        relay.errors().is_empty(),
        "回复搬运报了错：{:?}",
        relay.errors()
    );

    supervisor.stop().await.expect("supervisor 停止");
}

/// 读者侧（真正的回归）：会话 B 的 handler 卡死 ⇒ 全局在跑数永不归零；
/// 会话 A 满员时被挡住的读（supervisor 的读循环）在 A 空出位置后**必须**被唤醒。
///
/// 老实现只在 `total_depth == 0` 时 `notify_waiters()`，于是这里 A 的读永远醒不过来，
/// A 的第 3、4 条永远不执行 —— 本用例会带着 label 失败，而不是挂死。
async fn case_liveness() {
    let dir = TempDir::new("13-liveness");
    let store = open_store(&dir);
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    seed_project(&store, project_id, "E:\\hbot-parity-13b").await;

    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let adapter = FakeAdapter::new(vec![
        ConversationRef::new(CONVERSATION_A, ConversationKind::Group),
        ConversationRef::new(CONVERSATION_B, ConversationKind::Group),
    ]);
    let executor = RecordingExecutor::new();
    let hold = Arc::new(Semaphore::new(0));
    executor.hold_with(Arc::clone(&hold));
    // B 的 handler 永不返回：全局「在跑」数从此不可能归零。
    executor.wedge_conversation(CONVERSATION_B);

    let supervisor = build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        executor.clone(),
        bot_spec(bot_id, project_id, "parity"),
        |deps| {
            deps.queue_config = QueueConfig {
                session_capacity: SESSION_CAPACITY,
                max_in_flight: 8,
            };
            // 卡死的会话在收尾时必然撞上 drain 超时，把等待压到最短。
            deps.drain_timeout = std::time::Duration::from_millis(300);
            deps.shutdown_timeout = std::time::Duration::from_millis(300);
        },
    );
    supervisor.start().await.expect("supervisor 启动");

    // 读循环按顺序处理事件流：A1、A2 进 A 的队列（A1 进 handler 卡住），
    // B1 进 B 的队列（卡死），A3 撞上 A 满员 ⇒ **读循环就停在这里等唤醒**，
    // A4 还在适配器的通道里。
    for (conversation, external_id) in [
        (CONVERSATION_A, "evt-a-1"),
        (CONVERSATION_A, "evt-a-2"),
        (CONVERSATION_B, "evt-b-wedged"),
        (CONVERSATION_A, "evt-a-3"),
        (CONVERSATION_A, "evt-a-4"),
    ] {
        assert!(
            adapter.push(group_event(conversation, external_id, "x")),
            "投递 {external_id} 失败（事件通道没建好？）"
        );
    }

    // 先确认局面成立：A1 在跑、B1 在跑（卡死）、A3/A4 还没被执行。
    wait_until("A1、B1 都已进入 handler", || executor.running() >= 2).await;
    assert_eq!(
        executor.external_ids(),
        vec!["evt-a-1".to_owned(), "evt-b-wedged".to_owned()],
        "读循环的入场顺序不对：A3 应当在 A 满员处被挡住"
    );

    // 放行 4 个许可：够 A1..A4 依次跑完，但不够让卡死的 B1 结束
    // （B1 走的是 `pending()` 分支，根本不申请许可）。
    hold.add_permits(4);

    // A4 能被执行，等价于「A 的读在 A1/A2 释放时被唤醒过两次」——
    // 那两次的全局在跑数都 > 0（B1 永远卡着）。老实现会在这里超时失败。
    wait_until("被背压挡住的 A3、A4 最终被执行", || {
        let seen = executor.external_ids();
        seen.contains(&"evt-a-3".to_owned()) && seen.contains(&"evt-a-4".to_owned())
    })
    .await;

    let seen = executor.external_ids();
    assert_eq!(
        seen,
        vec![
            "evt-a-1".to_owned(),
            "evt-b-wedged".to_owned(),
            "evt-a-2".to_owned(),
            "evt-a-3".to_owned(),
            "evt-a-4".to_owned(),
        ],
        "A 会话仍然是先进先出，且被挡住的两条按序补上"
    );
    assert_eq!(
        executor.running(),
        1,
        "收尾时应当只剩卡死的 B1 在跑（A 的 4 条全部完成）"
    );

    // 卡死会话不该让 A 的消息被吞：A 的 4 条全部跑完了，所以只剩 B 那条还欠着
    // （`list_*_inbound` 只返回未处理的行，见 `unprocessed_inbound_ids` 的注释）。
    let pending = unprocessed_inbound_ids(&store, bot_id).await;
    assert_eq!(
        pending,
        vec!["evt-b-wedged".to_owned()],
        "A 的四条应当都已处理完、只剩卡死的 B 那条未处理：{pending:?}"
    );

    // 停止：卡死的 B1 必然让 drain 超时——那不是本用例的不变量，明确忽略；
    // 但必须确认它是**有界**返回（本用例整体还有 `DEADLINE` 兜底）。
    let stopped = supervisor.stop().await;
    assert!(
        stopped.is_ok() || matches!(stopped, Err(HbotError::Timeout { .. })),
        "停止只允许成功或 drain 超时，实际是 {stopped:?}"
    );
}
