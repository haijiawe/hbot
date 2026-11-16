//! 预算 3：批量写 1000 条 < 200 ms（`hbot-store`）。
//!
//! **这条预算的原始口径与当前实现不是一回事，读数字前先读这一段。**
//!
//! `hbot-store` 没有批量 API，也没有“外部事务闭包”入口：`crates/hbot-store/src/` 里
//! `transaction` 只出现在 `migrations.rs`（建库迁移自己用），业务写入路径上一条都没有。
//! 每个写方法都是「构造一个 `StoreOp` → 投给 `hbot-store-writer` 线程 → 阻塞等回复」，
//! db 线程上那条 SQL 在**自动提交模式**下执行 —— 即 **1 行 = 1 个独立事务**。
//!
//! 所以这里量三种，只有第一种参与判定：
//!
//! * `store_1000_writes_individual`（**判定**）：1000 次 `enqueue_outbound`，就是当前实现
//!   在“写 1000 条”这件事上的真实路径与真实耗时；
//! * `store_1000_writes_single_tx`（参考）：绕过 Store，用 rusqlite 直连同一个 schema，
//!   **一个事务里插 1000 行** —— 这才是 < 200 ms 这条预算被设定时的口径；
//! * `store_1000_writes_rusqlite_autocommit`（参考）：绕过 Store 的**通道往返**，但仍然
//!   1000 个自动提交事务 —— 用来把“通道/线程切换”与“SQLite 事务”两部分开销拆开。
//!
//! 判定只打第一行；后面两行是归因证据，不是让数字变好看的第二口径。
//!
//! 数据怎么造的：临时目录里 `Store::open` 一个新库（跑完即删），先插 1 个 project →
//! 1 个 bot → 1 个 conversation 满足外键，再写 1000 条 `pending` 出站消息（文案约 60–90 B，
//! 每条新 UUID、`created_at` 递增）。三个参考库各自独立建、独立插，互不共用连接。

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hbot_store::{
    Bot, BotId, Conversation, ConversationId, NewOutbound, OutboundId, Project, ProjectId, Store,
};
use rusqlite::params;
use uuid::Uuid;

use crate::harness::{self, Stats, TempDir, Unit};

/// 每次测量的行数（预算就是 1000 条）。
const ROWS: usize = 1000;
/// 每种路径复跑几次，取中位数。
const RUNS: usize = 3;
/// 预算：批量写 1000 条 < 200 ms。
const BUDGET: Duration = Duration::from_millis(200);
/// 与 `crates/hbot-store/src/repositories/outbound.rs` 的 `enqueue` 逐字一致的 INSERT，
/// 供参考测量复用（同一 schema、同一语句，只换事务边界）。
const INSERT_OUTBOUND: &str = "INSERT INTO outbound
         (id, bot_id, conversation_id, target_json, text, reply_to, state, attempts, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)";
/// 出站消息初始状态（与 `hbot_store::OUTBOUND_STATE_PENDING` 同值；这里写字面量是为了
/// 参考测量也能在没有 store 类型参与的情况下执行同一句 SQL）。
const STATE_PENDING: &str = "pending";

/// 外键父行：project → bot → conversation。
#[derive(Clone, Copy)]
struct Seeded {
    bot_id: BotId,
    conversation_id: ConversationId,
}

