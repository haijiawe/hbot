//! 存储层行为测试：PRAGMA 契约、迁移幂等、去重、并发写、外键、出站/日志链路。
//!
//! ID 全部是固定 UUID（`Uuid::from_u128(小整数)`），断言里能直接对上种子行；
//! 字符串 id 会让"同一个 UUID 的两种表示"混进测试，所以不再使用。

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::params;

use crate::error::StoreError;
use crate::migrations;
use crate::models::{
    BOT_STATUS_RUNNING, BOT_STATUS_STOPPED, Bot, Conversation, NewAgentLog, NewLog,
    OUTBOUND_STATE_FAILED, OUTBOUND_STATE_SENT, is_valid_bot_status,
};
use crate::test_support::{
    TempDir, bot_id, bot_id_at, conversation_id, fixed_uuid, inbound_id, new_inbound,
    new_inbound_at, new_log, new_outbound, new_outbound_at, open_store, outbound_id,
    outbound_target, project_id, project_id_at, raw, seed,
};
use crate::{BUSY_TIMEOUT_MS, Store};

/// PRAGMA 契约：WAL、busy_timeout=5000、schema 版本到最新。
#[test]
fn pragmas_are_applied_on_open() {
    let dir = TempDir::new("pragmas");
    let store = open_store(&dir);

    assert_eq!(store.journal_mode().expect("journal_mode"), "wal");
    assert_eq!(
        store.busy_timeout_ms().expect("busy_timeout"),
        BUSY_TIMEOUT_MS,
        "busy_timeout 必须是 5s"
    );
    assert_eq!(BUSY_TIMEOUT_MS, 5_000);
    assert_eq!(
        store.schema_version().expect("schema_version"),
        migrations::latest_version()
    );
}

/// 重开同一个文件：WAL 是库属性，必须保持；schema 版本不回退。
#[test]
fn reopening_keeps_wal_and_schema_version() {
    let dir = TempDir::new("reopen");
    {
        let store = Store::open(dir.db_path()).expect("open store");
        assert_eq!(store.journal_mode().expect("journal_mode"), "wal");
    }

    let reopened = Store::open(dir.db_path()).expect("reopen store");
    assert_eq!(reopened.journal_mode().expect("journal_mode"), "wal");
    assert_eq!(
        reopened.schema_version().expect("schema_version"),
        migrations::latest_version()
    );
}

/// 迁移重复执行安全：多次打开只留一条记录，user_version 单调到最新。
#[test]
fn migrations_are_idempotent_across_restarts() {
    let dir = TempDir::new("migrations");
    for _ in 0..3 {
        let store = Store::open(dir.db_path()).expect("open store");
        assert_eq!(
            store.schema_version().expect("schema_version"),
            migrations::latest_version()
        );
    }

    let raw = raw(&dir);
    let applied: i64 = raw
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count schema_migrations");
    assert_eq!(applied, i64::from(migrations::latest_version()));

    let user_version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(user_version, i64::from(migrations::latest_version()));
}

/// 启动路径只允许开 1 条写连接 + [`crate::db`] 的 `READ_CONNECTIONS` 条只读连接。
///
/// 这是**结构不变量**，不是时长断言：多开一条连接只会让 ready 晚几毫秒，功能测试不会红，
/// 所以 ADR-0002 §4 的启动预算只能靠这种断言兜住。计数记在测试线程本地（见
/// `crate::db::connection_opens_on_this_thread`），并行测试互不污染。
#[test]
fn startup_opens_only_the_writer_and_read_connections() {
    use crate::db::{
        READ_CONNECTIONS, connection_opens_on_this_thread, reset_connection_opens_on_this_thread,
    };

    let dir = TempDir::new("startup-conns");
    reset_connection_opens_on_this_thread();
    let store = Store::open(dir.db_path()).expect("open store");

    assert_eq!(
        connection_opens_on_this_thread(),
        1 + READ_CONNECTIONS,
        "Store::open 只该开 1 条写连接 + READ_CONNECTIONS 条只读连接；多开的就是启动路径上的浪费"
    );
    drop(store);
}

