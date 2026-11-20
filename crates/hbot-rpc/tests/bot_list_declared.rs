//! `bot_list` 的集成测试：**试跑面板必须能看见「已声明、还没启动过」的 bot**。
//!
//! 背景（2026-11-21 桌面验收时发现）：`project_open` 只登记项目、不建 `bots` 行，
//! 真正的补建发生在 `bot_start` 的 `bootstrap_bot`。而 `bot_list` 原先只读 `bots` 表，
//! 于是「装好软件 → 打开全新项目」时列表是空的 —— 用户没有任何可点启动的对象，
//! 也就没有入口把自己的 bot 跑起来（`bots.json` 是他手写的唯一声明处）。
//!
//! 这里钉住三件事：
//!
//! 1. 库里 0 行、但 `bots.json` 声明了一个 → `bot_list` 返回 1 条（状态 `stopped`、
//!    `updatedAt` 为 0，因为从没启动过）；
//! 2. `bot_start` 之后仍是 **1 条**（库里那行与声明是同一个 id，不许重复出现）；
//! 3. `bot_stop` 之后 1 条、状态回到 `stopped`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{BotSummary, Command, Response};

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-botlist-{label}-{}-{seq}", std::process::id()));
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

fn echo_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`收到：${ctx.text}`);
  },
});
"#
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), echo_handler()).expect("写 handler");
        // 只写 `bots.json`：项目打开后库里**没有**任何 bot 行 —— 这正是本测试要覆盖的状态。
        let bots_json =
            format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"本地试跑"}}]}}"#);
        std::fs::write(project.join("bots.json"), bots_json).expect("写 bots.json");

        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
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

    fn bot(&self) -> hbot_core::BotId {
        hbot_core::BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("uuid"))
    }

    async fn list(&self) -> Vec<BotSummary> {
        let response = self.dispatcher.dispatch(Command::BotList).await;
        match response {
            Response::Ok { data } => {
                let data = data.expect("bot_list 应当有 data（空列表也是 []）");
                serde_json::from_value(data).expect("BotSummary 数组")
            }
            other => panic!("bot_list 失败了：{other:?}"),
        }
    }
}

/// 库里 0 行时也必须列出 `bots.json` 里的声明 —— 否则试跑面板没有任何入口。
#[tokio::test]
async fn bot_list_surfaces_a_declared_bot_before_it_is_ever_started() {
    let harness = Harness::new("declared").await;

    // 前提：库里的 bot 行确实是 0（不是夹具顺带建出来的）。
    let rows = harness.state.store().list_bots().expect("读 bots 表");
    assert!(rows.is_empty(), "刚打开项目时库里不该有 bot 行：{rows:?}");

    let listed = harness.list().await;
    assert_eq!(listed.len(), 1, "声明的 bot 必须出现在列表里：{listed:?}");
    assert_eq!(listed[0].bot_id, harness.bot());
    assert_eq!(listed[0].name, "本地试跑");
    assert_eq!(listed[0].platform, "local");
    assert_eq!(
        listed[0].status,
        hbot_core::BotStatus::Stopped,
        "还没启动过就是停着"
    );
    assert_eq!(listed[0].updated_at.unix_millis(), 0, "从没变更过状态");
}

/// 启动之后库里那行与声明是同一个 id：列表仍是 1 条，状态变 `running`。
#[tokio::test]
async fn starting_the_bot_does_not_duplicate_the_declared_entry() {
    let harness = Harness::new("start").await;
    let bot = harness.bot();

    let started = harness
        .dispatcher
        .dispatch(Command::BotStart { bot_id: bot })
        .await;
    assert!(
        !matches!(started, Response::Error { .. }),
        "bot_start 失败：{started:?}"
    );

    let listed = harness.list().await;
    assert_eq!(listed.len(), 1, "声明与库里的行不许重复出现：{listed:?}");
    assert_eq!(listed[0].bot_id, bot);
    assert_eq!(listed[0].status, hbot_core::BotStatus::Running);

    let stopped = harness
        .dispatcher
        .dispatch(Command::BotStop { bot_id: bot })
        .await;
    assert!(
        !matches!(stopped, Response::Error { .. }),
        "bot_stop 失败：{stopped:?}"
    );

    let listed = harness.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, hbot_core::BotStatus::Stopped);
    assert!(
        listed[0].updated_at.unix_millis() > 0,
        "启动过一次之后状态时间戳应当有值"
    );
}
