//! 迁移执行器：按版本号顺序执行、重复执行安全、已发布迁移只增不改。
//!
//! 版本水位同时写两处（`docs/02-architecture.md` §7 要求 `PRAGMA user_version`）：
//! `schema_migrations` 表记录每条的版本与名字，`PRAGMA user_version` 记录最新版本，
//! 便于外部工具与 `docs/05-quality-gates.md` G6 的“user_version 单调”判定。
//!
//! 幂等性来自两件事：迁移 SQL 自身使用 `CREATE TABLE/INDEX IF NOT EXISTS`；执行器只跑
//! 尚未记录在 `schema_migrations` 里的版本。每条迁移在**单独事务**里执行，失败即整条回滚，
//! 不会留下半迁移的库。

use rusqlite::{Connection, params};

use crate::error::{Result, StoreError};
use crate::startup_trace;

/// 一条嵌入式迁移。SQL 通过 `include_str!` 编译进二进制，运行时不读文件。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    /// 单调递增的版本号，与文件名前缀一致。
    pub(crate) version: u32,
    /// 记录在 `schema_migrations.name` 里；已发布后不许改。
    pub(crate) name: &'static str,
    /// 迁移 SQL。
    pub(crate) sql: &'static str,
}

/// 全部迁移，按版本号升序。
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_init",
        sql: include_str!("migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "0002_outbound_routing_and_status",
        sql: include_str!("migrations/0002_outbound_routing_and_status.sql"),
    },
    Migration {
        version: 3,
        name: "0003_agent_logs",
        sql: include_str!("migrations/0003_agent_logs.sql"),
    },
    Migration {
        version: 4,
        name: "0004_device",
        sql: include_str!("migrations/0004_device.sql"),
    },
    Migration {
        version: 5,
        name: "0005_bot_kv",
        sql: include_str!("migrations/0005_bot_kv.sql"),
    },
    Migration {
        version: 6,
        name: "0006_bot_jobs",
        sql: include_str!("migrations/0006_bot_jobs.sql"),
    },
    Migration {
        version: 7,
        name: "0007_bot_waits",
        sql: include_str!("migrations/0007_bot_waits.sql"),
    },
    Migration {
        version: 8,
        name: "0008_bot_wait_reminders",
        sql: include_str!("migrations/0008_bot_wait_reminders.sql"),
    },
    Migration {
        version: 9,
        name: "0009_bot_file_usage",
        sql: include_str!("migrations/0009_bot_file_usage.sql"),
    },
    Migration {
        version: 10,
        name: "0010_outbound_attachments",
        sql: include_str!("migrations/0010_outbound_attachments.sql"),
    },
];

/// 本二进制支持的最新版本。
pub(crate) fn latest_version() -> u32 {
    // MIGRATIONS 是编译期常量且非空；空表时按 0 处理，不 panic。
    MIGRATIONS.last().map_or(0, |migration| migration.version)
}

/// 给迁移框架自己用的表；不属于任何业务迁移。
const CREATE_SCHEMA_MIGRATIONS: &str = "
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL,
    applied_at INTEGER NOT NULL
);";

/// 执行所有未应用的迁移，返回执行后的最新版本。
///
/// 需要 `&mut Connection` 以开启事务。安全点：任何一条失败都 `Err` 返回且该条回滚，
/// 之前的迁移保持已提交状态——下次启动会从失败那条重试。
pub(crate) fn apply(conn: &mut Connection, applied_at: i64) -> Result<u32> {
    let phase = startup_trace::Phase::start("mig_schema_table");
    conn.execute_batch(CREATE_SCHEMA_MIGRATIONS)?;
    phase.done();

    let phase = startup_trace::Phase::start("mig_read_applied");
    let applied = read_applied(conn)?;
    if startup_trace::enabled() {
        phase.done_with(&format!("rows={}", applied.len()));
    } else {
        phase.done();
    }
    let supported = latest_version();

    for (version, recorded) in &applied {
        if *version > supported {
            return Err(StoreError::SchemaTooNew {
                found: *version,
                supported,
            });
        }
        if let Some(migration) = MIGRATIONS.iter().find(|m| m.version == *version) {
            if recorded != migration.name {
                return Err(StoreError::MigrationMismatch {
                    version: *version,
                    name: migration.name,
                    recorded: recorded.clone(),
                });
            }
        }
    }

    let phase = startup_trace::Phase::start("mig_user_version");
    let user_version = read_user_version(conn)?;
    phase.done();
    if user_version > i64::from(supported) {
        return Err(StoreError::SchemaTooNew {
            found: u32::try_from(user_version).unwrap_or(supported),
            supported,
        });
    }

    for migration in MIGRATIONS {
        if applied
            .iter()
            .any(|(version, _)| *version == migration.version)
        {
            continue;
        }
        let phase = startup_trace::Phase::start("mig_apply_one");
        let transaction = conn.transaction()?;
        transaction
            .execute_batch(migration.sql)
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![migration.version, migration.name, applied_at],
            )
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        // PRAGMA 不支持绑定参数，版本号来自编译期常量，无注入面。
        transaction
            .execute_batch(&format!("PRAGMA user_version = {};", migration.version))
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction.commit()?;
        phase.done();
        tracing::info!(
            version = migration.version,
            name = migration.name,
            "applied store migration"
        );
    }

    // 老库（user_version 未维护）补齐水位，保证单调。
    let phase = startup_trace::Phase::start("mig_user_version_fixup");
    let current = read_user_version(conn)?;
    if current < i64::from(supported) {
        conn.execute_batch(&format!("PRAGMA user_version = {supported};"))?;
    }
    phase.done();

    Ok(supported)
}

/// 读出已应用迁移，升序。
fn read_applied(conn: &Connection) -> Result<Vec<(u32, String)>> {
    let mut statement =
        conn.prepare("SELECT version, name FROM schema_migrations ORDER BY version")?;
    let rows = statement.query_map([], |row| {
        let version: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        Ok((version, name))
    })?;
    let mut applied = Vec::new();
    for row in rows {
        let (version, name) = row?;
        // 版本号在库里是 INTEGER；越界说明库被外部工具动过，按“太高”处理而不是截断。
        match u32::try_from(version) {
            Ok(version) => applied.push((version, name)),
            Err(_) => {
                return Err(StoreError::SchemaTooNew {
                    found: u32::MAX,
                    supported: latest_version(),
                });
            }
        }
    }
    Ok(applied)
}

/// `PRAGMA user_version`（i64）。
fn read_user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_versions_are_strictly_increasing() {
        let mut previous = 0;
        for migration in MIGRATIONS {
            assert!(
                migration.version > previous,
                "migration version {} is not increasing after {previous}",
                migration.version
            );
            previous = migration.version;
        }
    }

    #[test]
    fn migration_names_are_unique() {
        let mut names: Vec<&str> = MIGRATIONS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), MIGRATIONS.len());
    }
}
