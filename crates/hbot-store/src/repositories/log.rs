//! `logs` 表：Bot 运行日志落库（UI 的 `run.log` 事件从这读）。

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::ids::BotId;
use crate::mappers::{LOG_COLUMNS, row_to_log};
use crate::models::{LogRow, NewAgentLog, NewLog};

/// 追加一条 Bot 日志。`id` 由 SQLite AUTOINCREMENT 生成，保证同一毫秒内的先后可区分。
pub(crate) fn append(conn: &Connection, entry: &NewLog) -> Result<()> {
    conn.execute(
        "INSERT INTO logs (bot_id, level, line, at, source) VALUES (?1, ?2, ?3, ?4, 'bot')",
        params![entry.bot_id.to_string(), entry.level, entry.line, entry.at],
    )?;
    Ok(())
}

/// 追加一条 Agent 日志：没有 bot 归属，`bot_id = NULL`、`source = 'agent'`。
pub(crate) fn append_agent(conn: &Connection, entry: &NewAgentLog) -> Result<()> {
    conn.execute(
        "INSERT INTO logs (bot_id, level, line, at, source) VALUES (NULL, ?1, ?2, ?3, 'agent')",
        params![entry.level, entry.line, entry.at],
    )?;
    Ok(())
}

/// 读最近的日志：按 `id` **降序**（最新在前）取前 `limit` 条。
///
/// `bot_id = None` 表示跨 bot 汇总（总览页）；否则只看该 bot。用 `id` 而不是 `at` 排序：
/// `at` 是调用方给的时间，时钟回拨或同一毫秒多条时会给不出稳定顺序，`id` 是写入序。
pub(crate) fn list(conn: &Connection, bot_id: Option<&BotId>, limit: u32) -> Result<Vec<LogRow>> {
    let mut logs = Vec::new();
    match bot_id {
        Some(bot_id) => {
            let sql = format!(
                "SELECT {LOG_COLUMNS} FROM logs WHERE bot_id = ?1 ORDER BY id DESC LIMIT ?2"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows =
                statement.query_map(params![bot_id.to_string(), i64::from(limit)], row_to_log)?;
            for row in rows {
                logs.push(row?);
            }
        }
        None => {
            let sql = format!("SELECT {LOG_COLUMNS} FROM logs ORDER BY id DESC LIMIT ?1");
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map([i64::from(limit)], row_to_log)?;
            for row in rows {
                logs.push(row?);
            }
        }
    }
    Ok(logs)
}
