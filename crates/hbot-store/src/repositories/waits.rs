//! `bot_waits` —— 会话等待（C2.session，docs/07-bot-runtime-blueprint.md 的 5.1 节）。
//!
//! 一层薄 SQL：注册、唤醒、超时结算、读当前、取消。**不认识业务**：不读时钟
//! （`now` 一律调用方传），不判参数（那是 `hbot_core::bot_session` 的闸）。
//!
//! ## 两条必须是一条 SQL 的地方（硬规则）
//!
//! [`claim`]（一条消息唤醒唯一那个等待）与 [`expire_due`]（超时结算）都用
//! `UPDATE … RETURNING` 一次完成「挑出行 + 改状态」。拆成「先查后改」的后果各不相同：
//!
//! * `claim` 拆开：同一条消息被投递两次（平台重投、崩溃重放）会**唤醒两次**。第二次时
//!   等待已经是 `fulfilled`，于是第二次「命中」的其实是一条已经用掉的等待；
//!   真机上表现为「用户发了一句话，bot 回了两遍」。
//! * `expire_due` 拆开：两轮 tick 会各自读到同一批到期行，同一行被结算两次（第二次
//!   看到的是一个已经 `expired` 的行，日志里就是两条一样的超时记录）。
//!
//! 与 `repositories::jobs` 的 `claim` 同一条理由，那边的模块文档写得更细。
//!
//! ## 为什么「同会话只有一个未结等待」交给数据库
//!
//! 不是靠调用方先 `current()` 再 `register()`：并发下两次都会读到「没有」，然后都去插。
//! 判据是 `0007` 里的**部分唯一索引** `bot_waits_conv_pending`（只覆盖 `pending`），
//! 撞上时这里把它翻译成一个**可区分**的错误（[`crate::StoreError::WaitPending`]），
//! 让装配层能报 `HBOT_SESSION_PENDING` 而不是一条「数据库约束失败」。
//!
//! ## 为什么 scope 判据写在 SQL 里
//!
//! `scope = 'conversation' OR owner = ?sender` 与 `WaitScope::accepts` 同义。写进 SQL 才能
//! 与「改成 fulfilled」在同一条语句里完成；先读出来再回上层判，就回到了上面说的双唤醒窗口。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::ids::{BotId, ConversationId, WaitId};
use crate::models::{NewWait, WaitRow};

/// 等待状态字符串：与 `hbot_core::bot_session::WaitState::as_str()` 同值。
pub(crate) const STATE_PENDING: &str = "pending";
/// 被一条消息唤醒（成功终态）。
pub(crate) const STATE_FULFILLED: &str = "fulfilled";
/// 到点还没人回答（超时终态）。
pub(crate) const STATE_EXPIRED: &str = "expired";
/// `bot_stop` 取消（终态）。
pub(crate) const STATE_CANCELLED: &str = "cancelled";

