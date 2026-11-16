//! 迁移失败时的可靠性契约：**失败那条整体回滚、原库仍可打开、数据不丢**。
//!
//! 现有单元测试（`src/tests.rs`）只覆盖了「`schema_version` 能重放」这一面。这里补的是
//! 另一半：一条真实迁移 SQL 执行到一半报错时，库会变成什么样。
//!
//! 做法是**不碰源码**、只造数据：先手工建一个「v1 状态」的库（`0001_init.sql` +
//! `schema_migrations` 只有 v1 + `user_version = 1`），再预先加上一条 0002 第一条语句要
//! 添加的列（`bots.status`），于是 `0002_outbound_routing_and_status.sql` 的第一句
//! `ALTER TABLE bots ADD COLUMN status …` 会撞上 `duplicate column name: status`。
//! 这就是一次货真价实的「迁移执行到一半失败」——不是 mock、不是改写 SQL。
//!
//! 约定的语义（`src/migrations.rs::apply` 的实现）：每条迁移各自一个事务，失败那条
//! 整体回滚；**`Store::open` 直接返回 `Err`**（不会带着半个 schema 继续跑）。所以这里
//! 断言三件事：
//!
//! 1. `Store::open` 报出 `StoreError::Migration { version: 2, .. }`；
//! 2. 用原生 `rusqlite` 仍能打开同一个文件——数据一条不少，`user_version` 还停在 1，
//!    `schema_migrations` 里只有 v1；
//! 3. 失败那条的**其它**语句一句都没落库（列、索引都不存在）——这是「回滚」而不是
//!    「执行了一部分」的唯一硬证据。
//!
//! 另外两条：失败是**可重复的**（重开还是同样的错，不是半提交后的另一种状态），并与一条
//! 对照组（同样的 fixture、不制造冲突 → 能升到 v2 且数据不丢）比较，证明失败来自冲突而
//! 不是 fixture 本身不合法。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_store::{Store, StoreError};
use rusqlite::{Connection, params};

/// v1 的建表 SQL：直接引用真身，避免测试里出现第二份 schema。
const INIT_SQL: &str = include_str!("../src/migrations/0001_init.sql");

/// `migrations.rs` 里的 `CREATE_SCHEMA_MIGRATIONS` 原文。
///
/// 刻意在这里抄一份而不是从源码导出：这份 fixture 描述的是「用户机器上已有的库长什么样」，
/// 它必须在实现改动时**独立地**陈旧下去（真源变了这条测试才会红）。两边一起变就等于没测。
const CREATE_SCHEMA_MIGRATIONS: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
        version    INTEGER PRIMARY KEY NOT NULL,
        name       TEXT NOT NULL,
        applied_at INTEGER NOT NULL
    );";

const PROJECT_ID: &str = "11111111-1111-4111-8111-111111111111";
const BOT_ID: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_NAME: &str = "迁移失败也不许丢的项目";
const BOT_NAME: &str = "迁移失败也不许丢的 bot";
const LOG_LINE: &str = "迁移前的日志，必须还在";