/// 跑 1000 条写入测量并打印结果行。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    harness::require_release();
    harness::section("3. 批量写 1000 条（hbot_store：逐条 API，无批量/外部事务入口）");

    let temp = TempDir::new("store")?;
    let db = temp.join("hbot-store.db");
    let store = Store::open(&db)?;
    let seeded = seed(&store, "bench-store")?;

    harness::info_line(
        "store_engine",
        &format!(
            "journal={} / schema_version={} / busy_timeout={} ms",
            store.journal_mode()?,
            store.schema_version()?,
            store.busy_timeout_ms()?
        ),
        "-",
        "WAL + synchronous=NORMAL + foreign_keys=ON；写入串行经过 hbot-store-writer 线程",
    );
    harness::info_line(
        "store_api_shape",
        "1 行 = 1 事务（逐条 API）",
        "-",
        "无批量 API、无外部事务闭包：每次调用 = 一次通道往返 + 一条自动提交事务（见模块文档）",
    );

    let mut totals = Vec::with_capacity(RUNS);
    let mut per_row = Vec::with_capacity(ROWS * RUNS);
    for run in 0..RUNS {
        let started = Instant::now();
        for index in 0..ROWS {
            let row_started = Instant::now();
            store.enqueue_outbound(NewOutbound {
                id: OutboundId::from_uuid(Uuid::new_v4()),
                bot_id: seeded.bot_id,
                conversation_id: seeded.conversation_id,
                text: outbound_text(run, index),
                created_at: now_ms() + index as i64,
            })?;
            per_row.push(row_started.elapsed());
        }
        totals.push(started.elapsed());
    }

    let pending = store.count_pending_outbound()?;
    assert_eq!(
        pending as usize,
        ROWS * RUNS,
        "写进去的 pending 行数不对（{} != {}）",
        pending,
        ROWS * RUNS
    );

    let mut sorted = per_row.clone();
    sorted.sort_unstable();
    let mean = per_row.iter().copied().sum::<Duration>() / per_row.len() as u32;
    harness::info_line(
        "store_per_row_mean",
        &Unit::Us.render(mean),
        "µs",
        &format!(
            "单行平均（{} 个样本 = {} 次 × {} 行）；一行 = 一次通道往返 + 一个事务",
            per_row.len(),
            RUNS,
            ROWS
        ),
    );
    harness::info_line(
        "store_per_row_p50",
        &Unit::Us.render(harness::percentile(&sorted, 0.5)),
        "µs",
        "单行 p50",
    );
    harness::info_line(
        "store_per_row_p95",
        &Unit::Us.render(harness::percentile(&sorted, 0.95)),
        "µs",
        "单行 p95（右侧长尾 = Windows 上 fsync/调度抖动）",
    );
    harness::info_line(
        "store_rows_written",
        &format!("{pending}"),
        "rows",
        "已落库的 pending 出站行（= 3 次 × 1000 行，用于确认测量真的写了）",
    );

    harness::budget_line(
        "store_1000_writes_individual",
        &Stats::of(&totals),
        Unit::Ms,
        BUDGET,
        "现行实现的真实路径：1000 次 enqueue_outbound = 1000 个独立事务 + 1000 次 store 线程往返；预算原口径是单事务 1000 条 ⇒ 本行与预算不同口径",
    );

    // ---- 参考：同一 schema 下把两条成本拆开 ----------------------------
    let single_tx = reference_single_transaction(&temp, "ref-single.db", "ref-single")?;
    let direct_autocommit = reference_autocommit(&temp, "ref-auto.db", "ref-auto")?;

    harness::info_line(
        "store_1000_writes_single_tx",
        &Unit::Ms.render(Stats::of(&single_tx).median),
        "ms",
        "参考（不判定）：绕过 Store，rusqlite 直连，单事务插 1000 行 —— 这才是 < 200 ms 预算的原始口径",
    );
    harness::info_line(
        "store_1000_writes_rusqlite_autocommit",
        &Unit::Ms.render(Stats::of(&direct_autocommit).median),
        "ms",
        "参考（不判定）：绕过 Store 的通道往返，仍 1000 个自动提交事务 —— 与上一行的差 ≈ 事务边界成本，与判定行的差 ≈ 通道/线程成本",
    );

    let judged = Stats::of(&totals).median;
    let single = Stats::of(&single_tx).median;
    let direct = Stats::of(&direct_autocommit).median;
    harness::info_line(
        "store_attribution",
        &format!(
            "逐条 {:.1} ms / 单事务 {:.1} ms = {:.0}×；1000 个事务本身 {:.1} ms",
            judged.as_secs_f64() * 1e3,
            single.as_secs_f64() * 1e3,
            judged.as_secs_f64() / single.as_secs_f64().max(f64::MIN_POSITIVE),
            direct.as_secs_f64() * 1e3
        ),
        "-",
        "归因：多数成本来自“1000 个事务（各自提交/同步）”，其中通道往返占一部分（见 per_row 均值与单事务 per_row 之差）",
    );

    Ok(())
}