/// 注册一条等待；返回新行 id。
///
/// 撞 `bot_waits_conv_pending` 时返回 [`StoreError::WaitPending`]（带挡路那条的键），
/// 让调用方能报 `HBOT_SESSION_PENDING` 而不是一条裸的约束错误。
pub(crate) fn register(conn: &Connection, bot_id: &BotId, wait: &NewWait) -> Result<WaitId> {
    let inserted = conn.execute(
        INSERT_SQL,
        params![
            wait.id.to_string(),
            bot_id.to_string(),
            wait.conversation_id.to_string(),
            wait.conversation_external_id,
            wait.owner,
            wait.key,
            wait.prompt,
            wait.scope,
            STATE_PENDING,
            wait.deadline,
            wait.created_at,
            wait.created_at,
            i64::from(wait.reminders_left),
            wait.timeout_ms,
        ],
    );
    match inserted {
        Ok(_) => Ok(wait.id),
        Err(source) => {
            // 只有部分唯一索引会在这里报约束失败；把挡路那条的键读出来，
            // 用户才知道先回答哪个。读不到就按原样上报（库坏了不该被伪装成业务冲突）。
            let pending = conn
                .query_row(
                    PENDING_KEY_SQL,
                    params![bot_id.to_string(), wait.conversation_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten();
            match pending {
                Some(key) => Err(StoreError::WaitPending { key }),
                None => Err(source.into()),
            }
        }
    }
}

/// 一条消息到达时问「它唤醒哪个等待」，命中就地改成 fulfilled 并返回改完的行。
///
/// 返回 `Ok(None)` 是正常路径（这条消息不回答任何等待），不是错误。
pub(crate) fn claim(
    conn: &Connection,
    bot_id: &BotId,
    conversation_id: &ConversationId,
    sender_id: &str,
    message_id: &str,
    now: i64,
) -> Result<Option<WaitRow>> {
    let transaction = conn.unchecked_transaction()?;
    let claimed = {
        let mut statement = transaction.prepare(CLAIM_SQL)?;
        let mut rows = statement.query(params![
            STATE_FULFILLED,
            message_id,
            now,
            bot_id.to_string(),
            conversation_id.to_string(),
            STATE_PENDING,
            sender_id,
        ])?;
        rows.next()?.map(row_to_wait).transpose()?
    };
    transaction.commit()?;
    Ok(claimed)
}

/// 超时结算：把 `deadline <= now` 的 pending 行改成 expired，返回改完的行。
///
/// `limit == 0` 直接返回空表：LIMIT 0 会静默返回空行，调用方分不清「没有到期」与
/// 「上界传错」（与 `jobs::claim_due` 同一口径）。
pub(crate) fn expire_due(
    conn: &Connection,
    bot_id: &BotId,
    now: i64,
    limit: u32,
) -> Result<Vec<WaitRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let transaction = conn.unchecked_transaction()?;
    let rows = {
        let mut statement = transaction.prepare(EXPIRE_SQL)?;
        let mapped = statement.query_map(
            params![
                STATE_EXPIRED,
                now,
                now,
                bot_id.to_string(),
                STATE_PENDING,
                i64::from(limit),
            ],
            row_to_wait,
        )?;
        let mut waits = Vec::new();
        for row in mapped {
            waits.push(row?);
        }
        waits
    };
    transaction.commit()?;
    Ok(rows)
}

/// 把到点但**还有重发额度**的等待推后一轮，返回推后之后的那些行。
///
/// 调用方（`tick_waits`）拿到返回的行就把 `prompt` 再发一遍。与 [`expire_due`] 分工：
/// 这条只处理 `reminders_left > 0`，那边只处理 `reminders_left = 0` —— 两个判据互斥，
/// 同一行不会在一条拍里既被重发又被判超时。
///
/// `deadline` 从 `now` 起算而不是从旧 `deadline` 起算：tick 落后时（进程被挂起、库忙）
/// 从旧值加一轮会得到仍然过期的时刻，于是下一拍立刻再重发一次 —— 那是「补发」而不是
/// 「重问」，用户会收到一串连着来的相同提问。从 `now` 起算最多损失一点精度，换的是
/// 「每次重发之间真的隔了一轮」。
///
/// `limit == 0` 直接返回空表（同 [`expire_due`] 的口径）。
pub(crate) fn promote_reminders(
    conn: &Connection,
    bot_id: &BotId,
    now: i64,
    limit: u32,
) -> Result<Vec<WaitRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let transaction = conn.unchecked_transaction()?;
    let rows = {
        let mut statement = transaction.prepare(PROMOTE_SQL)?;
        let mapped = statement.query_map(
            params![
                now,
                now,
                bot_id.to_string(),
                STATE_PENDING,
                i64::from(limit),
            ],
            row_to_wait,
        )?;
        let mut waits = Vec::new();
        for row in mapped {
            waits.push(row?);
        }
        waits
    };
    transaction.commit()?;
    Ok(rows)
}

/// 读这个会话当前未结的那条等待；没有返回 `None`。
pub(crate) fn current(
    conn: &Connection,
    bot_id: &BotId,
    conversation_id: &ConversationId,
) -> Result<Option<WaitRow>> {
    Ok(conn
        .query_row(
            &select_all("WHERE bot_id = ?1 AND conversation_id = ?2 AND state = ?3"),
            params![
                bot_id.to_string(),
                conversation_id.to_string(),
                STATE_PENDING,
            ],
            row_to_wait,
        )
        .optional()?)
}

