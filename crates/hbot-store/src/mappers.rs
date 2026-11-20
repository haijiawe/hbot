//! 行 → 实体映射。字段顺序必须与对应的 `*_COLUMNS` 一致。
//!
//! ID 列在库里是 TEXT（UUID 字符串）；解析失败说明库被外部改坏，按 `Sqlite` 错误上报
//! （`FromSqlConversionFailure`），**不**退化成默认值或空 ID。
//!
//! 每个表的列清单都是显式常量，禁止 `SELECT *`：加列后 `SELECT *` 会静默错位，而
//! `row.get(索引)` 的错位在类型相同时不会报错。

use std::str::FromStr;

use rusqlite::Row;
use rusqlite::types::Type;

use crate::models::{
    Bot, BotStatusRow, Conversation, DeviceAuditRow, DeviceSessionRow, InboundRow, LogRow,
    OutboundRow, Project,
};

/// `projects` 表的显式列清单（顺序见 [`row_to_project`]）。
pub(crate) const PROJECT_COLUMNS: &str = "id, name, path, created_at";

/// `bots` 表的显式列清单；避免 `SELECT *` 在加列后静默错位。
pub(crate) const BOT_COLUMNS: &str =
    "id, project_id, name, platform, config_json, enabled, created_at";

/// `bots` 的状态列清单（0002 新增，见 [`BotStatusRow`]）。
pub(crate) const BOT_STATUS_COLUMNS: &str = "id, status, updated_at";

/// `conversations` 表的显式列清单。
pub(crate) const CONVERSATION_COLUMNS: &str = "id, bot_id, external_id, kind, created_at";

/// `inbound` 表的显式列清单（顺序见 [`row_to_inbound`]）。
pub(crate) const INBOUND_COLUMNS: &str =
    "id, bot_id, external_id, conversation_id, sender_id, payload_json, received_at";

/// `outbound` 表的显式列清单（顺序见 [`row_to_outbound`]）。
pub(crate) const OUTBOUND_COLUMNS: &str = concat!(
    "id, bot_id, conversation_id, target_json, text, reply_to, kind, attachment_json, ",
    "attempts, created_at"
);

/// `logs` 表的显式列清单（顺序见 [`row_to_log`]）。
pub(crate) const LOG_COLUMNS: &str = "id, bot_id, level, line, at, source";

/// `device_sessions` 表的显式列清单（顺序见 [`row_to_device_session`]）。
pub(crate) const DEVICE_SESSION_COLUMNS: &str = concat!(
    "id, bot_id, state, sub_json, allowed_apps_json, ",
    "granted_at, deadline, ended_at, created_at"
);

/// `device_audit` 表的显式列清单（顺序见 [`row_to_device_audit`]）。
pub(crate) const DEVICE_AUDIT_COLUMNS: &str = "id, at, bot_id, actor, action, target, outcome";

/// 把一行 `projects` 读成 [`Project`]。
pub(crate) fn row_to_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        name: row.get(1)?,
        path: row.get(2)?,
        created_at: row.get(3)?,
    })
}

/// 把一行 `bots` 读成 [`Bot`]。
pub(crate) fn row_to_bot(row: &Row<'_>) -> rusqlite::Result<Bot> {
    Ok(Bot {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        project_id: parse_id(1, row.get::<_, String>(1)?)?,
        name: row.get(2)?,
        platform: row.get(3)?,
        config_json: row.get(4)?,
        // SQLite 无 bool：0 = 关闭，非 0 = 开启（与写入侧保持一致）。
        enabled: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
    })
}

/// 把一行 `bots` 的状态列读成 [`BotStatusRow`]。
pub(crate) fn row_to_bot_status(row: &Row<'_>) -> rusqlite::Result<BotStatusRow> {
    Ok(BotStatusRow {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        status: row.get(1)?,
        updated_at: row.get(2)?,
    })
}

