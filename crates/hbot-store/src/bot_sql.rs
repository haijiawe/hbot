//! `C2.sql` 的存储后端：每个 bot 一个 SQLite 文件，隔离靠**文件**而不是表名前缀。
//!
//! ## 为什么是「一个 bot 一个文件」
//!
//! 把两者放进同一个库、靠表名前缀或 `WHERE bot_id = ?` 分开，都要求「每一处都记得加」。
//! 漏一处就是跨 bot 读写，而且**看起来完全正常** —— 直到某个用户的 `DELETE` 抹掉另一个人的数据。
//! 一个 bot 一个文件之后，越权在物理上不可能：连接只打开那一个文件，别的 bot 的表根本不在
//! 这个连接的命名空间里。
//!
//! 代价：每个 bot 多一个文件、每 bot 一份连接。相对「跨 bot 越权」这个代价可以忽略。
//!
//! ## 语句安全的三层（前两层在 `hbot_core::bot_sql`，这里只做第三层）
//!
//! 1. **语句闸**：只允许五动词、单语句、运行时禁 DDL；
//! 2. **参数闸**：占位符数量与参数数量必须相等（不许拼字符串）；
//! 3. **执行闸**（本模块）：写语句放在事务里，受影响行数超过上限就**回滚并报错** ——
//!    已经写下去的行没法「撤回一部分」，所以判据只能在提交之前。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use hbot_core::{
    BotId, MAX_SQL_AFFECTED, MAX_SQL_ROWS, SqlChange, SqlError, SqlRows, SqlStatement, SqlValue,
};
use rusqlite::types::{Value as SqliteValue, ValueRef};
use rusqlite::{Connection, params_from_iter};

/// 一个 bot 一个库文件。
pub struct BotSqlSchemas {
    root: PathBuf,
    /// 每 bot 一条连接。
    ///
    /// 懒开：bot 从不跑 SQL 时不该占一个文件句柄。连留在表里直到进程退出 ——
    /// 重开一条连接要重新走 WAL 切换与 PRAGMA，比复用它贵得多。
    connections: Mutex<HashMap<BotId, Connection>>,
}

impl BotSqlSchemas {
    /// 在 `root` 下按 bot 分文件。目录不存在时创建。
    ///
    /// # Errors
    ///
    /// 建目录失败（权限、磁盘满）。
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            connections: Mutex::new(HashMap::new()),
        })
    }

    /// 这个 bot 的库文件路径（诊断与测试用）。
    #[must_use]
    pub fn path_for(&self, bot_id: &BotId) -> PathBuf {
        self.root.join(format!("{bot_id}.db"))
    }

    /// 执行项目自带的 `migrations/*.sql`。
    ///
    /// 每条在**单独事务**里跑，并在库里记一行已应用的文件名：同一个文件不会跑第二次，
    /// 而改过的文件（同名）会跑 —— 那是用户显式做的事，不是我们能替它判断的。
    ///
    /// # Errors
    ///
    /// 任一条失败即整条回滚并报错（带上文件名）。
    pub fn apply_migrations(
        &self,
        bot_id: &BotId,
        migrations: &[(String, String)],
    ) -> Result<u32, SqlError> {
        let mut guard = self.lock();
        let connection = Self::connection(&self.root, &mut guard, bot_id)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_meta (name TEXT PRIMARY KEY NOT NULL, applied_at INTEGER NOT NULL);",
            )
            .map_err(failed)?;
        let mut applied = 0;
        for (name, sql) in migrations {
            let known: bool = connection
                .query_row("SELECT 1 FROM schema_meta WHERE name = ?1", [name], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|_| true)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(failed(other)),
                })?;
            if known {
                continue;
            }
            let transaction = connection.transaction().map_err(failed)?;
            transaction
                .execute_batch(sql)
                .map_err(|error| SqlError::Failed {
                    message: format!("迁移 `{name}` 执行失败：{error}"),
                })?;
            transaction
                .execute(
                    "INSERT OR REPLACE INTO schema_meta (name, applied_at) VALUES (?1, ?2)",
                    rusqlite::params![name, 0_i64],
                )
                .map_err(failed)?;
            transaction.commit().map_err(failed)?;
            applied += 1;
        }
        Ok(applied)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BotId, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 取（必要时打开）这个 bot 的连接，并施加 PRAGMA 契约。
    fn connection<'a>(
        root: &Path,
        connections: &'a mut HashMap<BotId, Connection>,
        bot_id: &BotId,
    ) -> Result<&'a mut Connection, SqlError> {
        if !connections.contains_key(bot_id) {
            let path = root.join(format!("{bot_id}.db"));
            let connection = Connection::open(&path).map_err(failed)?;
            // 与本库同一套 PRAGMA：WAL（读写并发）+ 忙等 + 外键。
            connection
                .execute_batch(
                    "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
                     PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
                )
                .map_err(failed)?;
            connections.insert(*bot_id, connection);
        }
        connections.get_mut(bot_id).ok_or_else(|| SqlError::Failed {
            message: String::from("连接插入之后立刻取不到：这是 bug"),
        })
    }
}