/// 读出 `schema_migrations` 的 (version, name, applied_at)，升序。
fn schema_migrations_rows(dir: &TempDir) -> Vec<(i64, String, i64)> {
    let conn = raw(dir);
    let mut statement = conn
        .prepare("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
        .expect("prepare schema_migrations");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query schema_migrations");
    rows.map(|row| row.expect("schema_migrations row"))
        .collect()
}

/// 第二次打开**不重跑**迁移：`schema_migrations` 必须逐行不变。
///
/// 比 `migrations_are_idempotent_across_restarts` 更强：那条只看行数，而"每次启动重放一遍
/// 迁移、用 `INSERT OR REPLACE` 覆盖记账"同样能保持行数。这里先把 `applied_at` 改成哨兵值
/// （42，远小于任何真实的 `now_unix_ms()`），重开一次后再比：迁移只要重放，时间戳就会变。
#[test]
fn reopening_does_not_rerun_migrations() {
    const SENTINEL: i64 = 42;

    let dir = TempDir::new("no-rerun");
    drop(Store::open(dir.db_path()).expect("first open"));

    raw(&dir)
        .execute("UPDATE schema_migrations SET applied_at = ?1", [SENTINEL])
        .expect("stamp applied_at");
    let before = schema_migrations_rows(&dir);
    assert_eq!(
        before.len(),
        usize::try_from(migrations::latest_version()).expect("latest version fits usize"),
        "首次打开应当把每条迁移各记一行"
    );
    assert!(
        before
            .iter()
            .all(|(_, _, applied_at)| *applied_at == SENTINEL),
        "哨兵值没写进去，后面的重放断言就失去意义"
    );

    drop(Store::open(dir.db_path()).expect("second open"));

    assert_eq!(
        schema_migrations_rows(&dir),
        before,
        "第二次打开改了 schema_migrations：迁移被重放了（启动预算会随库增长变差）"
    );
}

/// 启动路径不许出现全库扫描/整理语句（`integrity_check` / `quick_check` / `VACUUM`）。
///
/// 这三样都是 O(库大小) 的，会随用户数据增长拖慢 ready——ADR-0002 §4 那条预算最怕的回归。
/// 崩溃安全由 WAL 自己保证，启动时不需要全库校验。真要在启动路径里加，请先改这个测试，
/// 并在 `docs/spikes/S11-headless-startup.md` 写清"什么情况下会漏检"。
#[test]
fn startup_path_has_no_full_database_scan() {
    for (name, source) in [
        ("db.rs", include_str!("db.rs")),
        ("migrations.rs", include_str!("migrations.rs")),
    ] {
        let lowered = source.to_ascii_lowercase();
        for forbidden in ["integrity_check", "quick_check", "vacuum"] {
            assert!(
                !lowered.contains(forbidden),
                "{name} 的启动路径里出现了 {forbidden}（全库扫描/整理），会随库增长拖慢 ready"
            );
        }
    }
}

/// 同一 `(bot_id, external_id)` 插两次：第一次 `Some`，第二次 `None`，库里只有一行。
#[test]
fn duplicate_inbound_is_inserted_only_once() {
    let dir = TempDir::new("dedupe");
    let store = open_store(&dir);

    let first = store
        .insert_inbound_if_new(new_inbound(101, "platform-msg-1"))
        .expect("first insert");
    assert_eq!(first, Some(inbound_id(101)), "首次投递必须返回 id");

    // 换了本地 id（另一个 UUID）但外部键相同：仍视为重复，返回 None 且不写入。
    let second = store
        .insert_inbound_if_new(new_inbound(102, "platform-msg-1"))
        .expect("second insert");
    assert_eq!(second, None, "重复投递必须返回 None");

    // 完全相同的行再插一次同样安全。
    let third = store
        .insert_inbound_if_new(new_inbound(101, "platform-msg-1"))
        .expect("third insert");
    assert_eq!(third, None);

    drop(store);
    let raw = raw(&dir);
    let count: i64 = raw
        .query_row("SELECT COUNT(*) FROM inbound", [], |row| row.get(0))
        .expect("count inbound");
    assert_eq!(count, 1, "去重后只能有一行");
}

/// 本地主键撞车不能被当成“重复投递”静默丢弃。
#[test]
fn inbound_id_collision_is_an_error() {
    let dir = TempDir::new("id-collision");
    let store = open_store(&dir);
    store
        .insert_inbound_if_new(new_inbound(201, "ext-1"))
        .expect("seed inbound")
        .expect("seed inserted");

    let error = store
        .insert_inbound_if_new(new_inbound(201, "ext-2"))
        .expect_err("same id with a new external key must fail");
    assert!(
        matches!(
            error,
            StoreError::IdCollision {
                table: "inbound",
                ..
            }
        ),
        "unexpected error: {error}"
    );
}

/// `mark_inbound_processed` 落 `processed_at`，未知 id 返回 false。
#[test]
fn mark_inbound_processed_updates_the_row() {
    let dir = TempDir::new("processed");
    let store = open_store(&dir);
    let id = store
        .insert_inbound_if_new(new_inbound(301, "ext-1"))
        .expect("insert")
        .expect("first insert");

    assert!(store.mark_inbound_processed(&id, 1_234).expect("mark"));
    assert!(
        !store
            .mark_inbound_processed(&inbound_id(999), 1)
            .expect("mark missing")
    );

    drop(store);
    let raw = raw(&dir);
    let processed_at: Option<i64> = raw
        .query_row(
            "SELECT processed_at FROM inbound WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        )
        .expect("read processed_at");
    assert_eq!(processed_at, Some(1_234));
}

/// 出站生命周期：pending → attempts 累加 → sent + sent_at。
#[test]
fn outbound_lifecycle_records_attempts_and_sent_time() {
    let dir = TempDir::new("outbound");
    let store = open_store(&dir);

    let id = store
        .enqueue_outbound(new_outbound(401))
        .expect("enqueue outbound");
    assert_eq!(id, outbound_id(401));
    assert_eq!(
        store.mark_outbound_attempt(&id).expect("attempt 1"),
        Some(1)
    );
    assert_eq!(
        store.mark_outbound_attempt(&id).expect("attempt 2"),
        Some(2)
    );
    assert!(store.mark_outbound_sent(&id, 999).expect("mark sent"));

    let missing = outbound_id(999);
    assert!(!store.mark_outbound_sent(&missing, 1).expect("missing sent"));
    assert_eq!(
        store
            .mark_outbound_attempt(&missing)
            .expect("missing attempt"),
        None
    );

    drop(store);
    let raw = raw(&dir);
    let (state, attempts, sent_at): (String, i64, Option<i64>) = raw
        .query_row(
            "SELECT state, attempts, sent_at FROM outbound WHERE id = ?1",
            params![id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read outbound row");
    assert_eq!(state, OUTBOUND_STATE_SENT);
    assert_eq!(attempts, 2);
    assert_eq!(sent_at, Some(999));
}

/// 日志按 `id` 单调追加，`at` 原样落库。
#[test]
fn logs_are_appended_in_order() {
    let dir = TempDir::new("logs");
    let store = open_store(&dir);

    for entry in [
        ("info", "first", 10_i64),
        ("warn", "second", 20),
        ("error", "third", 30),
    ] {
        store
            .append_log(NewLog {
                bot_id: bot_id(),
                level: entry.0.to_owned(),
                line: entry.1.to_owned(),
                at: entry.2,
            })
            .expect("append log");
    }

    drop(store);
    let raw = raw(&dir);
    let mut statement = raw
        .prepare("SELECT level, line, at FROM logs ORDER BY id")
        .expect("prepare logs query");
    let rows: Vec<(String, String, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query logs")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect logs");
    assert_eq!(
        rows,
        vec![
            ("info".to_owned(), "first".to_owned(), 10),
            ("warn".to_owned(), "second".to_owned(), 20),
            ("error".to_owned(), "third".to_owned(), 30),
        ]
    );
}

/// bot 读写：`get_bot` 还原 enabled/config 与强类型 ID，`list_bots` 覆盖全部。
#[test]
fn bots_round_trip_and_list() {
    let dir = TempDir::new("bots");
    let store = open_store(&dir);

    let bot = store
        .get_bot(&bot_id())
        .expect("get bot")
        .expect("bot exists");
    assert_eq!(bot.id, bot_id());
    assert_eq!(bot.project_id, project_id());
    assert_eq!(bot.name, "demo-bot");
    assert_eq!(bot.platform, "onebot");
    assert!(bot.enabled);
    assert_eq!(bot.config_json, "{\"token\":\"redacted\"}");

    let bots = store.list_bots().expect("list bots");
    assert_eq!(bots.len(), 1);
    assert_eq!(bots[0].id, bot_id());

    assert!(
        store
            .get_bot(&bot_id_at(999))
            .expect("get missing bot")
            .is_none()
    );
}

/// 外键真的生效：指向不存在 bot 的入站消息必须被拒绝，而不是落一行孤儿。
#[test]
fn foreign_key_violation_is_rejected() {
    let dir = TempDir::new("fk");
    let store = open_store(&dir);

    let mut message = new_inbound(501, "ext-fk");
    message.bot_id = bot_id_at(999);
    let error = store
        .insert_inbound_if_new(message)
        .expect_err("foreign key violation must fail");
    assert!(
        error.to_string().to_lowercase().contains("foreign key"),
        "unexpected error: {error}"
    );
}

/// 删除项目级联清掉 bot / 会话 / 入站 / 日志（`ON DELETE CASCADE` + foreign_keys=ON）。
#[test]
fn deleting_a_project_cascades_to_children() {
    let dir = TempDir::new("cascade");
    let store = open_store(&dir);
    store
        .insert_inbound_if_new(new_inbound(601, "ext-1"))
        .expect("insert inbound");
    store
        .append_log(NewLog {
            bot_id: bot_id(),
            level: "info".to_owned(),
            line: "hello".to_owned(),
            at: 10,
        })
        .expect("append log");
    drop(store);

    let raw = raw(&dir);
    raw.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         DELETE FROM projects WHERE id = '{}';",
        project_id()
    ))
    .expect("delete project");

    for table in ["bots", "conversations", "inbound", "outbound", "logs"] {
        let count: i64 = raw
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count table");
        assert_eq!(count, 0, "{table} must be cascade-deleted");
    }
}

/// 8 线程并发写：不 panic、不锁死、全部落库，且写后读一致。
#[test]
fn eight_threads_write_concurrently() {
    let dir = TempDir::new("concurrency");
    let store = Arc::new(open_store(&dir));

    let mut handles = Vec::new();
    for thread_index in 0..8_u32 {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            let mut inserted = 0_u32;
            for message_index in 0..25_u32 {
                // 固定 UUID：线程号 × 100 + 序号，保证 200 个 id 互不相同且可复现。
                let id = 1_000 + u128::from(thread_index) * 100 + u128::from(message_index);
                let external_id = format!("ext-{thread_index}-{message_index}");
                match store.insert_inbound_if_new(new_inbound(id, &external_id)) {
                    Ok(Some(_)) => inserted += 1,
                    Ok(None) => panic!("unexpected duplicate for {external_id}"),
                    Err(error) => panic!("insert failed: {error}"),
                }
            }
            inserted
        }));
    }

    let mut total = 0_u32;
    for handle in handles {
        total += handle.join().expect("worker thread must not panic");
    }
    assert_eq!(total, 200, "8 线程 × 25 条必须全部插入成功");

    drop(store);
    let raw = raw(&dir);
    let count: i64 = raw
        .query_row("SELECT COUNT(*) FROM inbound", [], |row| row.get(0))
        .expect("count inbound");
    assert_eq!(count, 200);
}