/// 把一行 `conversations` 读成 [`Conversation`]。
pub(crate) fn row_to_conversation(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        bot_id: parse_id(1, row.get::<_, String>(1)?)?,
        external_id: row.get(2)?,
        kind: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// 把一行 `inbound` 读成 [`InboundRow`]。
pub(crate) fn row_to_inbound(row: &Row<'_>) -> rusqlite::Result<InboundRow> {
    Ok(InboundRow {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        bot_id: parse_id(1, row.get::<_, String>(1)?)?,
        external_id: row.get(2)?,
        conversation_id: parse_id(3, row.get::<_, String>(3)?)?,
        sender_id: row.get(4)?,
        payload_json: row.get(5)?,
        received_at: row.get(6)?,
    })
}

/// 把一行 `outbound` 读成 [`OutboundRow`]。
pub(crate) fn row_to_outbound(row: &Row<'_>) -> rusqlite::Result<OutboundRow> {
    Ok(OutboundRow {
        id: parse_id(0, row.get::<_, String>(0)?)?,
        bot_id: parse_id(1, row.get::<_, String>(1)?)?,
        conversation_id: parse_id(2, row.get::<_, String>(2)?)?,
        target_json: row.get(3)?,
        text: row.get(4)?,
        reply_to: row.get(5)?,
        kind: row.get(6)?,
        attachment_json: row.get(7)?,
        // attempts 是 INTEGER；越界（外部工具写坏）按 u32::MAX 上报而不是截断成 0。
        attempts: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(u32::MAX),
        created_at: row.get(9)?,
    })
}

/// 把一行 `logs` 读成 [`LogRow`]。
pub(crate) fn row_to_log(row: &Row<'_>) -> rusqlite::Result<LogRow> {
    Ok(LogRow {
        id: row.get(0)?,
        // Agent 行的 bot_id 是 NULL；`Option<String>` 只把 NULL 映射成 None，
        // 非 NULL 仍走强类型解析（坏 UUID 照样报错，不会静默变成 None）。
        bot_id: match row.get::<_, Option<String>>(1)? {
            Some(raw) => Some(parse_id(1, raw)?),
            None => None,
        },
        level: row.get(2)?,
        line: row.get(3)?,
        at: row.get(4)?,
        source: row.get(5)?,
    })
}

/// 把一行 `device_sessions` 读成 [`DeviceSessionRow`]。
///
/// `state` / `sub_json` / `allowed_apps_json` 原样读出（存储层不解释领域枚举，也不解 JSON）。
pub(crate) fn row_to_device_session(row: &Row<'_>) -> rusqlite::Result<DeviceSessionRow> {
    Ok(DeviceSessionRow {
        id: row.get(0)?,
        bot_id: parse_id(1, row.get::<_, String>(1)?)?,
        state: row.get(2)?,
        sub_json: row.get(3)?,
        allowed_apps_json: row.get(4)?,
        granted_at: row.get(5)?,
        deadline: row.get(6)?,
        ended_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// 把一行 `device_audit` 读成 [`DeviceAuditRow`]。
pub(crate) fn row_to_device_audit(row: &Row<'_>) -> rusqlite::Result<DeviceAuditRow> {
    Ok(DeviceAuditRow {
        id: row.get(0)?,
        at: row.get(1)?,
        bot_id: parse_id(2, row.get::<_, String>(2)?)?,
        actor: row.get(3)?,
        action: row.get(4)?,
        target: row.get(5)?,
        outcome: row.get(6)?,
    })
}

/// 把 TEXT 列解析成强类型 ID。
///
/// `index` 只用于错误定位（与 `SELECT` 列序一致）。`uuid::Error` 会被包进
/// `rusqlite::Error::FromSqlConversionFailure`，因此调用方看到的是"存储层读到了坏数据"，
/// 而不是一条被吞掉的空 ID。
fn parse_id<T>(index: usize, raw: String) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    raw.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}
