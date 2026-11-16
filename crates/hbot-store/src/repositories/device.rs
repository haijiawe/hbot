//! `device_sessions` / `device_audit`：设备能力的会话与审计（`docs/09-device-capability.md` §3）。
//!
//! 两张表都由**上层**（`hbot-rpc` 的设备装配）驱动：存储层不判断状态迁移是否合法、不解析
//! 子能力 JSON、不读时钟。它只保证两件存储侧的事：行是原子的（撤销一条语句完成），
//! 读回来的顺序是确定的（`created_at DESC, id DESC`，不靠 rowid）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::BotId;
use crate::mappers::{
    DEVICE_AUDIT_COLUMNS, DEVICE_SESSION_COLUMNS, row_to_device_audit, row_to_device_session,
};
use crate::models::{DeviceAuditRow, DeviceSessionRow, NewDeviceAudit, NewDeviceSession};

/// `device_sessions.state` 里"生效中"的取值。
///
/// 与 `hbot_core::DeviceSessionState::Active.as_str()` 必须同值：`end_live_sessions` 的
/// `WHERE` 子句靠它筛 `active` 行。存储层不 import 领域枚举（模型层只存字符串），所以这条
/// 别名由 `device_tests::active_state_literal_matches_the_core_contract` 钉住 ——
/// 改单边就红，不会静默变成"什么都撤销不掉"。
pub(crate) const STATE_ACTIVE: &str = "active";

/// 插入一条设备会话，返回**落库后的行**。
///
/// 读回而不是把入参原样回显：`ended_at` 由 schema 决定（新会话必为 NULL），回显入参会
/// 让"插入参数"与"库里的行"变成两个可以各自漂移的形状。
pub(crate) fn insert_session(
    conn: &Connection,
    session: &NewDeviceSession,
) -> Result<DeviceSessionRow> {
    conn.execute(
        "INSERT INTO device_sessions
             (id, bot_id, state, sub_json, allowed_apps_json, granted_at, deadline, ended_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)",
        params![
            session.id,
            session.bot_id.to_string(),
            session.state,
            session.sub_json,
            session.allowed_apps_json,
            session.granted_at,
            session.deadline,
            session.created_at
        ],
    )?;
    let sql = format!("SELECT {DEVICE_SESSION_COLUMNS} FROM device_sessions WHERE id = ?1");
    Ok(conn.query_row(&sql, params![session.id], row_to_device_session)?)
}

/// 取这个 bot 最近一次设备会话；从没有过时返回 `None`。
///
/// 排序是 `(created_at DESC, id DESC)`：第二键不是"随便写的"。UUIDv4 没有时间含义，
/// 按 id 字典序定序等于随机；同一毫秒插入的两行只能靠 `id DESC` 做**确定性**定序，
/// 否则同一条 SQL 在两次执行里可以给出不同的行（rowid 顺序不是契约）。
pub(crate) fn latest_session(
    conn: &Connection,
    bot_id: &BotId,
) -> Result<Option<DeviceSessionRow>> {
    let sql = format!(
        "SELECT {DEVICE_SESSION_COLUMNS} FROM device_sessions
         WHERE bot_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![bot_id.to_string()], row_to_device_session)
        .optional()?)
}

/// 改一条会话的状态（可同时写 `ended_at`），返回是否命中一行。
///
/// `ended_at = None` 会把该列清成 NULL：想"只改状态、保留结束时刻"的调用方要把原值一起传回来。
pub(crate) fn update_state(
    conn: &Connection,
    id: &str,
    state: &str,
    ended_at: Option<i64>,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE device_sessions SET state = ?2, ended_at = ?3 WHERE id = ?1",
        params![id, state, ended_at],
    )?;
    Ok(changed == 1)
}

/// 把这个 bot 所有 `active` 会话置为 `state` 并写 `ended_at`，返回处理行数。
///
/// 撤销与到期共用它（`docs/09` §7）：一次 UPDATE 原子完成"全局失效"，不需要先读再逐条改
/// —— 后者会在两条语句之间留下"库里已撤销、另一个读者还看到 active"的窗口。
/// 非 `active` 行（`requested` / `expired` / `revoked` / `denied`）**不动**：它们已经结束了，
/// 改写会把审计里的历史擦掉。
pub(crate) fn end_live_sessions(
    conn: &Connection,
    bot_id: &BotId,
    state: &str,
    ended_at: i64,
) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE device_sessions SET state = ?2, ended_at = ?3
         WHERE bot_id = ?1 AND state = ?4",
        params![bot_id.to_string(), state, ended_at, STATE_ACTIVE],
    )?;
    Ok(changed)
}

/// 追加一条设备审计（不返回行：调用方不需要 id，读回来走 [`list_audit`]）。
///
/// `target` 只允许进程名 / 窗口 id / 子能力名；像素、剪贴板文本、OCR 原文一律不许
/// （`docs/09` §3.2、§4.3）。表结构里根本没有能装内容的列，这条是结构约束，不是调用约定。
pub(crate) fn append_audit(conn: &Connection, entry: &NewDeviceAudit) -> Result<()> {
    conn.execute(
        "INSERT INTO device_audit (at, bot_id, actor, action, target, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            entry.at,
            entry.bot_id.to_string(),
            entry.actor,
            entry.action,
            entry.target,
            entry.outcome
        ],
    )?;
    Ok(())
}

/// 读某个 bot 最近的审计，最新在前：`ORDER BY at DESC, id DESC`。
///
/// 只按 bot 过滤（不跨 bot 汇总）：审计要回答的是"这个 bot 刚才动了什么"，混进别人的行只会
/// 增加误读。同一毫秒的多条用 `id`（AUTOINCREMENT = 写入序）定序 —— `at` 是调用方给的时钟，
/// 回拨时不保证单调。
pub(crate) fn list_audit(
    conn: &Connection,
    bot_id: &BotId,
    limit: usize,
) -> Result<Vec<DeviceAuditRow>> {
    let sql = format!(
        "SELECT {DEVICE_AUDIT_COLUMNS} FROM device_audit
         WHERE bot_id = ?1
         ORDER BY at DESC, id DESC
         LIMIT ?2"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(
        params![bot_id.to_string(), i64::try_from(limit).unwrap_or(i64::MAX)],
        row_to_device_audit,
    )?;
    let mut collected = Vec::new();
    for row in rows {
        collected.push(row?);
    }
    Ok(collected)
}