/// 同一外部消息被多线程同时投递：只有一条 `Some`，其余 `None`，库里只有一行。
#[test]
fn concurrent_duplicates_insert_exactly_once() {
    let dir = TempDir::new("dedupe-race");
    let store = Arc::new(open_store(&dir));

    let mut handles = Vec::new();
    for thread_index in 0..8_u32 {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            // 每个线程用不同的行 id，但外部键相同——正是平台重复投递的形状。
            let id = 2_000 + u128::from(thread_index);
            store
                .insert_inbound_if_new(new_inbound(id, "ext-race"))
                .expect("insert must not fail")
                .is_some()
        }));
    }

    let winners = handles
        .into_iter()
        .map(|handle| handle.join().expect("worker must not panic"))
        .filter(|inserted| *inserted)
        .count();
    assert_eq!(winners, 1, "只允许一个线程拿到 Some");

    drop(store);
    let raw = raw(&dir);
    let count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM inbound WHERE external_id = 'ext-race'",
            [],
            |row| row.get(0),
        )
        .expect("count inbound");
    assert_eq!(count, 1);
}

/// busy_timeout 生效：写锁被别人占用时等锁而不是立刻 `SQLITE_BUSY`。
#[test]
fn busy_timeout_waits_for_a_held_write_lock() {
    let dir = TempDir::new("busy");
    let store = Arc::new(open_store(&dir));

    // 另一条连接拿住写事务，模拟“另一个进程正在写”。
    let locker = raw(&dir);
    locker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("acquire write lock");

    let started = Instant::now();
    let writer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.insert_inbound_if_new(new_inbound(701, "ext-busy")))
    };

    std::thread::sleep(Duration::from_millis(300));
    locker.execute_batch("COMMIT;").expect("release write lock");

    let inserted = writer
        .join()
        .expect("writer thread must not panic")
        .expect("insert must wait for the lock and then succeed");
    let waited = started.elapsed();

    assert!(
        inserted.is_some(),
        "insert should land once the lock is released"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "write should have blocked on the lock (waited {waited:?})"
    );
}

