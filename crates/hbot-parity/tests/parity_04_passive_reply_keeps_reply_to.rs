//! parity #04 — 被动回复的 `reply_to` 端到端活着：入站平台消息 ID → handler 的 `reply()`
//! → outbox 行 → **换一个进程重新打开同一个库** → 恢复出的 `PendingOutbound` 仍带同一个
//! `reply_to` → 假适配器收到的发送请求里 `reply_to` 一致。
//!
//! `docs/04-implementation-plan.md` §4 第 4 行「现状」= **缺口**（`reply_to` 列早就加了，
//! 但没有任何测试证明它端到端活着）。`hbot-rpc/tests/bot_runtime.rs` 的
//! `passive_reply_context_survives_restart` 只重启了 supervisor、复用同一个 `Store`；
//! 本用例把 `Store` 也丢掉、用同一个库文件重新 `Store::open`——这才叫「重启」。
//!
//! 被验证的层：真 `SandboxHandler`（真 QuickJS，`reply_to` 由
//! `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs` 的 `passive_reply_id` 生成）、
//! 真 supervisor、真队列、真 outbox、真 SQLite。假货只有平台适配器。

mod support;

use std::sync::Arc;
use std::time::Duration;

use hbot_bot::{ConversationRef, PendingOutboxSource};
use hbot_code::Limits;
use hbot_core::{BotId, ConversationKind, HbotError, ProjectId};
use hbot_rpc::EventBus;
use hbot_rpc::app::runtime::outbox_source::StorePendingOutbox;
use support::{
    DEADLINE, FakeAdapter, ReplyRelay, TempDir, blocking, bot_spec, group_event, logs_of,
    open_store, reopen_store, sandbox_executor, seed_project, wait_until, wait_until_async,
    write_bot_script,
};
use uuid::Uuid;

const BOT_UUID: u128 = 0xB04;
const PROJECT_UUID: u128 = 0x5151_0004;
const CONVERSATION: &str = "90004";
/// 平台侧的入站消息 ID：被动回复必须把它原样带回 `reply_to`。
const PLATFORM_MESSAGE_ID: &str = "msg-9002";

const HANDLER: &str = r#"
import { defineBot } from 'hbot';
export default defineBot({
  onMessage(ctx) {
    ctx.log('收到：' + ctx.text);
    ctx.reply('pong');
  },
});
"#;

