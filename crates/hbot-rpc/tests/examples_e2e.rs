//! 仓库自带的示例项目（`examples/`）在**生产装配路径**上的端到端测试。
//!
//! `crates/hbot-cli/tests/examples.rs` 只证明三个示例「清单合法、能编译、d.ts 与核心一致」；
//! 它不证明「打开 → 发消息 → 收到回复」这条真实链路。M7 的出口标准要的正是后者，所以这里把
//! 仓库里的 `examples/game` 与 `examples/chat` 原样复制进临时项目，走
//! `AppState::open → open_project → bot_start → bot_inject → dispatch → reply`。
//!
//! 三条断言刻意只写「回复内容」，**不写耗时**：耗时断言在并行负载下会擦边（见 hot_reload 那条先例）。
//!
//! 1. **game**（`C2.sql` + `onSchedule`）：`/score 120` 落库并回 `记下了：120`，随后 `/top` 读得到 120
//!    —— 这同时证明 `migrations/0001_init.sql` 真的执行过（没有表，INSERT 会被拒）。
//! 2. **chat**（`kv` + `history` + `ai`）：`/count` 只走 kv，回 `你已经说过 1 句了。`；
//!    `/count` 在调 `ctx.ai` 之前就 return，所以这条**不需要模型 key**。
//! 3. **support**（`session` + `sql`）：`/ticket` 注册一条等待并立刻回提示；下一条消息被
//!    `claim_wait` 拦下走 `onResume`（**不再**进 `onMessage`），落库后回「已记录」。这条钉的是
//!    「等待是注册不是阻塞」这条设计决定在真示例上成立 —— 如果 `prompt` 改成阻塞，这里会超时。

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
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-examples-e2e-{label}-{}-{seq}",
            std::process::id()
        ));
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

/// 仓库根（`CARGO_MANIFEST_DIR` 是 `crates/hbot-rpc`，往上两级）。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("仓库根")
        .to_path_buf()
}

/// 把示例项目整棵复制过去。示例刻意**不带** `bots.json`（里面会有平台凭据），
/// 由测试自己写一份只有 local 平台的。
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("创建目标目录");
    for entry in std::fs::read_dir(from).expect("读示例目录") {
        let entry = entry.expect("目录项");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("文件类型").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("复制文件");
        }
    }
}

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"示例\"}}]}}")
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    data_dir: PathBuf,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    /// `example` 是 `examples/` 下的目录名（`chat` / `game` / `support`）。
    async fn from_example(label: &str, example: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        copy_dir(&repo_root().join("examples").join(example), &project);
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir.clone(), Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            data_dir,
            state,
            dispatcher,
            bot: bot_id(),
        }
    }

    fn store(&self) -> &Arc<hbot_store::Store> {
        self.state.store()
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: self.bot })
            .await
    }

    async fn stop(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: self.bot })
            .await
    }

    fn conversation(&self) -> ConversationId {
        self.store()
            .get_conversation_by_external(&self.bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .expect("local 会话行")
            .id
    }

    async fn inject(&self, text: &str) -> Response {
        let conversation = self.conversation();
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: self.bot,
                conversation_id: conversation,
                text: text.to_owned(),
            })
            .await
    }

    /// 发一条消息并等一条**此前没出现过**的回复（返回那条回复的全文）。
    ///
    /// 刻意按「文本出现次数」比较而不是按数组下标：`list_logs` 的排序方向不属于本
    /// 测试的判据（它是另一条测试的事），按下标取会把上一轮的回复当成这一轮的。
    async fn ask(&self, text: &str) -> String {
        let before = self.replies();
        assert_ok(&self.inject(text).await, "bot_inject");
        for _ in 0..240 {
            if let Some(fresh) = first_new_reply(&before, &self.replies()) {
                return fresh;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "等不到 {text} 的回复；当前日志：{:?}",
            self.logs()
                .into_iter()
                .map(|row| row.line)
                .collect::<Vec<_>>()
        )
    }

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
    }

    fn replies(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }
}

/// `current` 里第一次出现、而 `previous` 里次数还不够的那条回复。
fn first_new_reply(previous: &[String], current: &[String]) -> Option<String> {
    for text in current {
        let seen_before = previous.iter().filter(|old| *old == text).count();
        let seen_now = current.iter().filter(|new| *new == text).count();
        if seen_now > seen_before {
            return Some(text.clone());
        }
    }
    None
}

fn assert_ok(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!(
                "{what} 期望成功，实际失败 {}：{}",
                error.code, error.message
            )
        }
    }
}

// ---------------------------------------------------------------------------
// game：sql 迁移 + 参数化语句 + 读回来
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_game_example_scores_and_reads_back_through_its_migration() {
    let harness = Harness::from_example("game", "game").await;
    assert_ok(&harness.start().await, "bot_start");

    let reply = harness.ask("/score 120").await;
    assert_eq!(reply, "记下了：120", "写入路径的回复不对");

    let top = harness.ask("/top").await;
    assert!(
        top.contains("120"),
        "读回路径必须看到刚写的 120（看不到说明迁移没执行或 INSERT 没落库）：{top}"
    );

    // 每 bot 一个库文件；`/top` 能读回来说明 `scores` 表已经由 0001_init.sql 建好。
    let db = harness.state.bot_sql().path_for(&harness.bot);
    assert!(db.is_file(), "bot_sql 库文件不存在：{}", db.display());
    let size = std::fs::metadata(&db).expect("读库文件元数据").len();
    assert!(size > 0, "库文件是空的：{}", db.display());
    let _ = &harness.data_dir;

    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// chat：kv 计数（不碰模型）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_chat_example_counts_without_calling_the_model() {
    let harness = Harness::from_example("chat", "chat").await;
    assert_ok(&harness.start().await, "bot_start");

    // `/count` 在 `ctx.ai.complete` 之前就 return，所以这条测试不依赖模型 key。
    let first = harness.ask("/count").await;
    assert_eq!(first, "你已经说过 1 句了。", "第一次计数不对");

    let second = harness.ask("/count").await;
    assert_eq!(second, "你已经说过 2 句了。", "计数没有累加（kv 没落库？）");

    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// support：session 等待（注册式）+ sql 落库
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_support_example_resumes_a_wait_and_records_the_ticket() {
    let harness = Harness::from_example("support", "support").await;
    assert_ok(&harness.start().await, "bot_start");

    // `ctx.session.prompt` 是「注册」不是「阻塞」：handler 立刻返回，而提示
    // 必须在注册成功的那一刻就发出去 —— 否则用户看到的只是一条空回复。
    let prompt = harness.ask("/ticket").await;
    assert!(
        prompt.contains("请一句话描述你遇到的问题"),
        "session.prompt 应该把提示发出来：{prompt}"
    );

    // 下一条消息属于那条等待：它走 onResume，而不是再进 onMessage。
    let resumed = harness.ask("打印机卡纸了").await;
    assert!(
        resumed.starts_with("已记录（编号 "),
        "onResume 应该落库并回执（没走恢复路径？）：{resumed}"
    );

    assert_ok(&harness.stop().await, "bot_stop");
}
