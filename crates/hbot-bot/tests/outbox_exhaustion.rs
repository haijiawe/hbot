//! outbox 重试预算耗尽之后会发生什么：**死信（`state='failed'`）**，不是永久 pending。
//!
//! ## 这条用例挡的是什么
//!
//! 修复前（本文件的上一版钉的就是这个事实）：预算烧完后行**永远留在 `pending`**，
//! `Store::mark_outbound_failed`（`models.rs` 里 `OUTBOUND_STATE_FAILED` 的注释写着「超过
//! 重试上限，转为死信（保留行，不再被 pending 扫描捡起）」）**一次都没被调用过**。后果：
//!
//! * `list_pending_outbound_for_bot` 每次都还把这条永久发不出去的消息扫出来，`Outbox::recover`
//!   把它装回去，`deliver` 先 `mark_outbound_attempt` 再判上限 → **每次启动 `attempts` 必然
//!   +1，永不停止**，而且因为 `attempts > max_attempts` 早已成立，连一次真正的发送都不会再发生
//!   （跨重启空转，`attempts` 2→3→4 单调增长到任意大）。
//! * 一条死消息永久占住 `pending` 队头，UI 上「待发」计数永远不归零。
//!
//! ## 修复后的契约（本文件断言的就是它）
//!
//! 两级预算，都在 [`OutboxConfig`] 里可配：
//!
//! | 字段 | 默认 | 语义 |
//! |---|---|---|
//! | `max_attempts` | 3 | **本次运行内**的尝试次数；用完保留 `pending`，留给下次启动 |
//! | `max_send_attempts` | 10 | **跨重启累计**的硬上限；达到就转死信 + `error` RunLog |
//!
//! 为什么必须是两级：只有第一级 → 上面的无限空转；只有第二级 → 平台抖一下（几百毫秒
//! 内）判死，重启这种正常运维动作会把还能发的消息丢掉。第一级给「这次先放一放」，
//! 第二级给「试够了就别再试」。
//!
//! 计数留在库里（`outbound.attempts`）而不是内存里，所以重启**不会**把次数归零。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, wait_until};
use hbot_bot::outbox::OutboxConfig;
use hbot_bot::{ConversationRef, OutboundRequest, PendingOutbound, PendingOutboxSource};
use hbot_core::{BotId, ConversationKind, HbotError};
use hbot_store::{OutboundId, OutboundRow, Store};
use uuid::Uuid;

fn bot_id() -> BotId {
    BotId::from_uuid(Uuid::from_u128(0xB07))
}

/// 只会失败的发送配置：退避压到最短，让用例在毫秒级跑完。
fn config(max_attempts: u32, max_send_attempts: u32) -> OutboxConfig {
    OutboxConfig {
        max_attempts,
        max_send_attempts,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        capacity: 16,
        send_timeout: Duration::from_secs(2),
    }
}

/// 从 `outbound` 表捞 pending 的 [`PendingOutboxSource`]（与 `hbot-rpc` 的
/// `StorePendingOutbox` 同构：把 `conversation_id` 回查成平台侧会话）。
///
/// 必须自己实现而不是用默认的 `NoPendingOutbox`：默认实现返回空集，重启后的
/// `Outbox::recover` 就一条都装不回来，"跨重启"这件事根本不会被触发。
struct StorePendingOutbox {
    store: Arc<Store>,
}

impl PendingOutboxSource for StorePendingOutbox {
    fn pending(&self, bot_id: &BotId) -> Result<Vec<PendingOutbound>, HbotError> {
        let rows = self
            .store
            .list_pending_outbound_for_bot(bot_id, 64)
            .map_err(|error| HbotError::internal(error.to_string()))?;
        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let conversation = self
                .store
                .get_conversation(&row.conversation_id)
                .map_err(|error| HbotError::internal(error.to_string()))?
                .ok_or_else(|| HbotError::internal("conversation row missing"))?;
            pending.push(PendingOutbound {
                id: row.id,
                bot_id: row.bot_id,
                conversation: ConversationRef::new(
                    conversation.external_id,
                    ConversationKind::Group,
                ),
                text: row.text,
                attempts: row.attempts,
                reply_to: row.reply_to,
            });
        }
        Ok(pending)
    }
}

/// 读回一行 `outbound`；死信行**保留**（不删），所以这里必须能拿到。
fn outbound_row(fixture: &Fixture, id: &OutboundId) -> OutboundRow {
    fixture
        .store
        .get_outbound(id)
        .expect("read outbound row")
        .expect("死信行必须保留：排障需要原始 target/text")
}

