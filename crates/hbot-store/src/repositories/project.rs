//! `projects` 表。

use rusqlite::{Connection, params};

use crate::error::Result;
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