/// 种子数据本身可读：入站行指向种子会话，且落库形状是 UUID 字符串。
#[test]
fn seeded_conversation_belongs_to_seeded_bot() {
    let dir = TempDir::new("seed");
    let store = open_store(&dir);
    store
        .insert_inbound_if_new(new_inbound(801, "ext-1"))
        .expect("insert inbound with seeded conversation")
        .expect("first insert");

    drop(store);
    let raw = raw(&dir);
    let stored: String = raw
        .query_row(
            "SELECT conversation_id FROM inbound WHERE id = ?1",
            params![inbound_id(801).to_string()],
            |row| row.get(0),
        )
        .expect("read conversation_id");
    assert_eq!(stored, conversation_id().to_string());
}

/// 重导出的 ID 与 `hbot_core::ids` 是**同一个类型**——这是编译期证据，不是运行时比对：
/// 下面几个函数只在两个路径指向同一类型时才编译通过（`AGENTS.md` §0.4 只有一份契约）。
#[test]
fn store_reexports_core_ids_verbatim() {
    fn bot(id: hbot_core::ids::BotId) -> crate::BotId {
        id
    }
    fn project(id: hbot_core::ids::ProjectId) -> crate::ProjectId {
        id
    }
    fn conversation(id: hbot_core::ids::ConversationId) -> crate::ConversationId {
        id
    }
    fn inbound(id: hbot_core::ids::InboundId) -> crate::InboundId {
        id
    }
    fn outbound(id: hbot_core::ids::OutboundId) -> crate::OutboundId {
        id
    }

    assert_eq!(bot(bot_id()), bot_id());
    assert_eq!(project(project_id()), project_id());
    assert_eq!(conversation(conversation_id()), conversation_id());
    assert_eq!(inbound(inbound_id(1)), inbound_id(1));
    assert_eq!(outbound(outbound_id(1)), outbound_id(1));
}