/// 一次性目录：没有 `tempfile` 这个 dev-dep，自己造（与 `src/test_support.rs` 同风格）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-store-migrate-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    fn db(&self) -> PathBuf {
        self.path.join("hbot.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 造一个「只升到 v1」的库，并塞进跨表数据。
///
/// `with_conflict` 为真时预先加上 `bots.status`：0002 的第一句 `ALTER TABLE bots ADD COLUMN
/// status …` 会因此失败（`duplicate column name`），从而把「迁移失败」注入到真实代码路径里。
fn seed_v1(db: &Path, with_conflict: bool) {
    let conn = Connection::open(db).expect("create v1 db");
    conn.execute_batch(INIT_SQL).expect("apply 0001");
    conn.execute_batch(CREATE_SCHEMA_MIGRATIONS)
        .expect("create schema_migrations");
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (1, '0001_init', 1000)",
        [],
    )
    .expect("record v1");
    conn.pragma_update(None, "user_version", 1)
        .expect("user_version");

    conn.execute(
        "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, 1000)",
        params![PROJECT_ID, PROJECT_NAME, "/tmp/it-project"],
    )
    .expect("insert project");
    conn.execute(
        "INSERT INTO bots (id, project_id, name, platform, config_json, enabled, created_at)
         VALUES (?1, ?2, ?3, 'onebot11', '{}', 1, 1000)",
        params![BOT_ID, PROJECT_ID, BOT_NAME],
    )
    .expect("insert bot");
    conn.execute(
        "INSERT INTO logs (bot_id, level, line, at) VALUES (?1, 'info', ?2, 1000)",
        params![BOT_ID, LOG_LINE],
    )
    .expect("insert log");

    if with_conflict {
        // 0002 的第一条语句：把它提前做掉，好让迁移在第二遍执行时撞车。
        conn.execute_batch("ALTER TABLE bots ADD COLUMN status TEXT NOT NULL DEFAULT 'stopped';")
            .expect("inject conflicting column");
    }
    drop(conn);
}

/// 表上所有列名（`PRAGMA table_info`）。
fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info");
    rows.map(|row| row.expect("column name")).collect()
}

/// 库里所有索引名。
fn indexes(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .expect("prepare index list");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query index list");
    rows.map(|row| row.expect("index name")).collect()
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version")
}

fn scalar_i64(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0))
        .expect("scalar query")
}

fn scalar_text(conn: &Connection, sql: &str) -> String {
    conn.query_row(sql, [], |row| row.get(0))
        .expect("scalar query")
}

/// `PRAGMA foreign_key_check` 返回的违规表名（空 = 自洽）。
fn foreign_key_violations(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_check")
        .expect("prepare foreign_key_check");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query foreign_key_check");
    rows.map(|row| row.expect("table name")).collect()
}

/// 核心用例：0002 执行到一半失败 → 库仍可打开、数据不丢、失败那条整体没落库。
#[test]
fn failed_migration_keeps_the_database_openable_and_the_data_intact() {
    let temp = TempDir::new("rollback");
    let db = temp.db();
    seed_v1(&db, true);

    // 1) 迁移失败必须被报出来，且指明是第 2 条、原因是 SQLite 的 duplicate column。
    let error = Store::open(&db).expect_err("0002 必须失败（bots.status 已存在）");
    match error {
        StoreError::Migration { version, source } => {
            assert_eq!(version, 2, "失败的必须是 0002");
            let message = source.to_string();
            assert!(
                message.contains("duplicate column name"),
                "失败原因应来自真实 SQLite 报错，实际：{message}"
            );
        }
        other => panic!("期望 StoreError::Migration，实际：{other:?}"),
    }

    // 2) 原库仍可打开（用它自己的 raw 连接，不经过 Store）。
    let conn = Connection::open(&db).expect("迁移失败后库必须仍能打开");

    // 3) 数据一条不少。
    assert_eq!(
        scalar_text(
            &conn,
            "SELECT name FROM projects WHERE id = '11111111-1111-4111-8111-111111111111'"
        ),
        PROJECT_NAME
    );
    assert_eq!(
        scalar_text(
            &conn,
            "SELECT name FROM bots WHERE id = '22222222-2222-4222-8222-222222222222'"
        ),
        BOT_NAME
    );
    assert_eq!(
        scalar_text(&conn, "SELECT line FROM logs ORDER BY id DESC LIMIT 1"),
        LOG_LINE
    );
    assert_eq!(scalar_i64(&conn, "SELECT COUNT(*) FROM logs"), 1);

    // 4) 迁移水位没被推进，schema_migrations 里还是只有 v1。
    assert_eq!(user_version(&conn), 1, "失败的迁移不许推进 user_version");
    assert_eq!(
        scalar_i64(&conn, "SELECT COUNT(*) FROM schema_migrations"),
        1
    );
    assert_eq!(
        scalar_text(
            &conn,
            "SELECT name FROM schema_migrations WHERE version = 1"
        ),
        "0001_init"
    );

    // 5) 失败那条的**其它**语句一句都没落库 —— 这才是「回滚」而不是「执行了一部分」。
    let bot_columns = columns(&conn, "bots");
    assert!(
        !bot_columns.contains(&"updated_at".to_string()),
        "0002 的第二句（bots.updated_at）不该落库，实际列：{bot_columns:?}"
    );
    let inbound_columns = columns(&conn, "inbound");
    assert!(
        !inbound_columns.contains(&"sender_id".to_string()),
        "0002 的 inbound.sender_id 不该落库，实际列：{inbound_columns:?}"
    );
    let outbound_columns = columns(&conn, "outbound");
    assert!(
        !outbound_columns.contains(&"target_json".to_string()),
        "0002 的 outbound.target_json 不该落库，实际列：{outbound_columns:?}"
    );
    assert!(
        !outbound_columns.contains(&"reply_to".to_string()),
        "0002 的 outbound.reply_to 不该落库，实际列：{outbound_columns:?}"
    );
    let index_names = indexes(&conn);
    assert!(
        !index_names.contains(&"idx_outbound_pending".to_string()),
        "0002 的索引不该落库，实际索引：{index_names:?}"
    );
    assert!(
        !index_names.contains(&"idx_inbound_unprocessed".to_string()),
        "0002 的索引不该落库，实际索引：{index_names:?}"
    );

    // 6) 回滚后的库自洽：外键没有悬空。`foreign_key_check` 返回行 = 有问题。
    let violations = foreign_key_violations(&conn);
    assert!(
        violations.is_empty(),
        "回滚后不应有外键悬空：{violations:?}"
    );

    // 7) 失败是可重复的：不是「第一次半提交、第二次换个错」。
    drop(conn);
    let second = Store::open(&db).expect_err("第二次启动也必须以同样的原因失败");
    match second {
        StoreError::Migration { version, .. } => assert_eq!(version, 2),
        other => panic!("期望同样的 StoreError::Migration，实际：{other:?}"),
    }
    // 且第二次失败没有进一步破坏库。
    let conn = Connection::open(&db).expect("仍可打开");
    assert_eq!(scalar_i64(&conn, "SELECT COUNT(*) FROM logs"), 1);
    assert_eq!(user_version(&conn), 1);
}