/// 该 bot 还有几条 `state = 'pending'` 的行。
///
/// `OutboundRow` 上没有 `state` 字段（见 `hbot-store/src/models.rs`），所以「行还被当成待发」
/// 只能用 SQL 层的过滤来观测：`list_pending_outbound_for_bot` 的 WHERE 是 `state = 'pending'`。
fn pending_count(fixture: &Fixture) -> usize {
    fixture
        .store
        .list_pending_outbound_for_bot(&bot_id(), 64)
        .expect("list pending")
        .len()
}

/// 全部 `error` 级日志行。
fn error_lines(fixture: &Fixture) -> Vec<String> {
    fixture
        .store
        .list_logs(Some(&bot_id()), 64)
        .expect("read logs")
        .into_iter()
        .filter(|row| row.level == "error")
        .map(|row| row.line)
        .collect()
}

fn request(text: &str) -> OutboundRequest {
    OutboundRequest {
        conversation: ConversationRef::new("chat-dead", ConversationKind::Group),
        text: text.to_owned(),
        reply_to: None,
    }
}

/// 累计预算耗尽 → 行转死信（`attempts >= max_send_attempts` 当场生效），
/// 并且留下一条能读懂原因的 `error` RunLog。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_outbound_is_dead_lettered_with_a_readable_log() {
    let declared = vec![ConversationRef::new("chat-dead", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(u32::MAX);

    let mut deps = fixture.deps();
    deps.outbox_config = config(2, 2);
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    let outbound_id = supervisor
        .send(request("永远发不出去的回复"))
        .await
        .expect("persisted");
    wait_until("dead letter recorded", || pending_count(&fixture) == 0).await;

    // 本轮最多试 2 次（含首次），全部失败后累计预算（2）也到顶。
    assert_eq!(fixture.adapter.send_calls(), 2, "max_attempts=2");
    assert!(fixture.adapter.sent().is_empty(), "一条都没发出去");

    let row = outbound_row(&fixture, &outbound_id);
    assert_eq!(
        row.attempts, 2,
        "死信记下最终尝试次数，UI 才能说“重试 2 次后放弃”"
    );
    assert!(row.reply_to.is_none(), "主动消息不带 reply_to");

    // 核心断言：不再被当成待发。
    assert_eq!(pending_count(&fixture), 0, "死信行必须离开 pending 集合");
    assert_eq!(
        fixture.store.count_pending_outbound().expect("count"),
        0,
        "全库待发计数同样要归零（UI 上“待发”不能永远挂着一条死消息）"
    );

    // 计数器：`failed` 记 1（一条放弃），`sent` 为空。
    let stats = supervisor.snapshot().outbox.expect("outbox stats");
    assert_eq!(stats.failed, 1, "放弃要计数");
    assert_eq!(stats.sent, 0);

    // 日志：一条 error，说明放弃原因与尝试次数。
    let lines = error_lines(&fixture);
    let dead = lines
        .iter()
        .find(|line| line.contains("放弃重试"))
        .unwrap_or_else(|| panic!("死信必须写一条 error 日志给 logs_tail，实际：{lines:?}"));
    assert!(
        dead.contains("尝试 2 次") && dead.contains("上限 2"),
        "日志要写清试了几次、上限多少：{dead}"
    );

    // 停手之后不再有任何新的发送尝试。
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(fixture.adapter.send_calls(), 2, "死信之后不许继续空转");

    supervisor.stop().await.expect("stop");
}

/// 队头一条死消息**不许**堵住后面的消息（串行 worker 最容易在这里出事）。
///
/// 只让前两次发送失败（正好把第一条打死），第二条必须照常发出去。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_letter_does_not_block_the_messages_behind_it() {
    let declared = vec![ConversationRef::new("chat-dead", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(2);

    let mut deps = fixture.deps();
    deps.outbox_config = config(2, 2);
    let supervisor = fixture.supervisor(bot_id(), deps);
    supervisor.start().await.expect("start");

    let dead_id = supervisor
        .send(request("第一条（注定失败）"))
        .await
        .expect("persisted");
    wait_until("first message dead-lettered", || {
        pending_count(&fixture) == 0
    })
    .await;

    // 第二条紧跟着入队：适配器已经不再失败，它必须被发出去。
    let live_id = supervisor
        .send(request("第二条（应当发出）"))
        .await
        .expect("persisted");
    wait_until("second message sent", || {
        supervisor
            .snapshot()
            .outbox
            .is_some_and(|stats| stats.sent == 1)
    })
    .await;

    let sent: Vec<String> = fixture
        .adapter
        .sent()
        .into_iter()
        .map(|envelope| envelope.text)
        .collect();
    assert_eq!(
        sent,
        vec!["第二条（应当发出）".to_owned()],
        "后续消息必须照发"
    );

    assert_eq!(pending_count(&fixture), 0, "两条都不在待发集合里");
    assert!(fixture.store.get_outbound(&live_id).expect("row").is_some());
    let dead = outbound_row(&fixture, &dead_id);
    assert_eq!(dead.attempts, 2);
    assert_eq!(fixture.adapter.send_calls(), 3, "死信 2 次 + 成功 1 次");

    supervisor.stop().await.expect("stop");
}

/// 跨重启：死信行**不会**被 `recover` 装回来，所以第二次启动连一次空转都没有。
///
/// 这正是修复前那条 `attempts` 2→3→4 增长曲线的反面。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_does_not_spin_on_a_dead_lettered_row() {
    let declared = vec![ConversationRef::new("chat-dead", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(u32::MAX);

    let deps = || {
        let mut deps = fixture.deps();
        deps.outbox_config = config(2, 2);
        deps.pending_outbox = Arc::new(StorePendingOutbox {
            store: Arc::clone(&fixture.store),
        });
        deps
    };

    // ---- 第一轮：烧掉累计预算，转死信 ----
    let first = fixture.supervisor(bot_id(), deps());
    first.start().await.expect("first start");
    let outbound_id = first
        .send(request("永远发不出去的回复"))
        .await
        .expect("persisted");
    wait_until("first run dead-lettered", || pending_count(&fixture) == 0).await;
    assert_eq!(fixture.adapter.send_calls(), 2);
    assert_eq!(outbound_row(&fixture, &outbound_id).attempts, 2);
    first.stop().await.expect("stop");

    // ---- 第二轮：recover 只会捞 pending，死信不在其中 ----
    let second = fixture.supervisor(bot_id(), deps());
    second.start().await.expect("second start");
    // 给 recover 充足的时间：如果它把死信装回来了，这里就会看到新的尝试。
    tokio::time::sleep(Duration::from_millis(60)).await;

    assert_eq!(
        fixture.adapter.send_calls(),
        2,
        "死信不该在重启后被重新发送（修复前这里会空转）"
    );
    assert_eq!(
        outbound_row(&fixture, &outbound_id).attempts,
        2,
        "attempts 不许因为重启而继续涨"
    );
    assert_eq!(
        second.snapshot().outbox.expect("outbox").failed,
        0,
        "这一轮没有任何东西可放弃"
    );
    assert_eq!(pending_count(&fixture), 0, "死信仍不在待发集合里");
    assert!(fixture.adapter.sent().is_empty(), "从头到尾一条都没发出去");

    second.stop().await.expect("stop");
}

/// 反面：累计预算**没**到顶时，行仍然留在 `pending` 等下次启动 —— 别把两级预算修成一级。
///
/// 这里 `max_attempts=2`（每轮 2 次）而 `max_send_attempts=10`（累计 10 次），所以两轮
/// 之后 `attempts` 是 4：仍 pending、仍不转死信。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_within_the_total_budget_waits_for_the_next_start() {
    let declared = vec![ConversationRef::new("chat-dead", ConversationKind::Group)];
    let fixture = Fixture::new(declared);
    fixture.adapter.fail_next_sends(u32::MAX);

    let deps = || {
        let mut deps = fixture.deps();
        deps.outbox_config = config(2, 10);
        deps.pending_outbox = Arc::new(StorePendingOutbox {
            store: Arc::clone(&fixture.store),
        });
        deps
    };

    let first = fixture.supervisor(bot_id(), deps());
    first.start().await.expect("first start");
    let outbound_id = first
        .send(request("还在预算内的回复"))
        .await
        .expect("persisted");
    wait_until("first run used its budget", || {
        first
            .snapshot()
            .outbox
            .is_some_and(|stats| stats.failed == 1)
    })
    .await;
    assert_eq!(outbound_row(&fixture, &outbound_id).attempts, 2);
    assert_eq!(pending_count(&fixture), 1, "还没到累计上限，留给下次启动");
    first.stop().await.expect("stop");

    let second = fixture.supervisor(bot_id(), deps());
    second.start().await.expect("second start");
    wait_until("second run used its budget", || {
        second
            .snapshot()
            .outbox
            .is_some_and(|stats| stats.failed == 1)
    })
    .await;
    assert_eq!(
        outbound_row(&fixture, &outbound_id).attempts,
        4,
        "重启续用库里的 attempts（不归零），但仍在累计上限内"
    );
    assert_eq!(pending_count(&fixture), 1, "4 < 10，仍是待发而不是死信");
    assert_eq!(fixture.adapter.send_calls(), 4);
    second.stop().await.expect("stop");
}
