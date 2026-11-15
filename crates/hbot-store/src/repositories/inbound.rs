//! `inbound` 表——可靠性与幂等的核心（`docs/02-architecture.md` §4.1、§7）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::ids::InboundId;
use crate::mappers::{INBOUND_COLUMNS, row_to_inbound};
use crate::models::{InboundReplayRow, InboundRow, NewInbound};

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

/// 启动补偿：按 `(received_at, rowid)` 分批取**某个 bot** 的未处理入站消息。
///
/// `after` 是上一批最后一行的游标（`None` = 从最早一条开始）。用 keyset 分页而不是
/// `LIMIT/OFFSET`：补偿期间新消息还在入库，`OFFSET` 会随插入漂移，可能漏行或重复。
///
/// 语义是 **at-least-once**（`docs/02-architecture.md` §5.1）：行在 handler 成功后被
/// `mark_processed` 打标，因此崩溃窗口内可能同一条执行两次——这比“静默丢消息”好，
/// 代价是可能重复回复，代码里不许假装是 exactly-once。
pub(crate) fn list_replay_for_bot(
    conn: &Connection,
    bot_id: &crate::ids::BotId,
    after: Option<(i64, i64)>,
    limit: u32,
) -> Result<Vec<InboundReplayRow>> {
    // 列名必须逐列限定：JOIN 后 `id` / `external_id` 在两张表里都有，不限定会被
    // SQLite 判为 ambiguous。顺序与 `INBOUND_COLUMNS` 一致，这样 `row_to_inbound`
    // 的下标映射不用改。
    let sql = "SELECT inbound.id, inbound.bot_id, inbound.external_id,
                      inbound.conversation_id, inbound.sender_id, inbound.payload_json,
                      inbound.received_at, inbound.rowid, c.external_id, c.kind
         FROM inbound
         JOIN conversations AS c ON c.id = inbound.conversation_id
         WHERE inbound.bot_id = ?1
           AND inbound.processed_at IS NULL
           AND (?2 IS NULL OR inbound.received_at > ?2
                OR (inbound.received_at = ?2 AND inbound.rowid > ?3))
         ORDER BY inbound.received_at, inbound.rowid
         LIMIT ?4";
    let (cursor_received_at, cursor_rowid) = match after {
        Some((received_at, rowid)) => (Some(received_at), rowid),
        None => (None, 0),
    };
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(
        params![
            bot_id.to_string(),
            cursor_received_at,
            cursor_rowid,
            i64::from(limit)
        ],
        |row| {
            let message = row_to_inbound(row)?;
            Ok(InboundReplayRow {
                message,
                rowid: row.get(7)?,
                conversation_external_id: row.get(8)?,
                conversation_kind: row.get(9)?,
            })
        },
    )?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}