/// `seed` 可在已有库上重复调用吗？——不能（UNIQUE path），这里是显式约束的回归测试。
#[test]
fn seeding_twice_is_rejected() {
    let dir = TempDir::new("seed-twice");
    let store = Store::open(dir.db_path()).expect("open store");
    seed(&store);

    let error = store
        .insert_project(&crate::models::Project {
            id: project_id_at(2),
            name: "second".to_owned(),
            path: "/tmp/hbot-demo".to_owned(),
            created_at: 9,
        })
        .expect_err("duplicate project path must fail");
    assert!(
        error.to_string().to_lowercase().contains("unique"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// 读取 API（续发 / 补偿 / 状态面板）：pending 队列、未处理入站、会话、状态、日志。
// ---------------------------------------------------------------------------

/// 待发送队列按**产生顺序**（`created_at`, `rowid`）返回，且 `limit` 生效；路由信息一并带出。
///
/// 这条测试原先断言“按 `id` 升序”（并且刻意乱序入队来证明是按 id 而不是入队顺序）。
/// 那是**旧行为，且是缺陷**：`id` 是 UUID v4，字典序与时间无关，重启续发会把同一会话的
/// 回复打乱成随机顺序，违反 `AGENTS.md` §4.1 “回复按产生顺序发出”。现在按
/// `(created_at, rowid)` 排，本用例的三行 `created_at` 相同，因此结果就是插入顺序。
#[test]
fn pending_outbound_is_listed_in_production_order_and_limited() {
    let dir = TempDir::new("pending-order");
    let store = open_store(&dir);

    // 故意乱序入队：id 最小的 11 反而最后写。现在顺序跟随插入顺序（12, 11, 13）。
    store
        .enqueue_outbound_with_target(
            new_outbound(12),
            outbound_target("{\"group\":\"42\"}", Some("ext-msg-7")),
        )
        .expect("enqueue outbound 12");
    store
        .enqueue_outbound(new_outbound(11))
        .expect("enqueue outbound 11");
    store
        .enqueue_outbound(new_outbound(13))
        .expect("enqueue outbound 13");

    let all = store.list_pending_outbound(10).expect("list pending");
    let ids: Vec<_> = all.iter().map(|row| row.id).collect();
    assert_eq!(
        ids,
        vec![outbound_id(12), outbound_id(11), outbound_id(13)],
        "同一毫秒产生的行必须按插入顺序返回，不能按 UUID 字典序"
    );

    let routed = &all[0];
    assert_eq!(routed.target_json, "{\"group\":\"42\"}");
    assert_eq!(routed.reply_to.as_deref(), Some("ext-msg-7"));
    assert_eq!(routed.attempts, 0, "新入队行 attempts 从 0 开始");
    assert_eq!(routed.text, "reply");
    assert_eq!(routed.bot_id, bot_id());
    assert_eq!(routed.conversation_id, conversation_id());

    let plain = &all[1];
    assert_eq!(plain.target_json, "{}", "未知目标落库为 '{{}}'");
    assert!(plain.reply_to.is_none(), "主动消息不带 reply_to");

    let limited = store.list_pending_outbound(2).expect("list pending limit");
    let limited_ids: Vec<_> = limited.iter().map(|row| row.id).collect();
    assert_eq!(limited_ids, vec![outbound_id(12), outbound_id(11)]);
}

/// 恢复顺序：`created_at` 相同（同一毫秒）时按插入顺序，不同时按时间升序。
///
/// 这是 `D4` 的核心断言：UUID 主键 + 同一毫秒的连续回复，只有 `rowid` 能保住生产顺序。
#[test]
fn pending_outbound_recovery_keeps_production_order_for_equal_timestamps() {
    let dir = TempDir::new("pending-fifo");
    let store = open_store(&dir);

    // 四条回复：前两条同一毫秒、后两条另一个毫秒；id 故意与时间顺序相反。
    let mut ids = Vec::new();
    for (index, (id, created_at)) in [(40_u128, 5_000_i64), (41, 5_000), (42, 6_000), (43, 6_000)]
        .into_iter()
        .enumerate()
    {
        store
            .enqueue_outbound(new_outbound_at(id, &format!("reply-{index}"), created_at))
            .expect("enqueue reply");
        ids.push(outbound_id(id));
    }

    let rows = store.list_pending_outbound(10).expect("list pending");
    let recovered: Vec<_> = rows.iter().map(|row| row.id).collect();
    assert_eq!(
        recovered, ids,
        "恢复必须还原产生顺序：先 created_at，再插入顺序"
    );
    let texts: Vec<_> = rows.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["reply-0", "reply-1", "reply-2", "reply-3"],
        "正文顺序与 id 顺序一致（同一毫秒内靠 rowid 保持）"
    );

    // 按 bot 过滤的同一视图也必须保持同一顺序（`bot_start` 续发走这一条）。
    let for_bot = store
        .list_pending_outbound_for_bot(&bot_id(), 10)
        .expect("list pending for bot");
    let recovered_for_bot: Vec<_> = for_bot.iter().map(|row| row.id).collect();
    assert_eq!(recovered_for_bot, ids);
}

/// 死信（`failed`）不再出现在待发送队列里，但行保留，`get_outbound` 仍读得到最终 attempts。
#[test]
fn failed_outbound_is_excluded_from_pending() {
    let dir = TempDir::new("pending-failed");
    let store = open_store(&dir);

    store
        .enqueue_outbound(new_outbound(21))
        .expect("enqueue 21");
    store
        .enqueue_outbound(new_outbound(22))
        .expect("enqueue 22");
    store
        .mark_outbound_sent(&outbound_id(21), 5_000)
        .expect("mark sent");
    assert!(
        store
            .mark_outbound_failed(&outbound_id(22), 3)
            .expect("mark failed"),
        "mark_outbound_failed must hit the row"
    );

    assert!(
        store
            .list_pending_outbound(10)
            .expect("list pending")
            .is_empty(),
        "已发送与死信都不算 pending"
    );
    assert_eq!(store.count_pending_outbound().expect("count"), 0);

    let dead = store
        .get_outbound(&outbound_id(22))
        .expect("get outbound")
        .expect("failed row must be kept for troubleshooting");
    assert_eq!(dead.attempts, 3, "死信保留最终尝试次数");
    assert_eq!(dead.text, "reply");

    assert!(
        !store
            .mark_outbound_failed(&outbound_id(99), 1)
            .expect("missing row"),
        "不存在的行返回 false，不是错误"
    );
    assert_eq!(
        store.get_outbound(&outbound_id(99)).expect("get missing"),
        None
    );
}

/// 未处理入站队列：已处理的被过滤，剩余按 `(received_at, id)` 升序；`sender_id` 一并读回。
#[test]
fn unprocessed_inbound_skips_processed_and_orders_by_received_at() {
    let dir = TempDir::new("inbound-unprocessed");
    let store = open_store(&dir);

    // 乱序写入 + 一个已被处理的行。
    store
        .insert_inbound_with_sender(new_inbound_at(31, "ext-31", 300), "user-a".to_owned())
        .expect("insert 31");
    store
        .insert_inbound_if_new(new_inbound_at(32, "ext-32", 100))
        .expect("insert 32");
    store
        .insert_inbound_if_new(new_inbound_at(33, "ext-33", 200))
        .expect("insert 33");
    store
        .mark_inbound_processed(&inbound_id(33), 400)
        .expect("mark processed");

    let rows = store
        .list_unprocessed_inbound(10)
        .expect("list unprocessed");
    let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    assert_eq!(
        ids,
        vec![inbound_id(32), inbound_id(31)],
        "按 received_at 升序，已处理的行不出现"
    );
    assert_eq!(rows[1].sender_id, "user-a");
    assert_eq!(rows[0].sender_id, "", "未提供发送者时落库为空串");
    assert_eq!(rows[0].external_id, "ext-32");
    assert_eq!(rows[0].payload_json, "{\"text\":\"hello\"}");

    let limited = store.list_unprocessed_inbound(1).expect("limit");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, inbound_id(32));
}

/// 会话可分别按主键与幂等键 `(bot_id, external_id)` 读出；不存在时是 `Ok(None)`。
#[test]
fn conversations_are_readable_by_id_and_external_key() {
    let dir = TempDir::new("conversation-read");
    let store = open_store(&dir);

    let by_id = store
        .get_conversation(&conversation_id())
        .expect("get by id")
        .expect("seeded conversation");
    assert_eq!(by_id.external_id, "group-42");
    assert_eq!(by_id.kind, "group");

    let by_external = store
        .get_conversation_by_external(&bot_id(), "group-42")
        .expect("get by external")
        .expect("seeded conversation");
    assert_eq!(by_external.id, conversation_id());

    assert_eq!(
        store
            .get_conversation_by_external(&bot_id(), "no-such-group")
            .expect("miss by external"),
        None
    );
    assert_eq!(
        store
            .get_conversation(&crate::ConversationId::from_uuid(
                crate::test_support::fixed_uuid(999)
            ))
            .expect("miss by id"),
        None
    );
}

/// `list_conversations_for_bot` 只返回该 bot 的行、顺序固定为 `(kind, external_id)`，
/// 且重复调用逐行相同（UI 每次刷新看到同一个顺序，测试不会 flaky）。
#[test]
fn conversations_are_listed_per_bot_in_a_deterministic_order() {
    let dir = TempDir::new("conversation-list");
    let store = open_store(&dir);

    // 同一个项目里的另一个 bot：它的会话不能混进 `bot_id()` 的结果。
    let other_bot = bot_id_at(20);
    store
        .insert_bot(&Bot {
            id: other_bot,
            project_id: project_id(),
            name: "other-bot".to_owned(),
            platform: "local".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 4,
        })
        .expect("insert other bot");
    store
        .insert_conversation(&Conversation {
            id: crate::ConversationId::from_uuid(fixed_uuid(40)),
            bot_id: other_bot,
            external_id: "chat-0".to_owned(),
            kind: "private".to_owned(),
            created_at: 5,
        })
        .expect("insert other bot's conversation");

    // 刻意按"乱序 + created_at 递增"插入：证明返回顺序既不是插入序、也不是 created_at 序。
    // 期望顺序只由 (kind, external_id) 决定：channel-1 < group-42 < chat-a < chat-b。
    for (offset, external_id, kind) in [
        (41u128, "chat-b", "private"),
        (42, "channel-1", "channel"),
        (43, "chat-a", "private"),
    ] {
        store
            .insert_conversation(&Conversation {
                id: crate::ConversationId::from_uuid(fixed_uuid(offset)),
                bot_id: bot_id(),
                external_id: external_id.to_owned(),
                kind: kind.to_owned(),
                created_at: offset as i64,
            })
            .expect("insert conversation for seeded bot");
    }

    let listed = store
        .list_conversations_for_bot(&bot_id())
        .expect("list conversations");
    let pairs: Vec<(&str, &str)> = listed
        .iter()
        .map(|row| (row.external_id.as_str(), row.kind.as_str()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("channel-1", "channel"),
            ("group-42", "group"),
            ("chat-a", "private"),
            ("chat-b", "private"),
        ],
        "顺序必须是 (kind, external_id)，且不掺别的 bot 的行"
    );

    // 确定性：两次调用逐行相同（含顺序）。
    let again = store
        .list_conversations_for_bot(&bot_id())
        .expect("list conversations again");
    assert_eq!(listed, again, "同一份数据两次查询必须一致");

    // 不存在的 bot 返回空表，不是错误：存在性判断是业务层（`bot_conversations`）的事。
    assert!(
        store
            .list_conversations_for_bot(&bot_id_at(999))
            .expect("list for unknown bot")
            .is_empty()
    );
}

/// bot 状态：新库默认 `stopped`；合法值可写可读；非法值返回 `Invalid` 且不改库。
#[test]
fn update_bot_status_validates_and_round_trips() {
    let dir = TempDir::new("bot-status");
    let store = open_store(&dir);

    let initial = store
        .get_bot_status(&bot_id())
        .expect("get status")
        .expect("seeded bot");
    assert_eq!(initial.status, BOT_STATUS_STOPPED, "0002 列默认 stopped");
    assert_eq!(initial.updated_at, 0, "从未变更过");

    assert!(
        store
            .update_bot_status(&bot_id(), BOT_STATUS_RUNNING, 7_000)
            .expect("update to running"),
        "命中行返回 true"
    );
    let running = store
        .get_bot_status(&bot_id())
        .expect("re-read")
        .expect("seeded bot");
    assert_eq!(running.status, BOT_STATUS_RUNNING);
    assert_eq!(running.updated_at, 7_000);

    let error = store
        .update_bot_status(&bot_id(), "exploded", 8_000)
        .expect_err("非法状态必须报错");
    match error {
        StoreError::Invalid {
            field,
            value,
            allowed,
        } => {
            assert_eq!(field, "bots.status");
            assert_eq!(value, "exploded");
            assert!(allowed.contains(&BOT_STATUS_RUNNING));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(
        store
            .get_bot_status(&bot_id())
            .expect("re-read after rejection")
            .expect("seeded bot")
            .status,
        BOT_STATUS_RUNNING,
        "被拒绝的值不得落库"
    );

    assert!(
        !store
            .update_bot_status(&bot_id_at(777), BOT_STATUS_RUNNING, 9_000)
            .expect("missing bot"),
        "不存在的 bot 返回 false"
    );
    assert!(!is_valid_bot_status("exploded"));
    assert!(is_valid_bot_status(BOT_STATUS_STOPPED));
}

/// 日志按写入序（`id` 降序）返回最新在前，`limit` 生效，`bot_id=None` 时跨 bot 汇总。
#[test]
fn list_logs_returns_newest_first_and_limits() {
    let dir = TempDir::new("logs-list");
    let store = open_store(&dir);

    for (index, line) in ["first", "second", "third", "fourth"].iter().enumerate() {
        store
            .append_log(new_log("info", line, 100 + index as i64))
            .expect("append log");
    }
    // 另一个 bot 的日志：按 bot 过滤时不能出现。
    store
        .insert_bot(&Bot {
            id: bot_id_at(20),
            project_id: project_id(),
            name: "other-bot".to_owned(),
            platform: "onebot".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 10,
        })
        .expect("insert other bot");
    store
        .append_log(NewLog {
            bot_id: bot_id_at(20),
            level: "warn".to_owned(),
            line: "other-bot-line".to_owned(),
            at: 200,
        })
        .expect("append other bot log");

    let mine = store
        .list_logs(Some(&bot_id()), 10)
        .expect("list seeded bot logs");
    let lines: Vec<&str> = mine.iter().map(|row| row.line.as_str()).collect();
    assert_eq!(lines, vec!["fourth", "third", "second", "first"]);
    assert!(
        mine.windows(2).all(|pair| pair[0].id > pair[1].id),
        "必须按 id 降序"
    );

    let limited = store.list_logs(Some(&bot_id()), 2).expect("limited logs");
    let limited_lines: Vec<&str> = limited.iter().map(|row| row.line.as_str()).collect();
    assert_eq!(limited_lines, vec!["fourth", "third"]);

    let all = store.list_logs(None, 10).expect("all logs");
    assert_eq!(all.len(), 5, "不限 bot 时包含另一个 bot 的日志");
    assert_eq!(all[0].line, "other-bot-line", "最新一条在最前");
}

/// 待发送计数与列表条件必须完全一致（指标 / 背压用同一份定义）。
#[test]
fn count_pending_outbound_matches_list_pending() {
    let dir = TempDir::new("pending-count");
    let store = open_store(&dir);

    assert_eq!(store.count_pending_outbound().expect("count empty"), 0);
    assert!(
        store
            .list_pending_outbound(10)
            .expect("list empty")
            .is_empty()
    );

    for id in [41_u128, 42, 43, 44] {
        store
            .enqueue_outbound(new_outbound(id))
            .expect("enqueue outbound");
    }
    store
        .mark_outbound_sent(&outbound_id(41), 1_000)
        .expect("mark sent");
    store
        .mark_outbound_failed(&outbound_id(42), 2)
        .expect("mark failed");

    let count = store.count_pending_outbound().expect("count");
    let listed = store.list_pending_outbound(10).expect("list");
    assert_eq!(count, listed.len() as u32);
    assert_eq!(count, 2);
    let ids: Vec<_> = listed.iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![outbound_id(43), outbound_id(44)]);
}

/// 0001 老库升级到最新：旧行保留、新列取默认值、水位推到最新。
///
/// 这是 `docs/05-quality-gates.md` G6 的“空库/老库都能升到最新”回归测试：手写一个只有
/// 0001 的库（不经过 `Store::open`），再用当前二进制打开它。
#[test]
fn upgrading_from_0001_keeps_rows_and_adds_new_columns() {
    let dir = TempDir::new("upgrade-0001");
    {
        let conn = raw(&dir);
        conn.execute_batch(migrations::MIGRATIONS[0].sql)
            .expect("apply 0001 by hand");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                 version    INTEGER PRIMARY KEY NOT NULL,
                 name       TEXT    NOT NULL,
                 applied_at INTEGER NOT NULL
             );
             INSERT INTO schema_migrations (version, name, applied_at)
                 VALUES (1, '0001_init', 0);
             PRAGMA user_version = 1;",
        )
        .expect("record 0001 as applied");

        conn.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES (?1, 'legacy', '/tmp/legacy', 1)",
            params![project_id().to_string()],
        )
        .expect("insert legacy project");
        conn.execute(
            "INSERT INTO bots (id, project_id, name, platform, config_json, enabled, created_at)
             VALUES (?1, ?2, 'legacy-bot', 'onebot', '{}', 1, 2)",
            params![bot_id().to_string(), project_id().to_string()],
        )
        .expect("insert legacy bot");
        conn.execute(
            "INSERT INTO conversations (id, bot_id, external_id, kind, created_at)
             VALUES (?1, ?2, 'group-42', 'group', 3)",
            params![conversation_id().to_string(), bot_id().to_string()],
        )
        .expect("insert legacy conversation");
        conn.execute(
            "INSERT INTO inbound (id, bot_id, external_id, conversation_id, payload_json, received_at)
             VALUES (?1, ?2, 'legacy-ext', ?3, '{}', 1000)",
            params![
                inbound_id(51).to_string(),
                bot_id().to_string(),
                conversation_id().to_string()
            ],
        )
        .expect("insert legacy inbound");
        conn.execute(
            "INSERT INTO outbound (id, bot_id, conversation_id, text, state, attempts, created_at)
             VALUES (?1, ?2, ?3, 'legacy-reply', 'pending', 0, 2000)",
            params![
                outbound_id(52).to_string(),
                bot_id().to_string(),
                conversation_id().to_string()
            ],
        )
        .expect("insert legacy outbound");
    }

    let store = Store::open(dir.db_path()).expect("open and upgrade legacy store");
    assert_eq!(store.schema_version().expect("schema version"), 3);

    let pending = store.list_pending_outbound(10).expect("list pending");
    assert_eq!(pending.len(), 1, "升级不得丢旧行");
    assert_eq!(pending[0].id, outbound_id(52));
    assert_eq!(pending[0].text, "legacy-reply");
    assert_eq!(pending[0].target_json, "{}", "新列取默认值");
    assert!(pending[0].reply_to.is_none(), "新列默认 NULL");
    assert_eq!(pending[0].attempts, 0);

    let inbound = store.list_unprocessed_inbound(10).expect("list inbound");
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].sender_id, "", "0002 的 sender_id 默认空串");

    let status = store
        .get_bot_status(&bot_id())
        .expect("legacy bot status")
        .expect("legacy bot");
    assert_eq!(status.status, BOT_STATUS_STOPPED);
    assert_eq!(status.updated_at, 0);
}

