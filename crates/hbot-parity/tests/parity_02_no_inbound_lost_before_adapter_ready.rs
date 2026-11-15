//! parity #02 — 启动/重连窗口不丢消息：`subscribe` 之前落库的入站会被启动补偿补上，
//! `subscribe` 之后到达的不会重复执行。
//!
//! `docs/04-implementation-plan.md` §4 第 2 行「现状」：已有
//! `startup_orders_rows_before_subscription`（只断言了启动步骤顺序）。**缺**的是
//! 「适配器未就绪时进库的入站不丢」+「就绪后不重复执行」这两条一起证。
//!
//! 窗口怎么造：`Supervisor::start` 的顺序是
//! `bot 行 → conversations → subscribe → recover outbox → replay → reading`
//! （`crates/hbot-bot/src/supervisor.rs` 的 `start`）。用一个「延迟就绪」的假适配器
//! 把 `subscribe()` 卡住，此时 bot 行与会话行已经在库里（外键可满足），事件流却还没
//! 交出去——测试就在这个窗口往库里插一条未处理入站，然后放行。

mod support;

use std::sync::Arc;

use hbot_bot::{ConversationRef, conversation_id_for};
use hbot_core::{BotId, ConversationKind, InboundId, ProjectId};
use hbot_store::NewInbound;
use support::{
    DEADLINE, FakeAdapter, RecordingExecutor, SubscribeGate, TempDir, blocking, bot_spec,
    build_supervisor, group_event, open_store, seed_project, wait_until,
};
use uuid::Uuid;

const BOT_UUID: u128 = 0xB02;
const PROJECT_UUID: u128 = 0x5151_0002;
const CONVERSATION: &str = "90002";
const REPLAYED: &str = "evt-replay-001";
const ARRIVED_AFTER_SUBSCRIBE: &str = "evt-live-002";
/// 哨兵：它被执行就说明前面那条重复投递已经处理完（同一条 mpsc 是 FIFO）。
const SENTINEL: &str = "evt-sentinel-003";
const SEEDED_INBOUND_UUID: u128 = 0x5eed_0000_0000_0000_0000_0000_0000_0002;

#[tokio::test]
async fn inbound_persisted_before_adapter_is_ready_is_replayed_not_lost() {
    if tokio::time::timeout(DEADLINE, case()).await.is_err() {
        panic!("parity_02：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn case() {
    let dir = TempDir::new("02-pre-ready");
    let store = open_store(&dir);
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    seed_project(&store, project_id, "E:\\hbot-parity-02").await;

    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let gate = SubscribeGate::closed();
    let adapter = FakeAdapter::with_gate(
        vec![ConversationRef::new(CONVERSATION, ConversationKind::Group)],
        gate.clone(),
    );
    let executor = RecordingExecutor::new();
    let supervisor = build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        executor.clone(),
        bot_spec(bot_id, project_id, "parity"),
        |_deps| {},
    );

    let starting = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.start().await })
    };

    // 等 supervisor 真的走到 subscribe（此刻 bot 行与会话行已经落库）。
    gate.wait_entered().await;

    // ① 窗口里插一条未处理入站：会话行必须已经存在，否则这里会撞外键。
    let conversation_id = conversation_id_for(&bot_id, CONVERSATION);
    let seeded = blocking({
        let store = Arc::clone(&store);
        move || {
            store.insert_inbound_if_new(NewInbound {
                id: InboundId::from_uuid(Uuid::from_u128(SEEDED_INBOUND_UUID)),
                bot_id,
                external_id: REPLAYED.to_owned(),
                conversation_id,
                payload_json: serde_json::json!({ "text": "m-replay-001" }).to_string(),
                received_at: 1_700_000_000_000,
            })
        }
    })
    .await
    .expect("窗口内插入未处理入站");
    assert!(
        seeded.is_some(),
        "subscribe 之前（bot 行与会话行已写）插的入站没有落库：启动顺序或外键不对"
    );

    // ② 放行 subscribe：start 会继续 recover outbox → replay → reading。
    gate.release();
    starting
        .await
        .expect("start 任务 panic")
        .expect("supervisor 启动");

    // ③ 订阅前落库的那条被启动补偿补上（一启动就该跑完，不依赖任何现场事件）。
    wait_until(
        "启动补偿把 subscribe 之前落库的那条跑掉",
        || executor.seen().len() == 1,
    )
    .await;
    assert_eq!(
        executor.external_ids(),
        vec![REPLAYED.to_owned()],
        "启动补偿没有先执行 subscribe 之前就落库的那条入站"
    );

    // ④ 订阅之后：重复投递同一条（不该再跑）+ 两条新消息（要跑）。
    //    哨兵保证重复投递在处理新消息之前已经走完（同一条事件通道是 FIFO）。
    assert!(
        adapter.push(group_event(CONVERSATION, REPLAYED, "m-replay-001")),
        "订阅之后事件流应当可投递"
    );
    assert!(
        adapter.push(group_event(
            CONVERSATION,
            ARRIVED_AFTER_SUBSCRIBE,
            "m-live-002"
        )),
        "订阅之后事件流应当可投递"
    );
    assert!(
        adapter.push(group_event(CONVERSATION, SENTINEL, "m-sentinel-003")),
        "订阅之后事件流应当可投递"
    );

    wait_until("订阅之后投递的 2 条被执行", || {
        executor.seen().len() == 3
    })
    .await;

    let seen = executor.external_ids();
    assert_eq!(
        seen,
        vec![
            REPLAYED.to_owned(),
            ARRIVED_AFTER_SUBSCRIBE.to_owned(),
            SENTINEL.to_owned(),
        ],
        "subscribe 之前那条被重复执行了，或订阅之后的投递丢了/乱序了"
    );
    assert_eq!(
        seen.iter().filter(|id| *id == REPLAYED).count(),
        1,
        "同一条入站执行了不止一次（唯一键去重失效）"
    );

    // ⑤ 启动补偿确实走了 replay 分支（日志里留了痕），不是被现场事件碰巧掩盖。
    let logs = blocking({
        let store = Arc::clone(&store);
        move || {
            store
                .list_logs(Some(&bot_id), 20)
                .map_err(|error| error.to_string())
        }
    })
    .await
    .expect("读 logs");
    assert!(
        logs.iter()
            .any(|row| row.line.contains("重启补偿") && row.level == "warn"),
        "没有看到启动补偿的 warn 日志行，replay 分支可能没走：{logs:?}"
    );

    assert_eq!(adapter.subscribe_calls(), 1, "适配器被订阅了不止一次");

    supervisor.stop().await.expect("supervisor 停止");
}
