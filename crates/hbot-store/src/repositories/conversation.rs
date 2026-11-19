//! `conversations` 表。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::{BotId, ConversationId};
use crate::mappers::{CONVERSATION_COLUMNS, row_to_conversation};
use crate::models::Conversation;

/// 插入会话。`(bot_id, external_id)` UNIQUE，重复插入会失败——调用方先查再插或改用 upsert。
pub(crate) fn insert(conn: &Connection, conversation: &Conversation) -> Result<()> {
    conn.execute(
        "INSERT INTO conversations (id, bot_id, external_id, kind, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            conversation.id.to_string(),
            conversation.bot_id.to_string(),
            conversation.external_id,
            conversation.kind,
            conversation.created_at
        ],
    )?;
    Ok(())
}

/// 幂等插入会话：主键或 `(bot_id, external_id)` 已存在时不报错，返回是否真的插入了。
///
/// 与 [`insert`] 的分工：那个是「重复即失败」（调用方要自己判冲突，靠错误文本嗅探，
/// 见 `hbot-bot` 的 `is_conversation_conflict`）；这个是「重复即无事发生」，用 SQL 的
/// `INSERT OR IGNORE` 把判据交给唯一约束本身 —— 出站主动消息要发给一个从没见过的
/// 会话时走这条（外键要求 `conversations` 行先存在）。
pub(crate) fn insert_if_absent(conn: &Connection, conversation: &Conversation) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO conversations (id, bot_id, external_id, kind, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            conversation.id.to_string(),
            conversation.bot_id.to_string(),
            conversation.external_id,
            conversation.kind,
            conversation.created_at,
        ],
    )?;
    Ok(inserted > 0)
}

/// 按主键读会话；不存在返回 `Ok(None)`。
pub(crate) fn get(conn: &Connection, id: &ConversationId) -> Result<Option<Conversation>> {
    let sql = format!("SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id.to_string()], row_to_conversation)
        .optional()?)
}

/// 按幂等键 `(bot_id, external_id)` 读会话；不存在返回 `Ok(None)`。
///
/// 这是适配器最常见的查询：平台只给外部会话 ID，要先换成内部 `conversation_id` 才能写
/// `inbound`（外键顺序硬约束，见 `Store::insert_inbound_if_new`）。
pub(crate) fn get_by_external(
    conn: &Connection,
    bot_id: &BotId,
    external_id: &str,
) -> Result<Option<Conversation>> {
    let sql = format!(
        "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE bot_id = ?1 AND external_id = ?2"
    );
    Ok(conn
        .query_row(
            &sql,
            params![bot_id.to_string(), external_id],
            row_to_conversation,
        )
        .optional()?)
}
