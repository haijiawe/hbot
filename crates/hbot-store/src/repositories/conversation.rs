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