/// 0002 老库升级到 0003：`logs` 表重建后旧行一条不少、`bot_id` 仍指向原 bot、`source='bot'`。
#[test]
fn upgrading_from_0002_keeps_existing_logs_with_bot_id_and_bot_source() {
    let dir = TempDir::new("upgrade-0002-logs");
    {
        let conn = raw(&dir);
        conn.execute_batch(migrations::MIGRATIONS[0].sql)
            .expect("apply 0001 by hand");
        conn.execute_batch(migrations::MIGRATIONS[1].sql)
            .expect("apply 0002 by hand");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                 version    INTEGER PRIMARY KEY NOT NULL,
                 name       TEXT    NOT NULL,
                 applied_at INTEGER NOT NULL
             );
             INSERT INTO schema_migrations (version, name, applied_at) VALUES
                 (1, '0001_init', 0),
                 (2, '0002_outbound_routing_and_status', 0);
             PRAGMA user_version = 2;",
        )
        .expect("record 0001+0002 as applied");
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES (?1, 'legacy', '/tmp/legacy', 1)",
            params![project_id().to_string()],
        )
        .expect("insert legacy project");
        conn.execute(
            "INSERT INTO bots (id, project_id, name, platform, config_json, enabled, created_at)
             VALUES (?1, ?2, 'legacy-bot', 'onebot', '{}', 1, 2)",
            params![bot_id().to_string(), project_id().to_string()],
        )
        .expect("insert legacy bot");
        // 0002 的 logs 还是 NOT NULL bot_id 的老形状。
        for (line, at) in [("old-first", 10_i64), ("old-second", 20)] {
            conn.execute(
                "INSERT INTO logs (bot_id, level, line, at) VALUES (?1, 'info', ?2, ?3)",
                params![bot_id().to_string(), line, at],
            )
            .expect("insert legacy log");
        }
    }

    let store = Store::open(dir.db_path()).expect("open and upgrade v2 store");
    assert_eq!(store.schema_version().expect("schema version"), 3);

    let rows = store
        .list_logs(Some(&bot_id()), 10)
        .expect("list legacy logs");
    assert_eq!(rows.len(), 2, "升级不得丢旧日志行");
    assert_eq!(rows[0].line, "old-second");
    assert_eq!(rows[1].line, "old-first");
    assert!(
        rows.iter().all(|row| row.bot_id == Some(bot_id())),
        "旧日志的 bot_id 必须保留"
    );
    assert!(
        rows.iter().all(|row| row.source == "bot"),
        "旧日志的 source 必须是 'bot'"
    );

    drop(store);
    let raw = raw(&dir);
    let user_version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(user_version, 3, "user_version 必须推进到 3");

    let table_sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'logs'",
            [],
            |row| row.get(0),
        )
        .expect("logs table sql");
    assert!(
        !table_sql.contains("bot_id  TEXT NOT NULL"),
        "logs.bot_id 必须可空：{table_sql}"
    );
    assert!(
        table_sql.contains("source  TEXT NOT NULL DEFAULT 'bot'"),
        "logs.source 必须是 NOT NULL DEFAULT 'bot'：{table_sql}"
    );

    let mut indexes: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'logs'")
        .expect("prepare index query")
        .query_map([], |row| row.get(0))
        .expect("query indexes")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect indexes");
    indexes.sort();
    assert_eq!(
        indexes,
        vec!["idx_logs_at".to_owned(), "idx_logs_bot_at".to_owned()],
        "两个索引都必须存在"
    );
}

