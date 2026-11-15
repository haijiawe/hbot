//! parity #05 — 一个 bot 崩了不影响另一个。
//!
//! `docs/04-implementation-plan.md` §4 第 5 行「现状」= 部分：已有
//! `dropping_a_wedged_isolate_does_not_hang`（单个 isolate 的 Drop 不挂）。**缺**的是
//! 「A bot 的 handler 卡死/超时之后，B bot 照常收消息、照常回复」这条跨 bot 隔离。
//!
//! 被验证的层：两个**真的** `Supervisor` + 两个**真的** `SandboxHandler`（真 QuickJS
//! 编译、真 isolate、真超时中断），共用一个真 SQLite（同一进程多 bot 是生产形状）。
//! 假货只有平台适配器。
//!
//! 判定顺序刻意写成「B 先回复 → 此刻 A 还没超时 → A 随后超时并被记录」：
//! 前半段证明 A 卡着的时候 B 没被拖住，后半段证明 A 的失败确实落到了日志里。

mod support;

use std::sync::Arc;
use std::time::Duration;

use hbot_bot::ConversationRef;
use hbot_code::Limits;
use hbot_core::{BotId, ConversationKind, ProjectId};
use hbot_rpc::EventBus;
use support::{
    DEADLINE, FakeAdapter, LogCapture, ReplyRelay, TempDir, bot_spec, group_event, logs_of,
    open_store, sandbox_executor, seed_project, wait_until, write_bot_script,
};
use uuid::Uuid;

const BOT_A: u128 = 0xB05_A000;
const BOT_B: u128 = 0xB05_B000;
const PROJECT_A: u128 = 0x5151_0005;
const PROJECT_B: u128 = 0x5151_0006;
const CONVERSATION_A: &str = "90005";
const CONVERSATION_B: &str = "90006";

/// A 的 handler：死循环，靠 handler 超时被内核中断。
const HANDLER_WEDGED: &str = r#"
import { defineBot } from 'hbot';
export default defineBot({
  onMessage() {
    for (;;) {}
  },
});
"#;

/// B 的 handler：正常回一句。
const HANDLER_HEALTHY: &str = r#"
import { defineBot } from 'hbot';
export default defineBot({
  onMessage(ctx) {
    ctx.log('B 收到：' + ctx.text);
    ctx.reply('pong-b');
  },
});
"#;

/// A 的 handler 超时预算。取得比 B 的整条链路（编译 + 派发 + 发送）长一个量级，
/// 这样「B 先回复」不是靠运气赢的；同时远低于 30 秒的用例上限。
const WEDGED_TIMEOUT: Duration = Duration::from_secs(4);

