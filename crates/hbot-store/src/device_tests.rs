//! `device_sessions` / `device_audit` 的存储侧测试（`docs/09-device-capability.md` §3、§7）。
//!
//! 这里只钉**存储契约**：行里存什么、读回来什么顺序、撤销是不是原子、审计能不能装内容。
//! 状态机本身（谁能迁到谁、超时怎么结算）不在这里 —— 那是 `hbot-rpc` / `hbot-device` 的事，
//! 存储层只按调用方给的语句落库。

use rusqlite::Connection;

use crate::Store;
use crate::ids::BotId;
use crate::mappers::{DEVICE_AUDIT_COLUMNS, DEVICE_SESSION_COLUMNS};
use crate::migrations;
use crate::models::{Bot, NewDeviceAudit, NewDeviceSession};
use crate::repositories::device::STATE_ACTIVE;
use crate::test_support::{TempDir, bot_id, bot_id_at, open_store, project_id, raw};

/// 造一条属于种子 bot 的设备会话（`created_at` 由调用方定）。
fn new_session(id: &str, created_at: i64) -> NewDeviceSession {
    NewDeviceSession {
        id: id.to_owned(),
        bot_id: bot_id(),
        state: STATE_ACTIVE.to_owned(),
        sub_json: "[\"screen\",\"input\"]".to_owned(),
        allowed_apps_json: "[\"notepad.exe\"]".to_owned(),
        granted_at: Some(created_at + 10),
        deadline: Some(created_at + 300_000),
        created_at,
    }
}

/// 造一条属于种子 bot 的审计。
fn new_audit(
    at: i64,
    actor: &str,
    action: &str,
    target: Option<&str>,
    outcome: &str,
) -> NewDeviceAudit {
    NewDeviceAudit {
        at,
        bot_id: bot_id(),
        actor: actor.to_owned(),
        action: action.to_owned(),
        target: target.map(str::to_owned),
        outcome: outcome.to_owned(),
    }
}

/// 再登记一个 bot：外键要求会话 / 审计的 `bot_id` 必须存在，用来验证"per-bot"过滤。
fn other_bot(store: &Store) -> BotId {
    let id = bot_id_at(999);
    store
        .insert_bot(&Bot {
            id,
            project_id: project_id(),
            name: "other-bot".to_owned(),
            platform: "onebot".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 4,
        })
        .expect("insert other bot");
    id
}

/// 裸读全部会话（`id` 升序），用于断言"没被动的行真的没动"。
fn sessions(conn: &Connection) -> Vec<(String, String, Option<i64>)> {
    let mut statement = conn
        .prepare("SELECT id, state, ended_at FROM device_sessions ORDER BY id")
        .expect("prepare sessions");
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .expect("query sessions")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect sessions")
}

/// 裸读 `schema_migrations`（与 `tests::schema_migrations_rows` 同口径）。
fn migration_rows(dir: &TempDir) -> Vec<(i64, String, i64)> {
    let conn = raw(dir);
    let mut statement = conn
        .prepare("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
        .expect("prepare schema_migrations");
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("query schema_migrations")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect schema_migrations")
}

/// `PRAGMA table_info(<table>)` 的列名清单（顺序 = 声明顺序）。
fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect column names")
}

/// ① 插入读回：每个字段都对得上，新会话的 `ended_at` 是 NULL。
#[test]
fn device_session_round_trips_every_column() {
    let dir = TempDir::new("device-roundtrip");
    let store = open_store(&dir);

    let inserted = store
        .insert_device_session(new_session("s-1", 1_000))
        .expect("insert session");

    assert_eq!(inserted.id, "s-1");
    assert_eq!(inserted.bot_id, bot_id());
    assert_eq!(inserted.state, STATE_ACTIVE);
    assert_eq!(inserted.sub_json, "[\"screen\",\"input\"]");
    assert_eq!(inserted.allowed_apps_json, "[\"notepad.exe\"]");
    assert_eq!(inserted.granted_at, Some(1_010));
    assert_eq!(inserted.deadline, Some(301_000));
    assert!(inserted.ended_at.is_none(), "新会话不该带 ended_at");
    assert_eq!(inserted.created_at, 1_000);

    let latest = store
        .latest_device_session(&bot_id())
        .expect("latest session")
        .expect("刚插入的会话必须能读到");
    assert_eq!(latest, inserted, "读回的行必须与插入返回的行逐字段一致");

    // 没有会话的 bot 返回 None（不报错、不伪造一行）。
    let none = store
        .latest_device_session(&bot_id_at(1_234))
        .expect("latest for empty bot");
    assert!(none.is_none());
}

