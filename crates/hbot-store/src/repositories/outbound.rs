//! `outbound` 表：出站消息先落库再发送，失败按 attempts 重试（`docs/02-architecture.md` §7）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::OutboundId;
use crate::mappers::{OUTBOUND_COLUMNS, row_to_outbound};
use crate::models::{
    NewOutbound, OUTBOUND_STATE_FAILED, OUTBOUND_STATE_PENDING, OUTBOUND_STATE_SENT, OutboundRow,
    OutboundTarget,
};

/// 入队一条出站消息：初始状态 `pending`、`attempts = 0`、`sent_at = NULL`。
pub(crate) fn enqueue(
    conn: &Connection,
    message: &NewOutbound,
    target: &OutboundTarget,
) -> Result<()> {
    conn.execute(
        "INSERT INTO outbound
             (id, bot_id, conversation_id, target_json, text, reply_to, state, attempts, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
        params![
            message.id.to_string(),
            message.bot_id.to_string(),
            message.conversation_id.to_string(),
            target.target_json,
            message.text,
            target.reply_to,
            OUTBOUND_STATE_PENDING,
            message.created_at
        ],
    )?;
    Ok(())
}

/// 标记发送成功；返回是否真的更新了一行（`false` = ID 不存在）。
pub(crate) fn mark_sent(conn: &Connection, id: &OutboundId, sent_at: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE outbound SET state = ?2, sent_at = ?3 WHERE id = ?1",
        params![id.to_string(), OUTBOUND_STATE_SENT, sent_at],
    )?;
    Ok(changed == 1)
}

/// 累加一次发送尝试并返回新的 attempts；行不存在返回 `None`。
///
/// 用 `RETURNING` 把“自增 + 读回”压成一条语句，避免两次往返之间被别的写插队。
pub(crate) fn bump_attempt(conn: &Connection, id: &OutboundId) -> Result<Option<u32>> {
    let attempts: Option<i64> = conn
        .query_row(
            "UPDATE outbound SET attempts = attempts + 1 WHERE id = ?1 RETURNING attempts",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;

    Ok(attempts.map(|value| u32::try_from(value).unwrap_or(u32::MAX)))
}

/// 转为死信：`state = 'failed'` 并记录最终 attempts；返回是否命中一行。
///
/// 保留行而不是删除：UI 要能看到“放弃发送了什么”，排障也需要原始 target/text。
pub(crate) fn mark_failed(conn: &Connection, id: &OutboundId, attempts: u32) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE outbound SET state = ?2, attempts = ?3 WHERE id = ?1",
        params![id.to_string(), OUTBOUND_STATE_FAILED, i64::from(attempts)],
    )?;
    Ok(changed == 1)
}

/// 按主键读一条出站消息；不存在返回 `Ok(None)`。
pub(crate) fn get(conn: &Connection, id: &OutboundId) -> Result<Option<OutboundRow>> {
    let sql = format!("SELECT {OUTBOUND_COLUMNS} FROM outbound WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id.to_string()], row_to_outbound)
        .optional()?)
}

/// 列出待发送的出站消息，按**产生顺序**取前 `limit` 条。
///
/// **排序说明**：先 `created_at`（生产时刻），再 `rowid`（插入顺序）。
/// 为什么不是 `id`：`id` 是 UUID（`ids.rs` 用 `Uuid::new_v4`），字典序与时间无关，
/// 按它续发会把同一会话的回复打乱成随机顺序——而 `AGENTS.md` §4.1 的契约是
/// “一个会话的回复按产生顺序发出”。
///
/// 为什么还要 `rowid`：同一毫秒内产生的多条回复 `created_at` 相同，必须靠插入顺序
/// 兜底。`outbound.id` 是 TEXT 主键，但 SQLite 对非 `WITHOUT ROWID` 表仍维护隐式
/// `rowid`，且它随插入单调递增，正好就是“入库顺序”。
pub(crate) fn list_pending(conn: &Connection, limit: u32) -> Result<Vec<OutboundRow>> {
    let sql = format!(
        "SELECT {OUTBOUND_COLUMNS} FROM outbound
         WHERE state = ?1
         ORDER BY created_at, rowid
         LIMIT ?2"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(
        params![OUTBOUND_STATE_PENDING, i64::from(limit)],
        row_to_outbound,
    )?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}

/// 列出某个 bot 的待发送出站消息，按**产生顺序**（`created_at`, `rowid`）取前 `limit` 条。
///
/// 与 [`list_pending`] 的区别只有 `bot_id` 过滤：进程重启续发时是**按 bot** 恢复的
/// （`hbot_bot::PendingOutboxSource::pending` 只收到一个 `bot_id`），先取全局前 N 条再
/// 在内存里过滤，会被别的 bot 的积压挤出窗口——那等于静默丢消息。
pub(crate) fn list_pending_for_bot(
    conn: &Connection,
    bot_id: &crate::ids::BotId,
    limit: u32,
) -> Result<Vec<OutboundRow>> {
    let sql = format!(
        "SELECT {OUTBOUND_COLUMNS} FROM outbound
         WHERE state = ?1 AND bot_id = ?2
         ORDER BY created_at, rowid
         LIMIT ?3"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(
        params![OUTBOUND_STATE_PENDING, bot_id.to_string(), i64::from(limit)],
        row_to_outbound,
    )?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}

/// 待发送条数（指标与背压用）。与 [`list_pending`] 同源同条件，保证两者一致。
pub(crate) fn count_pending(conn: &Connection) -> Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM outbound WHERE state = ?1",
        [OUTBOUND_STATE_PENDING],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}
