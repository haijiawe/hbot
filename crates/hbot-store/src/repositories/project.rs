//! `projects` 表。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::mappers::{PROJECT_COLUMNS, row_to_project};
use crate::models::Project;

/// 插入项目。同一 `path` 再插会因 UNIQUE 约束失败，由调用方决定是否先查后插。
pub(crate) fn insert(conn: &Connection, project: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            project.id.to_string(),
            project.name,
            project.path,
            project.created_at
        ],
    )?;
    Ok(())
}

/// 按 `path` 读一个项目；不存在返回 `Ok(None)`（不是错误）。
///
/// `bot_start` 用它判断"这个项目根是否已经登记过"：`bots.project_id` 有指向
/// `projects(id)` 的外键，而库开启 `foreign_keys=ON`，未登记就直接写 bot 行会撞 FK。
pub(crate) fn get_by_path(conn: &Connection, path: &str) -> Result<Option<Project>> {
    let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE path = ?1");
    Ok(conn.query_row(&sql, [path], row_to_project).optional()?)
}