/// Agent 行写 `bot_id = NULL` + `source = 'agent'`；按 bot 过滤时看不到，全项目时看得到。
#[test]
fn agent_logs_are_nullable_and_filtered_out_of_bot_views() {
    let dir = TempDir::new("agent-logs");
    let store = open_store(&dir);

    store
        .append_log(new_log("info", "bot-line", 100))
        .expect("append bot log");
    store
        .append_agent_log(NewAgentLog {
            level: "info".to_owned(),
            line: "[session 3f2a…] agent 工具 fs_read 成功（1 ms）".to_owned(),
            at: 101,
        })
        .expect("append agent log");

    let all = store.list_logs(None, 10).expect("list all logs");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].source, "agent");
    assert!(all[0].bot_id.is_none(), "Agent 行不允许有 bot_id");
    assert_eq!(all[1].source, "bot");
    assert_eq!(all[1].bot_id, Some(bot_id()));

    let bot_view = store.list_logs(Some(&bot_id()), 10).expect("list bot logs");
    assert_eq!(bot_view.len(), 1, "Agent 行不能出现在 bot 视图里");
    assert_eq!(bot_view[0].line, "bot-line");
}

/// `source` 的列默认值仍是 `'bot'`：直接 INSERT 不带 source 的老写法不会变成 NULL/agent。
#[test]
fn logs_source_defaults_to_bot_for_direct_inserts() {
    let dir = TempDir::new("logs-source-default");
    let store = open_store(&dir);
    drop(store);

    let conn = raw(&dir);
    conn.execute(
        "INSERT INTO logs (bot_id, level, line, at) VALUES (?1, 'info', 'no-source', 1)",
        params![bot_id().to_string()],
    )
    .expect("insert log without source");
    let source: String = conn
        .query_row(
            "SELECT source FROM logs WHERE line = 'no-source'",
            [],
            |row| row.get(0),
        )
        .expect("read source");
    assert_eq!(source, "bot");
}

/// 死信状态常量与 `state` 列一致：`failed` 不等于 `sent`，两者都不是 `pending`。
#[test]
fn outbound_state_constants_are_distinct() {
    assert_eq!(OUTBOUND_STATE_FAILED, "failed");
    assert_ne!(OUTBOUND_STATE_FAILED, OUTBOUND_STATE_SENT);
    assert_ne!(OUTBOUND_STATE_FAILED, crate::OUTBOUND_STATE_PENDING);
}
