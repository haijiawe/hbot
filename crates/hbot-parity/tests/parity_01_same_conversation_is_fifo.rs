//! parity #01 — 同一会话连发 N 条：回复不乱序，且同一会话没有两条 handler 同时在跑。
//!
//! `docs/04-implementation-plan.md` §4 第 1 行「现状」：只有 `hbot-bot` 的
//! `same_session_is_fifo`（执行器是假的、只到队列层）。**缺**的是经 supervisor +
//! 真实队列 + 真实 outbox + 适配器的端到端证据——本文件补那一段。
//!
//! 被验证的层：真实 `Supervisor`（含 `SessionQueues`）、真实 `Store`(SQLite)、
//! 真实 `Outbox`。被替换的「外部世界」：平台适配器（`FakeAdapter`）与用户脚本
//! （`RecordingExecutor` 取代 QuickJS handler，因为要数并发只能在 handler 内部数）。

mod support;

use std::sync::Arc;

use hbot_bot::{ConversationRef, InjectOutcome, QueueConfig};
use hbot_core::{BotId, ConversationKind, ProjectId};
use support::{
    DEADLINE, FakeAdapter, RecordingExecutor, ReplyRelay, TempDir, bot_spec, build_supervisor,
    group_event, open_store, seed_project, wait_until,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const BOT_UUID: u128 = 0xB01;
const PROJECT_UUID: u128 = 0x5151_0001;
const CONVERSATION: &str = "90001";
/// 连发条数。够暴露乱序，又不至于把用例拖到秒级。
const MESSAGES: usize = 10;

#[tokio::test]
async fn same_conversation_burst_is_fifo_and_never_concurrent() {
    if tokio::time::timeout(DEADLINE, case()).await.is_err() {
        panic!("parity_01：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn case() {
    let dir = TempDir::new("01-fifo");
    let store = open_store(&dir);
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    seed_project(&store, project_id, "E:\\hbot-parity-01").await;

    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let adapter = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION,
        ConversationKind::Group,
    )]);

    let executor = RecordingExecutor::new();
    // 闸门给满许可 = 每条都立刻放行，但 handler 内部保留一个并发观测窗：
    // 假如队列允许同一会话并发，第二条会在这两个调度点里进来，max_running 变 2。
    executor.hold_with(Arc::new(Semaphore::new(100)));
    let (replies_tx, replies_rx) = tokio::sync::mpsc::channel(64);
    executor.with_replies(replies_tx);

    let supervisor = build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        executor.clone(),
        bot_spec(bot_id, project_id, "parity"),
        |deps| {
            // 容量够放下全部 10 条：本用例要证的是「顺序 + 串行」，不是背压（那是 #13）。
            deps.queue_config = QueueConfig {
                session_capacity: MESSAGES,
                max_in_flight: MESSAGES,
            };
        },
    );
    let relay = ReplyRelay::spawn(replies_rx, Arc::clone(&supervisor));

    supervisor.start().await.expect("supervisor 启动");

    for index in 0..MESSAGES {
        let external_id = format!("evt-{index:03}");
        let text = format!("m-{index:03}");
        // 走真实入口：supervisor.inject → 先落库 → 每会话有界队列（不是 mock 队列）。
        let outcome = supervisor
            .inject(group_event(CONVERSATION, &external_id, &text))
            .await
            .expect("入队");
        assert!(
            matches!(outcome, InjectOutcome::Queued { .. }),
            "第 {index} 条应当 Queued，实际不是（队列被意外拒绝）"
        );
    }

    wait_until("10 条回复都经 outbox 发到适配器", || {
        adapter.sent_texts().len() == MESSAGES
    })
    .await;
    wait_until("队列跑空", || supervisor.queue_depth() == 0).await;

    let expected_ids: Vec<String> = (0..MESSAGES)
        .map(|index| format!("evt-{index:03}"))
        .collect();
    let expected_replies: Vec<String> = (0..MESSAGES)
        .map(|index| format!("reply-m-{index:03}"))
        .collect();

    // ① 执行顺序 == 发送顺序（同会话串行 FIFO）
    assert_eq!(
        executor.external_ids(),
        expected_ids,
        "同一会话的执行顺序与发送顺序不一致"
    );
    // ② 没有两条同时在跑
    assert_eq!(
        executor.max_running(),
        1,
        "同一会话出现了并发的 handler（峰值并发 {}），违反 AGENTS.md §0「每会话串行」",
        executor.max_running()
    );
    // ③ 回复顺序 == 发送顺序（经真实 outbox 之后仍然一致）
    assert_eq!(
        adapter.sent_texts(),
        expected_replies,
        "回复在 outbox/适配器这一侧乱序了"
    );
    // ④ 每条回复都绑定了触发它的平台消息
    assert_eq!(
        adapter.sent_reply_to(),
        expected_ids
            .iter()
            .map(|id| Some(id.clone()))
            .collect::<Vec<_>>(),
        "被动回复的 reply_to 与触发消息不一致"
    );
    assert!(
        relay.errors().is_empty(),
        "回复没能进入 outbox：{:?}",
        relay.errors()
    );

    supervisor.stop().await.expect("supervisor 停止");
}