#[tokio::test]
async fn a_wedged_bot_does_not_stop_another_bot() {
    if tokio::time::timeout(DEADLINE, case()).await.is_err() {
        panic!("parity_05：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn case() {
    let dir = TempDir::new("05-crash-isolation");
    let store = open_store(&dir);

    let project_a = ProjectId::from_uuid(Uuid::from_u128(PROJECT_A));
    let project_b = ProjectId::from_uuid(Uuid::from_u128(PROJECT_B));
    seed_project(&store, project_a, "E:\\hbot-parity-05-a").await;
    seed_project(&store, project_b, "E:\\hbot-parity-05-b").await;

    let bot_a = BotId::from_uuid(Uuid::from_u128(BOT_A));
    let bot_b = BotId::from_uuid(Uuid::from_u128(BOT_B));
    let root_a = dir.join("project-a");
    let root_b = dir.join("project-b");
    write_bot_script(&root_a, HANDLER_WEDGED);
    write_bot_script(&root_b, HANDLER_HEALTHY);

    let adapter_a = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION_A,
        ConversationKind::Group,
    )]);
    let adapter_b = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION_B,
        ConversationKind::Group,
    )]);

    let events_a = Arc::new(EventBus::new());
    // RunLog 事件也收下来：A 的失败必须在事件流里也看得见（不能只落库）。
    let capture = LogCapture::spawn(events_a.subscribe());
    let (handler_a, replies_a) = sandbox_executor(
        Arc::clone(&store),
        events_a,
        bot_a,
        root_a,
        Limits::new().with_handler_timeout(WEDGED_TIMEOUT),
    );
    let events_b = Arc::new(EventBus::new());
    let (handler_b, replies_b) =
        sandbox_executor(Arc::clone(&store), events_b, bot_b, root_b, Limits::new());

    let supervisor_a = support::build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter_a),
        handler_a,
        bot_spec(bot_a, project_a, "parity"),
        |_deps| {},
    );
    let supervisor_b = support::build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter_b),
        handler_b,
        bot_spec(bot_b, project_b, "parity"),
        |_deps| {},
    );
    let relay_a = ReplyRelay::spawn(replies_a, Arc::clone(&supervisor_a));
    let relay_b = ReplyRelay::spawn(replies_b, Arc::clone(&supervisor_b));

    supervisor_a.start().await.expect("A 启动");
    supervisor_b.start().await.expect("B 启动");

    // ① A 先卡住（死循环，4 秒后才被中断）。
    supervisor_a
        .inject(group_event(CONVERSATION_A, "evt-a-001", "让我卡住"))
        .await
        .expect("投给 A");

    // ② B 在 A 还卡着的时候把活干完。
    supervisor_b
        .inject(group_event(CONVERSATION_B, "evt-b-001", "hello-b"))
        .await
        .expect("投给 B");
    wait_until("B 回复了第一条", || adapter_b.sent_texts().len() == 1).await;
    assert_eq!(
        adapter_b.sent_texts(),
        vec!["pong-b".to_owned()],
        "B 没有正常回复"
    );
    assert_eq!(
        adapter_b.sent_reply_to(),
        vec![Some("evt-b-001".to_owned())],
        "B 的回复没有绑定触发消息"
    );

    // ③ 此刻 A 还没超时：证明 B 的所有进展都发生在 A 卡死期间。
    let logs_a_at_b_reply = logs_of(&store, bot_a).await;
    assert!(
        logs_a_at_b_reply.is_empty(),
        "B 回复时 A 已经有日志了，说明 A 早就结束了，这个用例没测到隔离：{logs_a_at_b_reply:?}"
    );
    assert_eq!(
        supervisor_a.status(),
        hbot_core::BotStatus::Running,
        "A 在 handler 超时之前就不该被判为失败"
    );

    // ④ A 的超时最终被记录下来（真 QuickJS 超时中断 → HbotError → logs 表 + RunLog）。
    wait_until("A 的 handler 超时被记录", || {
        !capture.lines().is_empty()
    })
    .await;
    let timeout_entries = capture.entries();
    // 真 QuickJS 超时是这样冒出来的：`CodeError::Timeout` 在
    // crates/hbot-rpc/src/app/runtime/sandbox_handler.rs 里被翻成
    // `HbotError::timeout("sandbox.dispatch", budget_ms)`，Display 结果是
    // `timeout after 4000ms: sandbox.dispatch`。所以这里按「超时 + 出错在 dispatch」
    // 断言，而不是按 CodeError 那句 `HBOT_TIMEOUT: …`（那句在这一层已经被换掉了）。
    assert!(
        timeout_entries.iter().any(|(level, line)| {
            *level == hbot_rpc::LogLevel::Error
                && line.contains("timeout after")
                && line.contains("sandbox.dispatch")
        }),
        "A 的超时没有以 error 级 RunLog 事件抛出：{timeout_entries:?}"
    );

    let logs_a = logs_of(&store, bot_a).await;
    assert_eq!(logs_a.len(), 1, "A 应当正好留下一条失败日志：{logs_a:?}");
    assert_eq!(logs_a[0].level, "error", "A 的失败日志级别不对");
    assert!(
        logs_a[0].line.contains("处理失败")
            && logs_a[0].line.contains("timeout after")
            && logs_a[0].line.contains("sandbox.dispatch"),
        "A 的失败日志没有说清是超时：{}",
        logs_a[0].line
    );

    // ⑤ 一次失败不该让 A 直接降级（阈值 3），也不该动 B。
    assert_eq!(
        supervisor_a.status(),
        hbot_core::BotStatus::Running,
        "单次 handler 失败不该立刻把 A 判死"
    );
    assert_eq!(
        supervisor_b.status(),
        hbot_core::BotStatus::Running,
        "A 失败把 B 也带崩了"
    );

    // ⑥ A 失败之后，B 还能继续处理新的消息。
    supervisor_b
        .inject(group_event(CONVERSATION_B, "evt-b-002", "hello-b-again"))
        .await
        .expect("A 失败后再投给 B");
    wait_until("B 回复了第二条", || adapter_b.sent_texts().len() == 2).await;
    assert_eq!(
        adapter_b.sent_texts(),
        vec!["pong-b".to_owned(), "pong-b".to_owned()],
        "A 失败之后 B 不再处理消息"
    );
    assert_eq!(
        adapter_b.sent_reply_to(),
        vec![Some("evt-b-001".to_owned()), Some("evt-b-002".to_owned())],
        "B 第二条回复的 reply_to 不对"
    );

    // ⑦ A 侧：崩溃确实没产生回复（不会把半张卡死算成一次成功）。
    assert!(
        adapter_a.sent().is_empty(),
        "A 的 handler 超时后不该有任何回复：{:?}",
        adapter_a.sent_texts()
    );
    assert!(
        relay_a.errors().is_empty(),
        "A 的回复通道不该报错：{:?}",
        relay_a.errors()
    );
    assert!(
        relay_b.errors().is_empty(),
        "B 的回复通道不该报错：{:?}",
        relay_b.errors()
    );
    assert_eq!(
        adapter_b.send_calls(),
        2,
        "B 的发送次数不对（有没有重复发送？）"
    );

    supervisor_b.stop().await.expect("B 停止");
    supervisor_a.stop().await.expect("A 停止");
}
