//! SQL 能力的宿主端口：把 `ctx.sql` 接到这个 bot 自己的库文件上。
//!
//! `hbot-store` 的 `BotSqlSchemas` 已经做完了三件事：按 bot 分文件、跑迁移、在事务里执行写
//! 语句并在越界时回滚。这里只补两件宿主才知道的事：
//!
//! 1. **清单里有没有声明** `capabilities.state.sql`（声明层判据，每次起 isolate 现读）；
//! 2. **项目的迁移文件在哪**（`migrations/*.sql`，启动时执行一次）。
//!
//! 为什么不把这两件塞进 `hbot-store`：它不认识 `hbot.json`（依赖方向只允许 store → core），
//! 也不该为了一个能力去读项目目录。

use std::path::{Path, PathBuf};

use hbot_core::BotId;
use hbot_store::BotSqlSchemas;

/// 这个项目的 SQL 后端：一个共享的 schema 集 + 这个 bot 的身份。
pub struct StoreSqlBackend {
    bot_id: BotId,
    project_root: PathBuf,
    schemas: std::sync::Arc<BotSqlSchemas>,
}

impl StoreSqlBackend {
    /// 装配一个端口。
    #[must_use]
    pub fn new(
        bot_id: BotId,
        project_root: PathBuf,
        schemas: std::sync::Arc<BotSqlSchemas>,
    ) -> Self {
        Self {
            bot_id,
            project_root,
            schemas,
        }
    }

    /// 跑一次项目的 `migrations/*.sql`（启动时调一次）。
    ///
    /// 返回应用了几条；目录不存在或没有文件时返回 0 —— **不是错误**：一个不用 SQL 的
    /// 项目没有 migrations 目录是正常的。
    ///
    /// # Errors
    ///
    /// 任一条迁移执行失败（带上文件名，由 store 侧构造）。
    pub fn apply_migrations(&self) -> Result<u32, hbot_core::SqlError> {
        let migrations = project_migrations(&self.project_root);
        if migrations.is_empty() {
            return Ok(0);
        }
        self.schemas.apply_migrations(&self.bot_id, &migrations)
    }
}

impl hbot_code::sql::SqlBackend for StoreSqlBackend {
    fn query(
        &self,
        bot_id: &BotId,
        statement: &hbot_core::SqlStatement,
        params: &[hbot_core::SqlValue],
    ) -> Result<hbot_core::SqlRows, hbot_core::SqlError> {
        hbot_core::BotSql::query(&*self.schemas, bot_id, statement, params)
    }

    fn exec(
        &self,
        bot_id: &BotId,
        statement: &hbot_core::SqlStatement,
        params: &[hbot_core::SqlValue],
    ) -> Result<hbot_core::SqlChange, hbot_core::SqlError> {
        hbot_core::BotSql::exec(&*self.schemas, bot_id, statement, params)
    }
}

impl std::fmt::Debug for StoreSqlBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreSqlBackend")
            .field("bot_id", &self.bot_id)
            .finish_non_exhaustive()
    }
}

/// 清单里是否声明了 `capabilities.state.sql`（宿主侧读它决定装不装端口）。
#[must_use]
pub fn declared_sql(root: &Path) -> bool {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return false;
    };
    manifest.capabilities.sql()
}

/// 读项目的迁移文件（`migrations/*.sql`，按文件名排序）。
///
/// 排序必须稳定：迁移之间有依赖（先建表再建索引），而目录遍历顺序在三个平台上都不保证。
fn project_migrations(root: &Path) -> Vec<(String, String)> {
    let dir = root.join("migrations");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let sql = std::fs::read_to_string(&path).ok()?;
            Some((name, sql))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_without_a_manifest_declares_no_sql() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(!declared_sql(&root));
    }

    #[test]
    fn a_project_without_a_migrations_directory_has_none() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(project_migrations(&root).is_empty());
    }
}
