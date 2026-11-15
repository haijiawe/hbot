//! `inbound` 表——可靠性与幂等的核心（`docs/02-architecture.md` §4.1、§7）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::ids::InboundId;
use crate::mappers::{INBOUND_COLUMNS, row_to_inbound};
use crate::models::{InboundRow, NewInbound};

/// 插入入站消息；`(bot_id, external_id)` 已存在时**不**插入并返回 `None`。
///
/// **落地顺序硬约束**：调用方必须先写好 `bots` 行与 `conversations` 行，否则这里会因为
/// `foreign_keys=ON` 报 `FOREIGN KEY constraint failed`（不是静默忽略）。适配器的顺序
/// 必须是：写 bot →（首次见到会话时）写 conversation → 写 inbound → 入队。
///
/// 用 `ON CONFLICT DO NOTHING` 让“查重 + 插入”在一个语句里原子完成，避免先 SELECT 后
/// INSERT 的竞态（平台重复投递可能并发到达）。冲突后用一次查询区分两种情况：同一
/// `(bot_id, external_id)` 已存在 = 正常去重；只有主键 `id` 撞了别的消息 = 调用方 bug，
/// 必须报错，绝不能静默丢掉一条新消息。外键/非空违规不是 conflict target，仍会直接报错。
///
/// `sender_id` 是平台侧发送者 ID 的字符串形态（不是 UUID）；平台没提供时传空串。
pub(crate) fn insert_if_new(
    conn: &Connection,
    message: &NewInbound,
    sender_id: &str,
) -> Result<Option<InboundId>> {
    let changed = conn.execute(
        "INSERT INTO inbound
             (id, bot_id, external_id, conversation_id, sender_id, payload_json, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT DO NOTHING",
        params![
            message.id.to_string(),
            message.bot_id.to_string(),
            message.external_id,
            message.conversation_id.to_string(),
            sender_id,
            message.payload_json,
            message.received_at
        ],
    )?;

    if changed == 1 {
        return Ok(Some(message.id));
    }

    // changed == 0 有两种可能：同一 (bot_id, external_id) 已存在（正常去重），
    // 或主键 id 撞上了另一条消息（调用方 bug：不能静默丢弃一条新消息）。
    let existing_id: Option<String> = conn
        .query_row(
            "SELECT id FROM inbound WHERE bot_id = ?1 AND external_id = ?2",
            params![message.bot_id.to_string(), message.external_id],
            |row| row.get(0),
        )
        .optional()?;

    match existing_id {
        Some(_) => Ok(None),
        None => Err(StoreError::IdCollision {
            table: "inbound",
            id: message.id.to_string(),
        }),
    }
}

/// 标记已处理；返回是否真的更新了一行（`false` = ID 不存在）。
pub(crate) fn mark_processed(conn: &Connection, id: &InboundId, processed_at: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE inbound SET processed_at = ?2 WHERE id = ?1",
        params![id.to_string(), processed_at],
    )?;
    Ok(changed == 1)
}

/// 列出未处理的入站消息，按 `(received_at, id)` 升序取前 `limit` 条。
///
/// 排序用 `received_at`（平台时间/接收时间）而不是 `id`：UUID v4 的字典序没有时间含义，
/// 按它排会把乱序投递当成“先到先处理”的假象。
pub(crate) fn list_unprocessed(conn: &Connection, limit: u32) -> Result<Vec<InboundRow>> {
    let sql = format!(
        "SELECT {INBOUND_COLUMNS} FROM inbound
         WHERE processed_at IS NULL
         ORDER BY received_at, id
         LIMIT ?1"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([i64::from(limit)], row_to_inbound)?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}