/// ② `latest` 的排序确定性：`created_at` 相同时按 `id DESC`，不靠 rowid / 插入顺序。
#[test]
fn latest_device_session_breaks_created_at_ties_by_id_descending() {
    let dir = TempDir::new("device-latest-tie");
    let store = open_store(&dir);

    // 同一毫秒两行，故意**先**插 "b" 再插 "a"：按 rowid / 插入顺序取最后一条会给出 "a"（错），
    // 只有 `id DESC` 才给出 "b"（对）。这条是钉住排序确定性的关键构造。
    store
        .insert_device_session(new_session("b", 500))
        .expect("insert b");
    store
        .insert_device_session(new_session("a", 500))
        .expect("insert a");
    // 更早的一行，最后插入：它不许因为"插得晚"被当成最新。
    store
        .insert_device_session(new_session("z", 400))
        .expect("insert z");

    let latest = store
        .latest_device_session(&bot_id())
        .expect("latest session")
        .expect("session row");
    assert_eq!(latest.id, "b", "created_at 相同必须按 id DESC 定序");
    assert_eq!(latest.created_at, 500);

    // `created_at` 才是第一排序键：更新的行一到，就是它。
    store
        .insert_device_session(new_session("a-new", 600))
        .expect("insert a-new");
    let latest = store
        .latest_device_session(&bot_id())
        .expect("latest session")
        .expect("session row");
    assert_eq!(latest.id, "a-new");
}

/// ③ `end_live_device_sessions` 只动 `active` 行，返回命中行数，别的状态原样保留。
#[test]
fn end_live_device_sessions_touches_only_active_rows() {
    let dir = TempDir::new("device-end-live");
    let store = open_store(&dir);
    let other = other_bot(&store);

    for (id, state) in [
        ("s-active", STATE_ACTIVE),
        ("s-requested", "requested"),
        ("s-expired", "expired"),
        ("s-revoked", "revoked"),
        ("s-denied", "denied"),
    ] {
        let mut session = new_session(id, 100);
        session.state = state.to_owned();
        store
            .insert_device_session(session)
            .expect("insert session");
    }
    // 别的 bot 的 active 行：撤销是本 bot 的全局失效，不许顺手动别人。
    let mut other_session = new_session("s-other", 100);
    other_session.bot_id = other;
    store
        .insert_device_session(other_session)
        .expect("insert other bot session");

    let ended = store
        .end_live_device_sessions(&bot_id(), "revoked", 9_999)
        .expect("end live sessions");
    assert_eq!(ended, 1, "只有那一条 active 该被处理");

    drop(store);
    let conn = raw(&dir);
    assert_eq!(
        sessions(&conn),
        vec![
            ("s-active".to_owned(), "revoked".to_owned(), Some(9_999)),
            ("s-denied".to_owned(), "denied".to_owned(), None),
            ("s-expired".to_owned(), "expired".to_owned(), None),
            ("s-other".to_owned(), STATE_ACTIVE.to_owned(), None),
            ("s-requested".to_owned(), "requested".to_owned(), None),
            ("s-revoked".to_owned(), "revoked".to_owned(), None),
        ],
        "只有 active 行被改写：其它状态与别的 bot 的行必须原样保留"
    );
}

/// ④ 撤销后再开新会话：`latest` 拿到新的，旧行留成 `revoked` 历史（不是删除）。
#[test]
fn revoked_session_stays_as_history_and_the_next_session_wins() {
    let dir = TempDir::new("device-revoke-reopen");
    let store = open_store(&dir);

    store
        .insert_device_session(new_session("old", 1_000))
        .expect("insert old session");
    assert_eq!(
        store
            .latest_device_session(&bot_id())
            .expect("latest")
            .expect("row")
            .id,
        "old"
    );

    let ended = store
        .end_live_device_sessions(&bot_id(), "revoked", 2_000)
        .expect("revoke");
    assert_eq!(ended, 1);

    // 撤销立即可见（同一个 Store、紧接着的读）。
    let revoked = store
        .latest_device_session(&bot_id())
        .expect("latest")
        .expect("row");
    assert_eq!(revoked.id, "old");
    assert_eq!(revoked.state, "revoked");
    assert_eq!(revoked.ended_at, Some(2_000));

    // 不自动续期：新会话是**新的一行**。
    store
        .insert_device_session(new_session("new", 3_000))
        .expect("insert new session");
    let latest = store
        .latest_device_session(&bot_id())
        .expect("latest")
        .expect("row");
    assert_eq!(latest.id, "new");
    assert_eq!(latest.state, STATE_ACTIVE);
    assert!(latest.ended_at.is_none());

    // 旧行还在，且还是 revoked —— 撤销不是删除，审计要靠它。
    drop(store);
    let conn = raw(&dir);
    assert_eq!(
        sessions(&conn),
        vec![
            ("new".to_owned(), STATE_ACTIVE.to_owned(), None),
            ("old".to_owned(), "revoked".to_owned(), Some(2_000)),
        ]
    );
}

