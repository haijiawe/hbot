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

/// 列出待发送的出站消息，按 `id` 升序取前 `limit` 条。
///
/// **排序说明**：按 `id`（UUID 字符串）升序是调用方契约（续发顺序与落库顺序一致由
/// 调用方保证 id 的生成方式决定）。UUID v4 的字典序本身不含时间含义，若上游用 v4 随机
/// 生成，续发顺序只是“确定但不一定按时间”——要按时间续发请看 `created_at` 字段。
pub(crate) fn list_pending(conn: &Connection, limit: u32) -> Result<Vec<OutboundRow>> {
    let sql = format!(
        "SELECT {OUTBOUND_COLUMNS} FROM outbound
         WHERE state = ?1
         ORDER BY id
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

/// 待发送条数（指标与背压用）。与 [`list_pending`] 同源同条件，保证两者一致。
pub(crate) fn count_pending(conn: &Connection) -> Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM outbound WHERE state = ?1",
        [OUTBOUND_STATE_PENDING],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}
