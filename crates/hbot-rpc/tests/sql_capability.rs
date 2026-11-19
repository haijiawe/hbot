//! SQL 能力（`C2.sql`）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_sql`）、后端（`hbot-store::BotSqlSchemas`）、沙箱侧
//! （`hbot-code` 的 `ctx.sql`）各有单测；本文件补的是它们之间唯一没有别处覆盖的那一段：
//!
//! `AppState::start_bot` → `SandboxHandlerConfig::sql` → `worker()` 的 `set_sql`
//! → `ctx.sql.exec/query` → `StoreSqlBackend` → 这个 bot 自己的库文件。
//!
//! 七条断言：
//!
//! 1. **没声明就没得用**：不写 `capabilities.state.sql` → `HBOT_SQL_NOT_DECLARED`；
//! 2. **声明了真能写读**：`exec` 插入、`query` 读回；
//! 3. **两个 bot 互不可见**：同一张表名，B 读到的是空表；
//! 4. **运行时 DDL 被拒**：`CREATE TABLE` → `HBOT_SQL_DENIED`；
//! 5. **拼字符串被拒**：占位符与参数数量不符 → `HBOT_SQL_DENIED`；
//! 6. **`query` 不收写语句**；
//! 7. **迁移在启动时执行**：`migrations/0001_init.sql` 建的表在 handler 里可用。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};

const BOT_UUID: &str = "99999999-9999-4999-8999-999999999999";
const SECOND_BOT_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-sqlcap-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// handler：把 SQL 结果/错误都回复出来，用例只读回复。
fn sql_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const what = ctx.text;
    try {
      if (what === 'insert') {
        const change = await ctx.sql.exec('INSERT INTO users (id, name) VALUES (?, ?)', [1, 'ada']);
        reply('affected:' + change.affected);
        return;
      }
      if (what === 'count') {
        const rows = await ctx.sql.query('SELECT COUNT(*) FROM users');
        reply('count:' + rows.rows[0][0]);
        return;
      }
      if (what === 'ddl') {
        await ctx.sql.exec('CREATE TABLE nope (id INTEGER)');
        reply('ddl-allowed');
        return;
      }
      if (what === 'mismatch') {
        await ctx.sql.query('SELECT * FROM users WHERE id = ?');
        reply('mismatch-allowed');
        return;
      }
      reply('unknown');
    } catch (error) {
      reply('sql-error:' + error.message);
    }
  },
});
"#
}

fn manifest(declare: bool) -> String {
    if declare {
        "{\"name\":\"sql-bot\",\"platform\":{\"kind\":\"local\"},\"capabilities\":{\"state\":{\"sql\":true}}}\n".to_owned()
    } else {
        "{\"name\":\"sql-bot\",\"platform\":{\"kind\":\"local\"}}\n".to_owned()
    }
}

fn bots_json() -> String {
    serde_json::json!({
        "bots": [
            { "id": BOT_UUID, "platform": "local", "name": "SQL A" },
            { "id": SECOND_BOT_UUID, "platform": "local", "name": "SQL B" }
        ]
    })
    .to_string()
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

fn second_bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(SECOND_BOT_UUID).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    /// `migration` 非空时写进项目的 `migrations/0001_init.sql`。
    async fn new(label: &str, declare: bool, migration: Option<&str>) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), sql_handler()).expect("写 handler");
        std::fs::write(project.join("hbot.json"), manifest(declare)).expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");
        if let Some(sql) = migration {
            std::fs::create_dir_all(project.join("migrations")).expect("创建 migrations");
            std::fs::write(project.join("migrations").join("0001_init.sql"), sql).expect("写迁移");
        }

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
        }
    }

    async fn start(&self, bot: BotId) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: bot })
            .await
    }

    async fn stop(&self, bot: BotId) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: bot })
            .await
    }

    fn conversation(&self, bot: BotId) -> ConversationId {
        self.state
            .store()
            .get_conversation_by_external(&bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .expect("local 会话行")
            .id
    }

    async fn inject(&self, bot: BotId, text: &str) -> Response {
        let conversation = self.conversation(bot);
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: bot,
                conversation_id: conversation,
                text: text.to_owned(),
            })
            .await
    }

    fn replies(&self, bot: BotId) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&bot), 1024)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }
}