/// ⑤ 审计：只按 bot 过滤、按 `at DESC`、`limit` 生效、同一毫秒按 `id DESC`。
#[test]
fn device_audit_is_filtered_by_bot_and_ordered_by_time_desc() {
    let dir = TempDir::new("device-audit-order");
    let store = open_store(&dir);
    let other = other_bot(&store);

    store
        .append_device_audit(new_audit(100, "bot_code", "screenshot", None, "ok"))
        .expect("audit 100");
    store
        .append_device_audit(new_audit(
            300,
            "agent_tool",
            "key",
            Some("notepad.exe"),
            "ok",
        ))
        .expect("audit 300");
    store
        .append_device_audit(new_audit(200, "bot_code", "focus", Some("42"), "denied"))
        .expect("audit 200");

    let mut foreign = new_audit(999, "bot_code", "click", None, "ok");
    foreign.bot_id = other;
    store.append_device_audit(foreign).expect("audit other bot");

    let rows = store.list_device_audit(&bot_id(), 10).expect("list audit");
    assert_eq!(
        rows.iter().map(|row| row.at).collect::<Vec<_>>(),
        vec![300, 200, 100],
        "必须 at DESC，且别的 bot 的行不许混进来"
    );
    assert!(rows.iter().all(|row| row.bot_id == bot_id()));
    assert_eq!(rows[0].actor, "agent_tool");
    assert_eq!(rows[0].target.as_deref(), Some("notepad.exe"));
    assert_eq!(rows[2].outcome, "ok");

    let limited = store
        .list_device_audit(&bot_id(), 2)
        .expect("limited audit");
    assert_eq!(
        limited.iter().map(|row| row.at).collect::<Vec<_>>(),
        vec![300, 200],
        "limit 取最近的 N 条"
    );

    // 同一毫秒：后写的 id 更大，排在前面。
    store
        .append_device_audit(new_audit(400, "bot_code", "move", None, "ok"))
        .expect("audit 400 first");
    store
        .append_device_audit(new_audit(400, "bot_code", "move", None, "failed"))
        .expect("audit 400 second");
    let ties = store.list_device_audit(&bot_id(), 2).expect("tie audit");
    assert_eq!(
        ties[0].outcome, "failed",
        "同一 at 必须按 id DESC（后写的在前）"
    );
    assert!(ties[0].id > ties[1].id);
}

/// ⑥ 已有库重开不重放迁移（沿用 `reopening_does_not_rerun_migrations` 的哨兵口径）；
/// 0004 自身也可重跑。
#[test]
fn reopening_a_database_with_device_tables_does_not_rerun_migrations() {
    const SENTINEL: i64 = 42;
    let dir = TempDir::new("device-reopen");

    {
        let store = open_store(&dir);
        store
            .insert_device_session(new_session("s-1", 1_000))
            .expect("insert session");
        store
            .append_device_audit(new_audit(1_000, "bot_code", "screenshot", None, "ok"))
            .expect("append audit");
        assert_eq!(
            store.schema_version().expect("schema version"),
            migrations::latest_version()
        );
    }
    assert_eq!(
        migrations::latest_version(),
        4,
        "0004_device 必须是第 4 条迁移（版本号与文件名前缀一致）"
    );

    // 把记账时间改成哨兵：迁移一旦重放，applied_at 会被覆盖回 now。
    raw(&dir)
        .execute("UPDATE schema_migrations SET applied_at = ?1", [SENTINEL])
        .expect("write sentinel");
    let before = migration_rows(&dir);
    assert_eq!(before.len(), 4, "0001–0004 各一行");
    assert!(
        before
            .iter()
            .all(|(_, _, applied_at)| *applied_at == SENTINEL),
        "哨兵没写进去，后面的断言就不成立"
    );
    assert!(
        before.iter().any(|(_, name, _)| name == "0004_device"),
        "0004_device 未登记进 MIGRATIONS"
    );

    let reopened = Store::open(dir.db_path()).expect("reopen store");
    assert_eq!(
        reopened.schema_version().expect("schema version"),
        migrations::latest_version()
    );
    let latest = reopened
        .latest_device_session(&bot_id())
        .expect("latest session")
        .expect("重开后会话必须还在");
    assert_eq!(latest.id, "s-1");
    assert_eq!(
        reopened
            .list_device_audit(&bot_id(), 10)
            .expect("audit after reopen")
            .len(),
        1
    );
    drop(reopened);
    assert_eq!(
        migration_rows(&dir),
        before,
        "第二次打开改写了 schema_migrations：迁移被重放了"
    );

    // 0004 的 SQL 自身也必须幂等（全部 CREATE … IF NOT EXISTS）：手工再跑两遍，行数不变。
    let conn = raw(&dir);
    conn.execute_batch(include_str!("migrations/0004_device.sql"))
        .expect("0004 必须可重跑");
    conn.execute_batch(include_str!("migrations/0004_device.sql"))
        .expect("0004 必须可重跑（第二遍）");
    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM device_sessions", [], |row| row.get(0))
        .expect("count sessions");
    let audit_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM device_audit", [], |row| row.get(0))
        .expect("count audit");
    assert_eq!(
        (session_count, audit_count),
        (1, 1),
        "重跑 0004 不许清表或重复建行"
    );
}