/// 对照组：同样的 fixture、不注入冲突 → 0002 能正常升上去且数据不丢。
///
/// 这条存在的意义是证明上面那条失败**确实来自注入的冲突**，而不是 fixture 本身不合法
/// （一个「怎么都开不了」的库也能让上面所有断言通过）。
#[test]
fn the_same_fixture_upgrades_cleanly_without_the_injected_conflict() {
    let temp = TempDir::new("control");
    let db = temp.db();
    seed_v1(&db, false);

    let store = Store::open(&db).expect("没有冲突时必须能开到最新版");
    assert_eq!(store.schema_version().expect("schema_version"), 2);
    drop(store);

    let conn = Connection::open(&db).expect("open");
    assert_eq!(user_version(&conn), 2);
    assert_eq!(
        scalar_i64(&conn, "SELECT COUNT(*) FROM schema_migrations"),
        2
    );
    // 数据在这次成功迁移里也没丢。
    assert_eq!(
        scalar_text(
            &conn,
            "SELECT name FROM projects WHERE id = '11111111-1111-4111-8111-111111111111'"
        ),
        PROJECT_NAME
    );
    assert_eq!(
        scalar_text(&conn, "SELECT line FROM logs ORDER BY id DESC LIMIT 1"),
        LOG_LINE
    );
    // 0002 的改动这次全部到位（与上面那条的反向断言互为镜像）。
    assert!(columns(&conn, "bots").contains(&"updated_at".to_string()));
    assert!(columns(&conn, "inbound").contains(&"sender_id".to_string()));
    assert!(columns(&conn, "outbound").contains(&"target_json".to_string()));
    assert!(columns(&conn, "outbound").contains(&"reply_to".to_string()));
    assert!(indexes(&conn).contains(&"idx_outbound_pending".to_string()));
}