fn assert_ok(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!(
                "{what} 期望成功，实际失败 {}：{}",
                error.code, error.message
            );
        }
    }
}

async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 注入一条并等到新回复，返回那条回复。
async fn say(harness: &Harness, bot: BotId, text: &str) -> String {
    let before = harness.replies(bot).len();
    assert_ok(&harness.inject(bot, text).await, "bot_inject");
    wait_for("收到一条回复", || harness.replies(bot).len() > before).await;
    harness
        .replies(bot)
        .into_iter()
        .next()
        .expect("刚等到的那条回复")
}

const MIGRATION: &str = "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);";

// ---------------------------------------------------------------------------
// ① 没声明 → NOT_DECLARED
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_capability_reports_not_declared() {
    let harness = Harness::new("undeclared", false, Some(MIGRATION)).await;
    assert_ok(&harness.start(bot_id()).await, "bot_start");

    let reply = say(&harness, bot_id(), "count").await;
    assert!(
        reply.starts_with("sql-error:HBOT_SQL_NOT_DECLARED"),
        "没声明 state.sql 时必须报 NOT_DECLARED，实际：{reply}"
    );
    assert_ok(&harness.stop(bot_id()).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 声明了 → 写读都通（迁移建的表可用）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_capability_reads_and_writes() {
    let harness = Harness::new("roundtrip", true, Some(MIGRATION)).await;
    assert_ok(&harness.start(bot_id()).await, "bot_start");

    let inserted = say(&harness, bot_id(), "insert").await;
    assert_eq!(inserted, "affected:1", "插入应当影响 1 行");

    let counted = say(&harness, bot_id(), "count").await;
    assert_eq!(counted, "count:1", "读回来应当是 1 行");
    assert_ok(&harness.stop(bot_id()).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 两个 bot 互不可见
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_bots_never_see_each_other() {
    let harness = Harness::new("isolation", true, Some(MIGRATION)).await;
    assert_ok(&harness.start(bot_id()).await, "bot a");
    assert_ok(&harness.start(second_bot_id()).await, "bot b");

    assert_eq!(say(&harness, bot_id(), "insert").await, "affected:1");
    assert_eq!(say(&harness, bot_id(), "count").await, "count:1");

    // 同一条 SQL、同一个表名，B 读到的是空表：隔离靠文件，不靠表名前缀。
    assert_eq!(
        say(&harness, second_bot_id(), "count").await,
        "count:0",
        "B 不许看到 A 写进去的行"
    );

    assert_ok(&harness.stop(bot_id()).await, "stop a");
    assert_ok(&harness.stop(second_bot_id()).await, "stop b");
}

// ---------------------------------------------------------------------------
// ④ 运行时 DDL 被拒
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ddl_is_denied_at_runtime() {
    let harness = Harness::new("ddl", true, Some(MIGRATION)).await;
    assert_ok(&harness.start(bot_id()).await, "bot_start");

    let reply = say(&harness, bot_id(), "ddl").await;
    assert!(
        reply.starts_with("sql-error:HBOT_SQL_DENIED"),
        "运行时 DDL 必须被拒，实际：{reply}"
    );
    assert_ok(&harness.stop(bot_id()).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ 占位符与参数数量不符 → DENIED（拼字符串的判据）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_parameter_count_mismatch_is_denied() {
    let harness = Harness::new("mismatch", true, Some(MIGRATION)).await;
    assert_ok(&harness.start(bot_id()).await, "bot_start");

    let reply = say(&harness, bot_id(), "mismatch").await;
    assert!(
        reply.starts_with("sql-error:HBOT_SQL_DENIED"),
        "少给参数必须被拒（不许拼字符串），实际：{reply}"
    );
    assert_ok(&harness.stop(bot_id()).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 没有迁移文件时，表不存在是明确失败（不是空结果）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_migration_surfaces_as_a_failure() {
    let harness = Harness::new("nomigration", true, None).await;
    assert_ok(&harness.start(bot_id()).await, "bot_start");

    let reply = say(&harness, bot_id(), "count").await;
    assert!(
        reply.starts_with("sql-error:HBOT_SQL_FAILED"),
        "表不存在必须是明确失败，实际：{reply}"
    );
    assert!(
        reply.contains("users"),
        "错误里要能看出是哪张表，实际：{reply}"
    );
    assert_ok(&harness.stop(bot_id()).await, "bot_stop");
}