/// 取消这个会话未结的那条等待；返回**本次是否真的取消了一行**。
///
/// 已经终态时返回 `false` 而不是报错：取消是幂等的（`bot_stop` 会调它，而那条等待
/// 可能刚好被一条消息唤醒）。
pub(crate) fn cancel(
    conn: &Connection,
    bot_id: &BotId,
    conversation_id: &ConversationId,
    now: i64,
) -> Result<bool> {
    let changed = conn.execute(
        CANCEL_SQL,
        params![
            STATE_CANCELLED,
            now,
            bot_id.to_string(),
            conversation_id.to_string(),
            STATE_PENDING,
        ],
    )?;
    Ok(changed == 1)
}

/// 取消一个 bot 的全部未结等待，返回取消了几条（`bot_stop` 用）。
pub(crate) fn cancel_for_bot(conn: &Connection, bot_id: &BotId, now: i64) -> Result<u32> {
    let changed = conn.execute(
        CANCEL_FOR_BOT_SQL,
        params![STATE_CANCELLED, now, bot_id.to_string(), STATE_PENDING],
    )?;
    Ok(u32::try_from(changed).unwrap_or(u32::MAX))
}

/// 数一个 bot 还有多少条未结等待（`bot_stop` 之外的容量闸判据）。
pub(crate) fn count_pending_for_bot(conn: &Connection, bot_id: &BotId) -> Result<u32> {
    let count: i64 = conn.query_row(
        COUNT_SQL,
        params![bot_id.to_string(), STATE_PENDING],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// 列一个 bot 最近的等待行（**新的在前**，最多 limit 条）—— 面板与排障用。
///
/// 面板要看「这个 bot 现在在等谁回答什么」，所以带状态一起回：终态行也留着
/// （用户要能看到「刚才那条已经超时了」），过滤交给调用方。
pub(crate) fn list_recent_for_bot(
    conn: &Connection,
    bot_id: &BotId,
    limit: u32,
) -> Result<Vec<WaitRow>> {
    let mut statement = conn.prepare(&select_all(LIST_RECENT_TAIL))?;
    let mapped = statement.query_map(params![bot_id.to_string(), i64::from(limit)], row_to_wait)?;
    let mut waits = Vec::new();
    for row in mapped {
        waits.push(row?);
    }
    Ok(waits)
}

/// list_recent_for_bot 的尾部（新的在前；同一毫秒用 rowid 作次级键）。
const LIST_RECENT_TAIL: &str = "WHERE bot_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2";

/// 数未结等待。
const COUNT_SQL: &str = "SELECT COUNT(*) FROM bot_waits WHERE bot_id = ?1 AND state = ?2";

fn select_all(where_clause: &str) -> String {
    format!("{SELECT_COLUMNS} {where_clause}")
}

fn row_to_wait(row: &rusqlite::Row<'_>) -> rusqlite::Result<WaitRow> {
    Ok(WaitRow {
        id: row.get::<_, String>(0)?.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        key: row.get(1)?,
        prompt: row.get(2)?,
        owner: row.get(3)?,
        scope: row.get(4)?,
        state: row.get(5)?,
        conversation: row.get(6)?,
        deadline: row.get(7)?,
        fulfilled_message_id: row.get(8)?,
        created_at: row.get(9)?,
        reminders_left: u8::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
        timeout_ms: row.get(11)?,
    })
}

const SELECT_COLUMNS: &str = "SELECT id, key, prompt, owner, scope, state, \
     conversation_external_id, deadline, fulfilled_message_id, created_at, reminders_left, \
     timeout_ms FROM bot_waits";

/// 注册一条等待。
///
/// 部分唯一索引 `bot_waits_conv_pending` 只覆盖 `state = 'pending'`：同一会话历史上
/// 的等待（超时的、被回答的）可以共存，只有「还开着的那条」唯一。
const INSERT_SQL: &str = "\
INSERT INTO bot_waits (id, bot_id, conversation_id, conversation_external_id, owner, key, \
     prompt, scope, state, deadline, created_at, updated_at, reminders_left, timeout_ms) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

/// 读「挡路的那条 pending」的键（只在唯一索引冲突时用）。
const PENDING_KEY_SQL: &str = "\
SELECT key FROM bot_waits \
WHERE bot_id = ?1 AND conversation_id = ?2 AND state = 'pending' LIMIT 1";

/// 一条消息唤醒唯一那个等待：挑 + 改 + 返回，一步完成。
///
/// 四个条件缺一不可：会话匹配、仍 `pending`、**还没过期**（`deadline > now`——
/// 一条已经该超时的等待不该被一句迟到的消息救活），以及 `scope` 判据。
const CLAIM_SQL: &str = "\
UPDATE bot_waits SET state = ?1, fulfilled_message_id = ?2, updated_at = ?3 \
WHERE id = (\
    SELECT id FROM bot_waits \
    WHERE bot_id = ?4 AND conversation_id = ?5 AND state = ?6 AND deadline > ?3 \
      AND (scope = 'conversation' OR owner = ?7) \
    ORDER BY created_at DESC LIMIT 1\
) \
RETURNING id, key, prompt, owner, scope, state, conversation_external_id, deadline, \
          fulfilled_message_id, created_at, reminders_left, timeout_ms";

/// 超时结算：把 db 已到期的 pending 一批改成 expired 并返回。
const EXPIRE_SQL: &str = "\
UPDATE bot_waits SET state = ?1, updated_at = ?2 \
WHERE id IN (\
    SELECT id FROM bot_waits \
    WHERE bot_id = ?4 AND state = ?5 AND deadline <= ?3 AND reminders_left = 0 \
    ORDER BY deadline LIMIT ?6\
) \
RETURNING id, key, prompt, owner, scope, state, conversation_external_id, deadline, \
          fulfilled_message_id, created_at, reminders_left, timeout_ms";

/// 重发：把到点但还有额度的 pending 行推后一轮（`deadline = now + timeout_ms`），
/// 额度减一，返回推后后的行。
const PROMOTE_SQL: &str = "\
UPDATE bot_waits SET deadline = ?1 + timeout_ms, reminders_left = reminders_left - 1, \
     updated_at = ?2 \
WHERE id IN (\
    SELECT id FROM bot_waits \
    WHERE bot_id = ?3 AND state = ?4 AND deadline <= ?2 AND reminders_left > 0 \
    ORDER BY deadline LIMIT ?5\
) \
RETURNING id, key, prompt, owner, scope, state, conversation_external_id, deadline, \
          fulfilled_message_id, created_at, reminders_left, timeout_ms";

/// 取消一条等待（只动 pending）。
const CANCEL_SQL: &str = "\
UPDATE bot_waits SET state = ?1, updated_at = ?2 \
WHERE bot_id = ?3 AND conversation_id = ?4 AND state = ?5";

/// `bot_stop`：取消这个 bot 的全部未结等待。
const CANCEL_FOR_BOT_SQL: &str = "\
UPDATE bot_waits SET state = ?1, updated_at = ?2 WHERE bot_id = ?3 AND state = ?4";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreError;
    use crate::test_support::{
        TempDir, bot_id, bot_id_at, conversation_id, new_wait, new_wait_with_reminders, open_store,
        raw, wait_id,
    };

    /// 状态与 scope 常量必须与 core 的枚举拼写逐字一致 —— 跨 crate 的隐性契约。
    ///
    /// 存储层在 SQL 里只能写字符串，core 在 serde 里写枚举；两边一旦分叉，表现是
    /// 「等待永远停在 pending」（写进去的 'Pending' 谁也认不出），而不是一条编译错误。
    #[test]
    fn state_and_scope_spellings_match_the_core_enums() {
        use hbot_core::bot_session::{WaitScope, WaitState};
        assert_eq!(STATE_PENDING, WaitState::Pending.as_str());
        assert_eq!(STATE_FULFILLED, WaitState::Fulfilled.as_str());
        assert_eq!(STATE_EXPIRED, WaitState::Expired.as_str());
        assert_eq!(STATE_CANCELLED, WaitState::Cancelled.as_str());
        assert_eq!(WaitScope::Sender.as_str(), "sender");
        assert_eq!(WaitScope::Conversation.as_str(), "conversation");
    }

    /// 注册 → 读回：一行进去、一行出来，`state` 是 `pending`。
    #[test]
    fn register_then_current_round_trips() {
        let dir = TempDir::new("waits-round-trip");
        let store = open_store(&dir);
        let spec = new_wait(7, "order-no", "u1", 1_000);
        let id = store
            .register_wait(&bot_id(), &spec)
            .expect("register wait");
        assert_eq!(id, wait_id(7), "注册必须返回新行主键");

        let current = store
            .current_wait(&bot_id(), &conversation_id())
            .expect("current wait")
            .expect("必须读回那一行");
        assert_eq!(current.key, "order-no");
        assert_eq!(current.owner, "u1");
        assert_eq!(current.state, STATE_PENDING);
        assert_eq!(current.scope, "sender");
        assert_eq!(current.deadline, 61_000);
        assert_eq!(current.fulfilled_message_id, None);
    }

    /// 同会话第二条 pending 被部分唯一索引拒，而且报的是**可区分**的业务错误。
    ///
    /// 这条是 §5.1 规则 4 的唯一落地判据：靠调用方先查后插在并发下会漏。
    #[test]
    fn a_second_pending_wait_in_the_same_conversation_is_refused() {
        let dir = TempDir::new("waits-single-pending");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("first wait");
        let error = store
            .register_wait(&bot_id(), &new_wait(8, "captcha", "u1", 1_000))
            .expect_err("第二条必须被拒");
        match error {
            StoreError::WaitPending { key } => {
                assert_eq!(
                    key, "order-no",
                    "错误必须点名挡路的那条，否则用户不知道该先回哪个"
                );
            }
            other => panic!("要的是 WaitPending，实际：{other}"),
        }
    }

    /// 同一个 `(bot, conversation)` 上，**终态**的历史行可以与新的 pending 共存。
    ///
    /// 部分唯一索引只覆盖 `pending`：超时过的、被回答过的等待必须能留档，
    /// 否则「这个用户之前被问过什么」就查不到了。
    #[test]
    fn terminal_rows_do_not_block_a_new_pending_wait() {
        let dir = TempDir::new("waits-history-allowed");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("first");
        assert!(
            store
                .cancel_wait(&bot_id(), &conversation_id(), 1_100)
                .expect("cancel")
        );
        store
            .register_wait(&bot_id(), &new_wait(8, "captcha", "u1", 1_200))
            .expect("终态行不该挡住新的 pending");
        assert_eq!(
            store
                .current_wait(&bot_id(), &conversation_id())
                .expect("current")
                .expect("row")
                .key,
            "captcha"
        );
    }

    /// 唤醒：命中就把行改成 `fulfilled` 并写下**是哪条消息**回答的。
    #[test]
    fn claim_marks_the_wait_fulfilled_with_the_message_id() {
        let dir = TempDir::new("waits-claim");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");

        let hit = store
            .claim_wait(&bot_id(), &conversation_id(), "u1", "msg-9", 2_000)
            .expect("claim")
            .expect("必须命中");
        assert_eq!(hit.id, wait_id(7));
        assert_eq!(hit.key, "order-no");
        assert_eq!(hit.fulfilled_message_id.as_deref(), Some("msg-9"));
        assert_eq!(hit.state, STATE_FULFILLED);

        // 第二次绝不能再命中：同一条消息被重投时不能唤醒两次。
        assert!(
            store
                .claim_wait(&bot_id(), &conversation_id(), "u1", "msg-9", 2_100)
                .expect("second claim")
                .is_none(),
            "一条等待只能被唤醒一次"
        );
    }

    /// 缺省 scope 只认开等待的那个人：群里 B 说一句话不能填掉 A 的等待（docs/00 P0-3）。
    #[test]
    fn a_different_sender_cannot_answer_a_sender_scoped_wait() {
        let dir = TempDir::new("waits-scope-sender");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        assert!(
            store
                .claim_wait(&bot_id(), &conversation_id(), "u2", "msg-9", 2_000)
                .expect("claim")
                .is_none(),
            "群里另一个人不能填掉 u1 的等待"
        );
        // 等待仍在，且仍归 u1。
        assert_eq!(
            store
                .current_wait(&bot_id(), &conversation_id())
                .expect("current")
                .expect("row")
                .owner,
            "u1"
        );
    }

    /// `scope = conversation` 时任何人都能回答（显式放宽之后的行为）。
    #[test]
    fn a_conversation_scoped_wait_accepts_anyone() {
        let dir = TempDir::new("waits-scope-conversation");
        let store = open_store(&dir);
        let mut spec = new_wait(7, "poll-result", "u1", 1_000);
        spec.scope = "conversation".to_owned();
        store.register_wait(&bot_id(), &spec).expect("register");
        let hit = store
            .claim_wait(&bot_id(), &conversation_id(), "u2", "msg-9", 2_000)
            .expect("claim")
            .expect("任何人都该能回答");
        assert_eq!(hit.owner, "u1", "归属仍是开等待的那个人");
    }

    /// 过了 deadline 的等待不能被一句迟到的消息救活。
    #[test]
    fn an_expired_by_deadline_wait_is_not_claimed() {
        let dir = TempDir::new("waits-deadline");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        // deadline = 61_000；now 正好等于它时就不该再被唤醒（边界是「严格大于」）。
        assert!(
            store
                .claim_wait(&bot_id(), &conversation_id(), "u1", "msg-9", 61_000)
                .expect("claim at deadline")
                .is_none()
        );
    }

    /// 超时结算：只动到期的行，第二次是空。
    #[test]
    fn expire_due_marks_only_the_waits_that_are_past_their_deadline() {
        let dir = TempDir::new("waits-expire");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");

        // 还没到点：一条都不该结算。
        assert!(
            store
                .expire_due_waits(&bot_id(), 60_999, 10)
                .expect("expire early")
                .is_empty()
        );
        let expired = store
            .expire_due_waits(&bot_id(), 61_000, 10)
            .expect("expire");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].state, STATE_EXPIRED);
        assert_eq!(expired[0].key, "order-no");
        assert!(
            store
                .expire_due_waits(&bot_id(), 61_000, 10)
                .expect("second expire")
                .is_empty(),
            "同一行不能被结算两次"
        );
        // 结算之后也不能再被唤醒。
        assert!(
            store
                .claim_wait(&bot_id(), &conversation_id(), "u1", "msg-9", 61_001)
                .expect("claim after expire")
                .is_none()
        );
    }

    /// 取消只动 pending；终态时返回 false 而不是报错（幂等）。
    #[test]
    fn cancel_only_touches_pending_and_is_idempotent() {
        let dir = TempDir::new("waits-cancel");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        assert!(
            store
                .cancel_wait(&bot_id(), &conversation_id(), 1_100)
                .expect("cancel")
        );
        assert!(
            !store
                .cancel_wait(&bot_id(), &conversation_id(), 1_200)
                .expect("second cancel"),
            "已经取消过的行不该再被改一次"
        );
        assert!(
            store
                .current_wait(&bot_id(), &conversation_id())
                .expect("current")
                .is_none(),
            "取消之后就没有未结等待了"
        );
    }

    /// `bot_stop`：整批取消，返回条数；终态行不动。
    #[test]
    fn cancel_for_bot_counts_the_pending_rows_only() {
        let dir = TempDir::new("waits-cancel-bot");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        assert_eq!(
            store
                .cancel_waits_for_bot(&bot_id(), 1_100)
                .expect("cancel"),
            1
        );
        assert_eq!(
            store
                .cancel_waits_for_bot(&bot_id(), 1_200)
                .expect("second cancel"),
            0,
            "没有 pending 行时返回 0，不是报错"
        );
    }

    /// 删 bot 级联删等待：不留「已删 bot 的等待还在等」这种孤儿行。
    #[test]
    fn deleting_the_bot_cascades_to_its_waits() {
        let dir = TempDir::new("waits-cascade");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        drop(store);
        // 直接删 bot 行（没有 delete_bot 这个 API，级联靠外键）。
        let conn = raw(&dir);
        conn.execute("DELETE FROM bots WHERE id = ?1", [bot_id().to_string()])
            .expect("delete bot");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM bot_waits", [], |row| row.get(0))
            .expect("count waits");
        assert_eq!(left, 0, "bot 行删掉后等待行必须级联消失");
    }

    /// 到点但还有额度的等待被**推后一轮**，而不是直接判超时。
    #[test]
    fn a_wait_with_reminders_is_pushed_back_instead_of_expiring() {
        let dir = TempDir::new("waits-promote");
        let store = open_store(&dir);
        store
            .register_wait(
                &bot_id(),
                &new_wait_with_reminders(7, "order-no", "u1", 1_000, 1),
            )
            .expect("register");

        // 到点那一刻：先重发（不落 expired），行还在 pending、额度减一。
        let promoted = store
            .promote_wait_reminders(&bot_id(), 61_000, 8)
            .expect("promote");
        assert_eq!(promoted.len(), 1, "到点必须被重发一次");
        assert_eq!(promoted[0].state, STATE_PENDING, "重发不是终态");
        assert_eq!(promoted[0].reminders_left, 0, "额度用掉一次");
        assert_eq!(
            promoted[0].deadline,
            61_000 + 60_000,
            "按 now + timeout 推后一轮（不是从旧 deadline 加）"
        );
        assert!(
            store
                .expire_due_waits(&bot_id(), 61_000, 8)
                .expect("expire")
                .is_empty(),
            "额度用尽之前的行不该被判超时"
        );
        let expired = store
            .expire_due_waits(&bot_id(), 121_000, 8)
            .expect("expire after the reminder");
        assert_eq!(expired.len(), 1, "第二轮到点才落 expired");
        assert_eq!(expired[0].state, STATE_EXPIRED);
    }

    /// 两个判据互斥：同一拍里 `promote` 拿走的行，`expire` 不会同时拿到。
    #[test]
    fn promote_and_expire_never_claim_the_same_row() {
        let dir = TempDir::new("waits-promote-exclusive");
        let store = open_store(&dir);
        store
            .register_wait(
                &bot_id(),
                &new_wait_with_reminders(7, "order-no", "u1", 1_000, 1),
            )
            .expect("register");
        assert_eq!(
            store
                .promote_wait_reminders(&bot_id(), 61_000, 8)
                .expect("promote")
                .len(),
            1
        );
        assert!(
            store
                .promote_wait_reminders(&bot_id(), 61_000, 8)
                .expect("second promote")
                .is_empty(),
            "额度已经用尽，第二拍不该再推后"
        );
        assert!(
            store
                .expire_due_waits(&bot_id(), 61_000, 8)
                .expect("expire")
                .is_empty(),
            "deadline 已经被推后，这一拍没到点"
        );
    }

    /// 没有额度（缺省）的等待直接过期：老行为一字不改。
    #[test]
    fn a_wait_without_reminders_expires_at_once() {
        let dir = TempDir::new("waits-no-reminders");
        let store = open_store(&dir);
        store
            .register_wait(&bot_id(), &new_wait(7, "order-no", "u1", 1_000))
            .expect("register");
        assert!(
            store
                .promote_wait_reminders(&bot_id(), 61_000, 8)
                .expect("promote")
                .is_empty(),
            "没额度就不该被推后"
        );
        assert_eq!(
            store
                .expire_due_waits(&bot_id(), 61_000, 8)
                .expect("expire")
                .len(),
            1
        );
    }

    /// 面板查询「新的在前」：终态行也回（要看得到刚才那条已经超时了）。
    ///
    /// 这里每条等待都先各自到期落终态再登记下一条：同一会话只允许一条 pending
    /// （部分唯一索引），而终态行不占这个位置 —— 面板要显示的正是这些历史。
    #[test]
    fn list_recent_returns_terminal_rows_newest_first() {
        let dir = TempDir::new("waits-recent");
        let store = open_store(&dir);
        for (id, key, owner, at) in [
            (7, "a", "u1", 1_000),
            (8, "b", "u2", 2_000),
            (9, "c", "u3", 3_000),
        ] {
            store
                .register_wait(&bot_id(), &new_wait(id, key, owner, at))
                .expect("register");
            // 每条登记完立刻让它到期，好腾出「同一会话一条 pending」的名额。
            store
                .expire_due_waits(&bot_id(), at + 60_000, 8)
                .expect("expire");
        }

        let all = store
            .list_recent_waits_for_bot(&bot_id(), 10)
            .expect("list");
        let ids: Vec<WaitId> = all.iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![wait_id(9), wait_id(8), wait_id(7)], "新的在前");
        assert!(
            all.iter().all(|row| row.state == STATE_EXPIRED),
            "终态行照样返回，过滤交给 UI"
        );

        let capped = store
            .list_recent_waits_for_bot(&bot_id(), 2)
            .expect("capped");
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].id, wait_id(9));
        assert_eq!(capped[1].id, wait_id(8), "被截断丢掉的是最旧那条");

        assert!(
            store
                .list_recent_waits_for_bot(&bot_id_at(99), 10)
                .expect("other bot")
                .is_empty(),
            "别的 bot 的等待不许串到这里"
        );
    }
}