/// ⑦ 审计表**不接受内容字段**（`docs/09` §3.2、§4.3）。
///
/// 列清单被精确钉死：加一列 `content` / `png` / `ocr_text` 都会让这条测试红。
/// 记了内容，审计本身就成了新的泄漏面 —— 密码框的像素、剪贴板里的 token、屏幕 OCR 原文
/// 会一直躺在库里被导出、被展示、被喂进模型上下文。
#[test]
fn device_audit_rejects_content_columns() {
    let dir = TempDir::new("device-audit-columns");
    let store = open_store(&dir);
    drop(store);

    let conn = raw(&dir);
    let audit_columns = columns(&conn, "device_audit");
    assert_eq!(
        audit_columns,
        vec!["id", "at", "bot_id", "actor", "action", "target", "outcome"],
        "device_audit 的列清单被冻结：只存标识符与结果"
    );
    for forbidden in [
        "content",
        "text",
        "png",
        "image",
        "ocr",
        "clipboard",
        "payload",
        "bytes",
        "pixel",
        "blob",
        "data",
    ] {
        assert!(
            !audit_columns
                .iter()
                .any(|column| column.contains(forbidden)),
            "device_audit 出现了能装内容的列（关键字 {forbidden}）：审计不许存像素/剪贴板文本/OCR 原文"
        );
    }

    // 会话表的列清单同样冻结（形状是与 hbot-rpc 的契约，改列名要两边一起改）。
    assert_eq!(
        columns(&conn, "device_sessions"),
        vec![
            "id",
            "bot_id",
            "state",
            "sub_json",
            "allowed_apps_json",
            "granted_at",
            "deadline",
            "ended_at",
            "created_at",
        ]
    );

    // 列清单常量与真实表一致（mappers 里的顺序就是 `SELECT` 的顺序）。
    assert_eq!(
        columns(&conn, "device_audit"),
        DEVICE_AUDIT_COLUMNS.split(", ").collect::<Vec<_>>()
    );
    assert_eq!(
        columns(&conn, "device_sessions"),
        DEVICE_SESSION_COLUMNS.split(", ").collect::<Vec<_>>()
    );
}

/// ⑧ `update_device_session_state` 的命中 / 未命中，以及 `ended_at = None` 的清空语义。
#[test]
fn update_device_session_state_reports_hit_and_miss() {
    let dir = TempDir::new("device-update-state");
    let store = open_store(&dir);
    store
        .insert_device_session(new_session("s-1", 1_000))
        .expect("insert session");

    assert!(
        store
            .update_device_session_state("s-1", "expired", Some(7_000))
            .expect("update hit"),
        "命中一行必须返回 true"
    );
    let row = store
        .latest_device_session(&bot_id())
        .expect("latest")
        .expect("row");
    assert_eq!(row.state, "expired");
    assert_eq!(row.ended_at, Some(7_000));
    assert_eq!(row.created_at, 1_000, "只改状态，不许动其它列");
    assert_eq!(row.sub_json, "[\"screen\",\"input\"]");

    // 不存在的 id：返回 false 而不是 Err —— 调用方靠它区分"没这行"和"写失败"。
    assert!(
        !store
            .update_device_session_state("missing", "revoked", Some(1))
            .expect("update miss"),
        "未命中必须返回 false"
    );

    // ended_at = None 表示清空（例如把会话退回未结束态重新走流程）。
    assert!(
        store
            .update_device_session_state("s-1", "requested", None)
            .expect("update clear ended_at"),
        "命中一行必须返回 true"
    );
    let row = store
        .latest_device_session(&bot_id())
        .expect("latest")
        .expect("row");
    assert_eq!(row.state, "requested");
    assert!(row.ended_at.is_none(), "None 必须把 ended_at 清成 NULL");
}

/// ⑨ 存储层的 `active` 字面量与领域契约同值。
///
/// `end_live_device_sessions` 靠 `STATE_ACTIVE` 筛行；值漂了会变成"一条都撤销不掉"的静默失效
/// （比报错难查得多）。存储层不 import 领域枚举，所以这条一致性必须由测试钉住。
#[test]
fn active_state_literal_matches_the_core_contract() {
    assert_eq!(
        hbot_core::DeviceSessionState::Active.as_str(),
        STATE_ACTIVE,
        "STATE_ACTIVE 与 hbot_core 的字面量漂移了：撤销会一条都命中不了"
    );
}
