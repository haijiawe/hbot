//! `bots` 表。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::BotId;
use crate::mappers::{BOT_COLUMNS, BOT_STATUS_COLUMNS, row_to_bot, row_to_bot_status};
use crate::models::{Bot, BotStatusRow};

/// 插入 bot。
pub(crate) fn insert(conn: &Connection, bot: &Bot) -> Result<()> {
    conn.execute(
        "INSERT INTO bots (id, project_id, name, platform, config_json, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            bot.id.to_string(),
            bot.project_id.to_string(),
            bot.name,
            bot.platform,
            bot.config_json,
            // SQLite 无 bool：存 0/1，读侧再还原。
            i64::from(bot.enabled),
            bot.created_at
        ],
    )?;
    Ok(())
}

/// 按主键读一个 bot；不存在返回 `Ok(None)`（不是错误）。
pub(crate) fn get(conn: &Connection, id: &BotId) -> Result<Option<Bot>> {
    let sql = format!("SELECT {BOT_COLUMNS} FROM bots WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id.to_string()], row_to_bot)
        .optional()?)
}

/// 列出全部 bot，按创建时间升序（同批创建时按 id 兜底，保证稳定）。
pub(crate) fn list(conn: &Connection) -> Result<Vec<Bot>> {
    let sql = format!("SELECT {BOT_COLUMNS} FROM bots ORDER BY created_at, id");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], row_to_bot)?;
    let mut bots = Vec::new();
    for row in rows {
        bots.push(row?);
    }
    Ok(bots)
}

/// 更新 bot 运行状态；返回是否命中一行（`false` = ID 不存在）。
///
/// 取值合法性由 `Store::update_bot_status` 在入队前校验（本函数不做业务校验）。
pub(crate) fn update_status(
    conn: &Connection,
    id: &BotId,
    status: &str,
    updated_at: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE bots SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id.to_string(), status, updated_at],
    )?;
    Ok(changed == 1)
}

/// 读 bot 的运行状态；不存在返回 `Ok(None)`。
pub(crate) fn get_status(conn: &Connection, id: &BotId) -> Result<Option<BotStatusRow>> {
    let sql = format!("SELECT {BOT_STATUS_COLUMNS} FROM bots WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id.to_string()], row_to_bot_status)
        .optional()?)
}