/// 建父行并返回 ids。
fn seed(store: &Store, tag: &str) -> Result<Seeded, Box<dyn std::error::Error>> {
    let now = now_ms();
    let project_id = ProjectId::from_uuid(Uuid::new_v4());
    store.insert_project(&Project {
        id: project_id,
        name: format!("bench-{tag}"),
        path: format!("C:\\hbot-bench\\{tag}"),
        created_at: now,
    })?;
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    store.insert_bot(&Bot {
        id: bot_id,
        project_id,
        name: format!("bench-bot-{tag}"),
        platform: "onebot11".to_owned(),
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: now,
    })?;
    let conversation_id = ConversationId::from_uuid(Uuid::new_v4());
    store.insert_conversation(&Conversation {
        id: conversation_id,
        bot_id,
        external_id: format!("group-{tag}"),
        kind: "group".to_owned(),
        created_at: now,
    })?;
    Ok(Seeded {
        bot_id,
        conversation_id,
    })
}

/// 建一个独立库、插好父行，然后丢掉 Store（关掉写线程）返回库路径。
fn seed_db(
    temp: &TempDir,
    file: &str,
    tag: &str,
) -> Result<(std::path::PathBuf, Seeded), Box<dyn std::error::Error>> {
    let path = temp.join(file);
    let seeded = {
        let store = Store::open(&path)?;
        seed(&store, tag)?
    };
    Ok((path, seeded))
}

/// 参考测量：一个事务里插 `ROWS` 行，复跑 `RUNS` 次。
fn reference_single_transaction(
    temp: &TempDir,
    file: &str,
    tag: &str,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let (path, seeded) = seed_db(temp, file, tag)?;
    let conn = open_reference_connection(&path)?;
    let mut samples = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let started = Instant::now();
        {
            let transaction = conn.unchecked_transaction()?;
            for index in 0..ROWS {
                insert_reference_row(&transaction, seeded, run, index)?;
            }
            transaction.commit()?;
        }
        samples.push(started.elapsed());
    }
    Ok(samples)
}

/// 参考测量：不包事务，`ROWS` 次自动提交，复跑 `RUNS` 次。
fn reference_autocommit(
    temp: &TempDir,
    file: &str,
    tag: &str,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let (path, seeded) = seed_db(temp, file, tag)?;
    let conn = open_reference_connection(&path)?;
    let mut samples = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let started = Instant::now();
        for index in 0..ROWS {
            insert_reference_row(&conn, seeded, run, index)?;
        }
        samples.push(started.elapsed());
    }
    Ok(samples)
}

/// 与 `Store::open_connection` 同口径的 PRAGMA（WAL 已在库里持久化）。
fn open_reference_connection(
    path: &Path,
) -> Result<rusqlite::Connection, Box<dyn std::error::Error>> {
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
    )?;
    Ok(conn)
}

/// 参考测量用的一行（`&Connection` 与 `&Transaction` 都解引用到 `Connection`）。
fn insert_reference_row(
    conn: &rusqlite::Connection,
    seeded: Seeded,
    run: usize,
    index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute(
        INSERT_OUTBOUND,
        params![
            OutboundId::from_uuid(Uuid::new_v4()).to_string(),
            seeded.bot_id.to_string(),
            seeded.conversation_id.to_string(),
            "{}",
            outbound_text(run, index),
            None::<String>,
            STATE_PENDING,
            now_ms() + index as i64,
        ],
    )?;
    Ok(())
}

/// 与真实出站消息同量级的文案（约 60–90 B）。
fn outbound_text(run: usize, index: usize) -> String {
    format!("bench outbound run {run} row {index:04} — 你好，这是一条用于批量写入测量的消息")
}

/// Unix 毫秒（与 store 的 `created_at` 口径一致）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