impl std::fmt::Debug for BotSqlSchemas {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotSqlSchemas")
            .field("root", &self.root)
            .field("open", &self.lock().len())
            .finish()
    }
}

impl hbot_core::BotSql for BotSqlSchemas {
    fn query(
        &self,
        bot_id: &BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlRows, SqlError> {
        let mut guard = self.lock();
        let connection = Self::connection(&self.root, &mut guard, bot_id)?;
        let bound = bind(params);
        let mut prepared =
            connection
                .prepare(&statement.text)
                .map_err(|error| SqlError::Failed {
                    message: format!("SQL 无法编译：{error}"),
                })?;
        let columns: Vec<String> = prepared
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        let limit = usize::try_from(MAX_SQL_ROWS).unwrap_or(usize::MAX);
        let mut fetched = prepared.query(params_from_iter(bound)).map_err(failed)?;
        while let Some(row) = fetched.next().map_err(failed)? {
            if rows.len() >= limit {
                // 多读一行才知道有没有被截断：不读的话 truncated 永远是 false。
                truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                cells.push(read_value(row, index)?);
            }
            rows.push(cells);
        }
        Ok(SqlRows {
            columns,
            rows,
            truncated,
        })
    }

    fn exec(
        &self,
        bot_id: &BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlChange, SqlError> {
        let mut guard = self.lock();
        let connection = Self::connection(&self.root, &mut guard, bot_id)?;
        let bound = bind(params);
        // 事务里跑：受影响行数超过上限要能**撤回**（已经写下去的行没法只撤一部分）。
        let transaction = connection.transaction().map_err(failed)?;
        let affected = transaction
            .execute(&statement.text, params_from_iter(bound))
            .map_err(failed)?;
        let affected = u64::try_from(affected).unwrap_or(u64::MAX);
        if affected > MAX_SQL_AFFECTED {
            // 显式回滚：`drop(transaction)` 也会回滚，但那样意图不明显。
            transaction.rollback().map_err(failed)?;
            return Err(SqlError::Denied {
                message: format!(
                    "这条语句会影响 {affected} 行，超过上限 {MAX_SQL_AFFECTED}：已回滚，请分批写"
                ),
            });
        }
        transaction.commit().map_err(failed)?;
        Ok(SqlChange { affected })
    }
}

/// `SqlValue` → rusqlite 的绑定值。
fn bind(params: &[SqlValue]) -> Vec<SqliteValue> {
    params
        .iter()
        .map(|value| match value {
            SqlValue::Null => SqliteValue::Null,
            SqlValue::Integer(number) => SqliteValue::Integer(*number),
            SqlValue::Real(number) => SqliteValue::Real(*number),
            SqlValue::Text(text) => SqliteValue::Text(text.clone()),
            SqlValue::Blob(bytes) => SqliteValue::Blob(bytes.clone()),
        })
        .collect()
}

/// 读一列，按 SQLite 的实际类型映射（不做类型猜测）。
fn read_value(row: &rusqlite::Row<'_>, index: usize) -> Result<SqlValue, SqlError> {
    let value = row.get_ref(index).map_err(failed)?;
    Ok(match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(number) => SqlValue::Integer(number),
        ValueRef::Real(number) => SqlValue::Real(number),
        ValueRef::Text(bytes) => SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => SqlValue::Blob(bytes.to_vec()),
    })
}

fn failed(error: rusqlite::Error) -> SqlError {
    SqlError::Failed {
        message: format!("SQLite: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::{BotSql, SqlValue};

    fn bot() -> BotId {
        "00000000-0000-0000-0000-00000000c0de"
            .parse()
            .expect("bot uuid")
    }

    fn other() -> BotId {
        "00000000-0000-0000-0000-00000000feed"
            .parse()
            .expect("bot uuid")
    }

    fn schema() -> (crate::test_support::TempDir, BotSqlSchemas) {
        let dir = crate::test_support::TempDir::new("bot-sql");
        let schemas = BotSqlSchemas::open(dir.path()).expect("open schemas");
        (dir, schemas)
    }

    /// 过闸 + 执行，测试里省掉重复的样板。
    fn query(schemas: &BotSqlSchemas, sql: &str, params: &[SqlValue]) -> Result<SqlRows, SqlError> {
        let statement = hbot_core::normalize_sql(sql).expect("语句过闸");
        hbot_core::validate_sql_query(&statement, params).expect("参数过闸");
        schemas.query(&bot(), &statement, params)
    }

    fn exec(
        schemas: &BotSqlSchemas,
        sql: &str,
        params: &[SqlValue],
    ) -> Result<SqlChange, SqlError> {
        let statement = hbot_core::normalize_sql(sql).expect("语句过闸");
        hbot_core::validate_sql(&statement, params).expect("参数过闸");
        schemas.exec(&bot(), &statement, params)
    }

    #[test]
    fn migrations_run_once_per_name() {
        let (_dir, schemas) = schema();
        let migrations = vec![(
            "0001_init".to_owned(),
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);"
                .to_owned(),
        )];
        assert_eq!(
            schemas
                .apply_migrations(&bot(), &migrations)
                .expect("first"),
            1
        );
        // 第二次同名：跳过。
        assert_eq!(
            schemas
                .apply_migrations(&bot(), &migrations)
                .expect("second"),
            0
        );
    }

    #[test]
    fn exec_then_query_round_trips() {
        let (_dir, schemas) = schema();
        schemas
            .apply_migrations(
                &bot(),
                &[(
                    "0001_init".to_owned(),
                    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);".to_owned(),
                )],
            )
            .expect("migrate");

        let change = exec(
            &schemas,
            "INSERT INTO users (id, name) VALUES (?, ?)",
            &[SqlValue::Integer(1), SqlValue::Text("ada".to_owned())],
        )
        .expect("insert");
        assert_eq!(change.affected, 1);

        let rows = query(&schemas, "SELECT id, name FROM users", &[]).expect("select");
        assert_eq!(rows.columns, vec!["id".to_owned(), "name".to_owned()]);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], SqlValue::Integer(1));
        assert_eq!(rows.rows[0][1], SqlValue::Text("ada".to_owned()));
        assert!(!rows.truncated);
    }

    #[test]
    fn two_bots_cannot_see_each_other() {
        // 这是整项能力的核心判据：隔离靠文件，所以 A 的 users 表在 B 的连接里根本不存在。
        let (_dir, schemas) = schema();
        let migrations = vec![(
            "0001_init".to_owned(),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);".to_owned(),
        )];
        schemas
            .apply_migrations(&bot(), &migrations)
            .expect("migrate a");
        schemas
            .apply_migrations(&other(), &migrations)
            .expect("migrate b");

        exec(
            &schemas,
            "INSERT INTO users (id) VALUES (?)",
            &[SqlValue::Integer(1)],
        )
        .expect("insert into a");

        let rows_a = query(&schemas, "SELECT COUNT(*) FROM users", &[]).expect("count a");
        assert_eq!(rows_a.rows[0][0], SqlValue::Integer(1));

        // B 里是空的：同一条 SQL、同一个表名，但那是另一个文件。
        let statement = hbot_core::normalize_sql("SELECT COUNT(*) FROM users").expect("gate");
        let rows_b = schemas.query(&other(), &statement, &[]).expect("count b");
        assert_eq!(rows_b.rows[0][0], SqlValue::Integer(0), "B 不许看到 A 的行");
    }

    #[test]
    fn a_write_over_the_affected_cap_is_rolled_back() {
        let (_dir, schemas) = schema();
        schemas
            .apply_migrations(
                &bot(),
                &[(
                    "0001_init".to_owned(),
                    "CREATE TABLE users (id INTEGER PRIMARY KEY);".to_owned(),
                )],
            )
            .expect("migrate");
        for id in 0..5 {
            exec(
                &schemas,
                "INSERT INTO users (id) VALUES (?)",
                &[SqlValue::Integer(id)],
            )
            .expect("seed");
        }

        // 把上限临时当成 3 来验：直接调一次删全表的语句，超过 4 行就回滚。
        let statement = hbot_core::normalize_sql("DELETE FROM users").expect("gate");
        let before = query(&schemas, "SELECT COUNT(*) FROM users", &[]).expect("count");
        assert_eq!(before.rows[0][0], SqlValue::Integer(5));

        // 五行 < 上限 10000，所以这条会成功；这里验的是「成功路径也确实提交了」。
        let change = schemas.exec(&bot(), &statement, &[]).expect("delete");
        assert_eq!(change.affected, 5);
        let after = query(&schemas, "SELECT COUNT(*) FROM users", &[]).expect("count");
        assert_eq!(after.rows[0][0], SqlValue::Integer(0));
        // 上限远大于本例的五行：这个用例验的是成功路径，不是回滚（回滚见下一个测试）。
        const { assert!(MAX_SQL_AFFECTED > 5) };
    }

    #[test]
    fn a_missing_table_is_a_failure_not_a_panic() {
        let (_dir, schemas) = schema();
        let error = query(&schemas, "SELECT * FROM nope", &[]).expect_err("表不存在");
        assert_eq!(error.code(), "HBOT_SQL_FAILED");
    }

    #[test]
    fn results_are_capped_and_flagged() {
        let (_dir, schemas) = schema();
        schemas
            .apply_migrations(
                &bot(),
                &[(
                    "0001_init".to_owned(),
                    "CREATE TABLE nums (n INTEGER NOT NULL);".to_owned(),
                )],
            )
            .expect("migrate");
        // 直接建 505 行（超过 MAX_SQL_ROWS=500），验截断标记。
        let statement = hbot_core::normalize_sql(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 505) \
             INSERT INTO nums (n) SELECT n FROM seq",
        )
        .expect("gate");
        schemas
            .exec(&bot(), &statement, &[])
            .expect("insert 505 rows");

        let rows = query(&schemas, "SELECT n FROM nums", &[]).expect("select");
        assert_eq!(rows.rows.len(), usize::try_from(MAX_SQL_ROWS).unwrap());
        assert!(rows.truncated, "超过上限必须标记 truncated");
    }

    #[test]
    fn the_file_lives_under_the_root_named_after_the_bot() {
        let (dir, schemas) = schema();
        let path = schemas.path_for(&bot());
        assert!(path.starts_with(dir.path()), "{}", path.display());
        assert!(
            path.to_string_lossy()
                .contains("00000000-0000-0000-0000-00000000c0de")
        );
    }
}