#[tokio::test]
async fn passive_reply_keeps_reply_to_across_a_real_restart() {
    if tokio::time::timeout(DEADLINE, case()).await.is_err() {
        panic!("parity_04：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn case() {
    let dir = TempDir::new("04-reply-to");
    let db_path = dir.join("hbot.sqlite3");
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let project_root = dir.join("project");
    write_bot_script(&project_root, HANDLER);

    // ---------- 第一轮：真 handler 产生回复，发送故意失败，行留在 pending ----------
    let reply_to_before_restart = {
        let store = open_store(&dir);
        seed_project(&store, project_id, "E:\\hbot-parity-04").await;

        let adapter = FakeAdapter::new(vec![ConversationRef::new(
            CONVERSATION,
            ConversationKind::Group,
        )]);
        // 平台暂时收不到：行停在 pending，正好给「下次启动续发」留出窗口。
        adapter.fail_sends(u32::MAX);

        let events = Arc::new(EventBus::new());
        let (handler, replies_rx) = sandbox_executor(
            Arc::clone(&store),
            events,
            bot_id,
            project_root.clone(),
            Limits::new(),
        );

        let supervisor = support::build_supervisor(
            Arc::clone(&store),
            Arc::clone(&adapter),
            handler,
            bot_spec(bot_id, project_id, "parity"),
            |deps| {
                deps.pending_outbox = Arc::new(StorePendingOutbox::new(Arc::clone(&deps.store)));
                deps.outbox_config.max_attempts = 1_000;
                deps.outbox_config.base_backoff = Duration::from_secs(30);
                deps.outbox_config.max_backoff = Duration::from_secs(30);
                deps.outbox_config.send_timeout = Duration::from_secs(2);
                deps.shutdown_timeout = Duration::from_secs(2);
            },
        );
        let relay = ReplyRelay::spawn(replies_rx, Arc::clone(&supervisor));
        supervisor.start().await.expect("supervisor 启动");

        // 真实的被动回复：平台消息 ID 由 `passive_reply_id` 绑成 reply_to。
        supervisor
            .inject(group_event(CONVERSATION, PLATFORM_MESSAGE_ID, "ping"))
            .await
            .expect("入队");

        // ① handler 真的跑完了（否则后面读到的是「还没跑」而不是「跑错了」）。
        wait_until_async("入站被标记为已处理", || {
            let store = Arc::clone(&store);
            async move {
                blocking(move || {
                    store
                        .list_unprocessed_inbound(10)
                        .map(|rows| rows.is_empty())
                })
                .await
                .expect("读未处理入站")
            }
        })
        .await;

        // ② 回复经过 outbox 发出了第一次尝试（失败但记了 attempt）。
        wait_until("回复进入 outbox 并尝试发送", || {
            adapter.send_calls() > 0
        })
        .await;

        // ③ handler 产出的 OutboundRequest 带 reply_to
        let requests = relay.requests();
        assert_eq!(requests.len(), 1, "应当只有一条回复：{requests:?}");
        assert_eq!(
            requests[0].reply_to.as_deref(),
            Some(PLATFORM_MESSAGE_ID),
            "handler 的 reply() 没有把平台消息 ID 绑成 reply_to"
        );

        // ④ outbox 行里 reply_to 非空且等于平台消息 ID
        let pending = blocking({
            let store = Arc::clone(&store);
            move || store.list_pending_outbound_for_bot(&bot_id, 10)
        })
        .await
        .expect("读 pending outbound");
        assert_eq!(pending.len(), 1, "应当正好一行待发：{pending:?}");
        let row = &pending[0];
        assert_eq!(row.text, "pong", "回复正文不对");
        assert_eq!(
            row.reply_to.as_deref(),
            Some(PLATFORM_MESSAGE_ID),
            "outbox 行把 reply_to 写丢了（`reply_to` 列有值才是 D3 的契约）"
        );
        assert_eq!(row.attempts, 1, "第一次尝试后 attempts 应当是 1");

        let logs = logs_of(&store, bot_id).await;
        assert!(
            logs.iter().any(|line| line.line.contains("收到：ping")),
            "handler 的 ctx.log 没有进 logs 表：{logs:?}"
        );

        let reply_to = row.reply_to.clone();

        // 收尾：停 supervisor、断掉所有持库句柄，准备「重启」。
        //
        // 这里**不能**要求 stop() 一定 Ok：本轮的假适配器永远拒收，outbox 会一直重试，
        // 而 `Outbox::stop`（crates/hbot-bot/src/outbox.rs:291-305）只在 worker 退出时
        // 才算 flush 成功；退不掉就 `HbotError::Timeout { operation: "flush outbox" }`
        // —— 这正是「平台不可达时不许假装发成功、行保持 pending 等下次启动」的正确行为。
        // 所以两种结果都接受，后面用 pending 行是否还在来判定真正的不变量。
        match supervisor.stop().await {
            Ok(()) => {}
            Err(HbotError::Timeout { ref operation, .. }) if operation == "flush outbox" => {}
            Err(other) => panic!("第一轮 supervisor 停止出现意外错误：{other}"),
        }
        drop(relay);
        drop(supervisor);
        drop(store);
        reply_to
    };

    // ---------- 第二轮：新 Store 打开同一个库文件（新进程语义） ----------
    let store2 = reopen_store(&db_path);
    let adapter2 = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION,
        ConversationKind::Group,
    )]);
    let events2 = Arc::new(EventBus::new());
    let (handler2, replies_rx2) = sandbox_executor(
        Arc::clone(&store2),
        events2,
        bot_id,
        project_root.clone(),
        Limits::new(),
    );
    let supervisor2 = support::build_supervisor(
        Arc::clone(&store2),
        Arc::clone(&adapter2),
        handler2,
        bot_spec(bot_id, project_id, "parity"),
        |deps| {
            deps.pending_outbox = Arc::new(StorePendingOutbox::new(Arc::clone(&deps.store)));
            deps.outbox_config.max_attempts = 5;
            deps.outbox_config.base_backoff = Duration::from_millis(10);
            deps.outbox_config.max_backoff = Duration::from_millis(50);
            deps.outbox_config.send_timeout = Duration::from_secs(2);
        },
    );
    let relay2 = ReplyRelay::spawn(replies_rx2, Arc::clone(&supervisor2));

    // ⑤ 恢复源（重启时 supervisor 用的就是它）交出的 PendingOutbound 仍带同一个 reply_to。
    //
    // 直接问 `StorePendingOutbox`，不靠「发出去之后再看」——这一步只要 reply_to 丢在
    // 恢复路径上就立刻红，不必等 outbox 真的把消息发出去。
    let recovered = {
        let source = StorePendingOutbox::new(Arc::clone(&store2));
        blocking(move || source.pending(&bot_id))
            .await
            .expect("重建 PendingOutbound")
    };
    assert_eq!(
        recovered.len(),
        1,
        "重启后应当只有一条待续发：{recovered:?}"
    );
    assert_eq!(
        recovered[0].reply_to.as_deref(),
        Some(PLATFORM_MESSAGE_ID),
        "重启后续发的 PendingOutbound 丢了 reply_to（恢复路径没把 reply_to 带过来）"
    );
    assert_eq!(
        recovered[0].reply_to, reply_to_before_restart,
        "重启前后的 reply_to 不是同一个值"
    );

    // 恢复源交出来的东西真的会被发出去（同一批行，经过 supervisor 的 recover_outbox）。
    supervisor2.start().await.expect("第二轮 supervisor 启动");

    // ⑥ 假适配器（= 平台收到的东西）看到的 reply_to 也一致
    wait_until("适配器收到续发的回复", || {
        !adapter2.sent().is_empty()
    })
    .await;
    assert_eq!(
        adapter2.sent_reply_to(),
        vec![Some(PLATFORM_MESSAGE_ID.to_owned())],
        "平台侧收到的发送请求里 reply_to 与触发消息不一致（回复被发成了主动消息）"
    );
    assert_eq!(
        adapter2.sent_texts(),
        vec!["pong".to_owned()],
        "续发的正文不对"
    );
    // 第二轮没有新入站被处理，所以 handler 的回复通道应当是空的 —— 上面那条回复
    // 只能是「重启前落库的那一行」被恢复出来的。
    assert!(
        relay2.requests().is_empty(),
        "第二轮不该有新产生的回复：{:?}",
        relay2.requests()
    );

    // ⑦ 落库状态：行已 sent，没有残留 pending
    let rows = blocking({
        let store = Arc::clone(&store2);
        move || store.list_pending_outbound_for_bot(&bot_id, 10)
    })
    .await
    .expect("读 pending outbound");
    assert!(rows.is_empty(), "成功续发之后不该还有 pending 行：{rows:?}");

    // 顺带确认启动补偿确实恢复了这一条（而不是别的东西把它发出去）。
    let restored = supervisor2.snapshot().restored_outbound;
    assert!(
        restored >= 1,
        "snapshot 没有记录恢复的 outbound（restored_outbound = {restored}）"
    );

    supervisor2.stop().await.expect("第二轮 supervisor 停止");
}
