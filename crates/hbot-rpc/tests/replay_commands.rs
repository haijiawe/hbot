//! `replay_run` 的命令级行为：参数校验、错误码、以及"真有历史入站时返回条目"。
//!
//! ## 这个文件测什么、不测什么
//!
//! 测的是 `Dispatcher::dispatch → commands::replay → hbot_replay::replay` 这条链的**接缝**：
//! `limit = 0` 与"没有项目"的 `-32002`、bot 不在库里的 `-32001`、以及一次真实的
//! "编译 + 逐条 dispatch + 比对"跑完之后 wire 报告的形状（`entries` 条数与 `events` 一致、
//! `status` 用了开放字符串 `match`）。
//!
//! **不测** diff 算法本身（`hbot-replay` 的单测已经用 O(n²)+LIS 的用例钉住了），也不测
//! CLI 的打印与退出码（`hbot-cli/tests/replay.rs` 钉住）。
//!
//! ## 为什么要真的编译一次
//!
//! 回放的核心价值就是"用**当前**代码重跑"，所以第三条用例确实走
//! `compile_project_with_entry` + `Sandbox::spawn`：只有真跑一遍，才能证明"报告里的
//! `events`/`entries` 不是凭空造的"。代价是这个用例比其它三条慢（几秒），但把它降级成
//! "只测形状"就等于放弃了这条链唯一有价值的部分。
//!
//! ## 库是哪一份
//!
//! 用的是 `AppState` 里那份**应用库** `<data_dir>/hbot.db`（不是项目目录里的
//! `.hbot/hbot.db`）：入站/出站行直接写进 `state.store()`，这正是 IDE 场景的真相
//! ——桌面进程的 bot 由 `AppState` 起，历史只在应用库里。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId, InboundId, OutboundId, ProjectId};
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::{AppState, Command, Dispatcher, Response};
use hbot_store::{Bot, Conversation, NewInbound, NewOutbound, OutboundTarget, Project};
use serde_json::{Value, json};
use uuid::Uuid;

/// 时间基准：与其它 rpc 测试同一刻度。
const AT: i64 = 1_700_000_000_000;

/// 会话的外部 id（群号）。
const GROUP_EXTERNAL_ID: &str = "30001";

/// 与脚手架默认模板同款的最小 bot：入站文本 → 一条被动回复。
const REPLY_SOURCE: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`pong:${ctx.text}`);
  },
});
"#;

/// 临时目录（`Drop` 递归删除）。刻意不引 `tempfile`，与 `debug_commands.rs` 同款。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-replay-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 装配好的 [`AppState`] + [`Dispatcher`]，外加一个已经脚手架化的项目目录。
struct Harness {
    /// 只为了在 `Harness` 存活期间保住临时目录（`TempDir` 的 `Drop` 负责删）。
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    project: PathBuf,
}

impl Harness {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let state = Arc::new(
            AppState::open(
                dir.path().join("data"),
                Arc::new(SystemClock),
                Arc::new(UuidIds),
            )
            .expect("AppState::open"),
        );
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        let project = dir.path().join("project");
        Self {
            _dir: dir,
            state,
            dispatcher,
            project,
        }
    }

    /// 写一份能编译的项目（脚手架 + 一句 handler），但**不**打开它。
    fn scaffold(&self) {
        hbot_code::manifest::scaffold(&self.project, "replay-fixture").expect("脚手架应成功");
        std::fs::write(self.project.join("src").join("main.ts"), REPLY_SOURCE)
            .expect("写 src/main.ts");
    }

    async fn call(&self, command: Command) -> Response {
        self.dispatcher.dispatch(command).await
    }

    /// 打开项目（不登记 `projects` 行，行由 [`Harness::insert_rows`] 自己写，好让
    /// 归属校验的路径与 bot 的 `project_id` 对得上）。
    async fn open_project(&self) {
        let response = self
            .call(Command::ProjectOpen {
                path: self.project.display().to_string(),
            })
            .await;
        assert!(
            matches!(response, Response::Ok { .. }),
            "project_open 应该成功：{response:?}"
        );
        // Windows 上 `open_project` 存的是 canonicalize 之后的路径（`\\?\C:\...`），
        // 而 `std::env::temp_dir()` 给的是 8.3 短路径（`ADMINI~1`）——两边都规范化再比。
        let expected = std::fs::canonicalize(&self.project).expect("canonicalize 项目目录");
        assert_eq!(
            self.state.project_root().as_deref(),
            Some(expected.as_path())
        );
    }

    /// 在库里写 project + bot + conversation + 一条入站（与它当时的出站）。
    fn insert_rows(&self, bot_id: BotId) {
        let store = self.state.store();
        let project_id = ProjectId::from_uuid(Uuid::new_v4());
        store
            .insert_project(&Project {
                id: project_id,
                name: "replay-fixture".to_owned(),
                // 与 `project_root()` 同一写法（canonicalize），让归属校验真的命中这一行。
                path: std::fs::canonicalize(&self.project)
                    .expect("canonicalize 项目目录")
                    .to_string_lossy()
                    .into_owned(),
                created_at: AT,
            })
            .expect("insert_project");
        store
            .insert_bot(&Bot {
                id: bot_id,
                project_id,
                name: "replay-bot".to_owned(),
                platform: "onebot11".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: AT,
            })
            .expect("insert_bot");
        let conversation_id = ConversationId::from_uuid(Uuid::new_v4());
        store
            .insert_conversation(&Conversation {
                id: conversation_id,
                bot_id,
                external_id: GROUP_EXTERNAL_ID.to_owned(),
                kind: "group".to_owned(),
                created_at: AT,
            })
            .expect("insert_conversation");
        store
            .insert_inbound_with_sender(
                NewInbound {
                    id: InboundId::from_uuid(Uuid::new_v4()),
                    bot_id,
                    external_id: "m1".to_owned(),
                    conversation_id,
                    payload_json: payload_json("m1", "你好"),
                    received_at: AT,
                },
                "10086".to_owned(),
            )
            .expect("insert_inbound");
        store
            .enqueue_outbound_with_target(
                NewOutbound {
                    id: OutboundId::from_uuid(Uuid::new_v4()),
                    bot_id,
                    conversation_id,
                    text: "pong:你好".to_owned(),
                    created_at: AT + 1,
                },
                // 被动回复：`reply_to` = 入站 external_id，这是回放归属的唯一外键。
                OutboundTarget::new(
                    json!({"groupId": GROUP_EXTERNAL_ID}).to_string(),
                    Some("m1".to_owned()),
                ),
            )
            .expect("enqueue_outbound");
    }
}

/// 一份 OneBot 11 群消息的归一化载荷（`hbot-adapters` 落库的就是这个形状）。
fn payload_json(external_id: &str, text: &str) -> String {
    json!({
        "text": text,
        "senderId": "10086",
        "conversationExternalId": GROUP_EXTERNAL_ID,
        "kind": "group",
        "messageId": external_id,
        "replyTo": null,
        "botPlatformId": "20000",
        "raw": {
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "message_id": external_id.parse::<i64>().unwrap_or_default(),
            "group_id": GROUP_EXTERNAL_ID.parse::<i64>().unwrap_or_default(),
            "user_id": 10086,
            "raw_message": text,
            "message": [{"type": "text", "data": {"text": text}}],
            "sender": {"user_id": 10086, "nickname": "测试"},
        },
    })
    .to_string()
}

/// 命令失败且错误码是 `expected`，并返回错误文案（供断言关键词）。
fn assert_error_code(response: Response, expected: i32) -> String {
    match response {
        Response::Error { error } => {
            assert_eq!(
                error.code, expected,
                "错误码不对（期望 -{expected}，实际 -{}：{}）",
                error.code, error.message
            );
            error.message
        }
        Response::Ok { data } => panic!("期望 -{expected}，实际成功：{data:?}"),
    }
}

/// 命令成功，返回 `data`（调用方自己取字段）。
fn assert_ok_data(response: Response) -> Value {
    match response {
        Response::Ok { data } => data.expect("replay_run 成功时 data 必须是报告对象"),
        Response::Error { error } => {
            panic!("期望成功，实际 -{}：{}", error.code, error.message)
        }
    }
}

/// ① 没有打开项目 → `-32002`（提示先 `project_open`）。
#[tokio::test]
async fn replay_without_a_project_is_invalid() {
    let harness = Harness::new("no-project");
    let message = assert_error_code(
        harness
            .call(Command::ReplayRun {
                bot_id: BotId::from_uuid(Uuid::new_v4()),
                from: None,
                limit: None,
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("project_open"),
        "文案该告诉调用方先打开项目，实际：{message}"
    );
}

/// ② 项目开着，但库里没有这个 bot → `-32001`（不是 `-32006`）。
#[tokio::test]
async fn replay_with_an_unknown_bot_is_not_found() {
    let harness = Harness::new("unknown-bot");
    harness.scaffold();
    harness.open_project().await;
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    let message = assert_error_code(
        harness
            .call(Command::ReplayRun {
                bot_id,
                from: None,
                limit: None,
            })
            .await,
        -32001,
    );
    assert!(
        message.contains(&bot_id.to_string()),
        "文案该带上 bot id，实际：{message}"
    );
}

/// ③ 有历史入站时真的编译 + 重跑一遍，报告里的 `entries` 与 `events` 一致。
///
/// 这一条是全文件的重点：它证明"报告不是凭空造的"——真的编译了 `src/main.ts`、真的
/// dispatch 了一条事件、真的比对出 `match`。
#[tokio::test]
async fn replay_with_history_reports_one_matching_entry() {
    let harness = Harness::new("history");
    harness.scaffold();
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    harness.insert_rows(bot_id);
    harness.open_project().await;

    let report = assert_ok_data(
        harness
            .call(Command::ReplayRun {
                bot_id,
                from: None,
                limit: None,
            })
            .await,
    );
    assert_eq!(
        report["events"],
        json!(1),
        "一条历史入站 → events 1：{report}"
    );
    assert_eq!(
        report["entries"].as_array().map(Vec::len),
        Some(1),
        "entries 条数必须与 events 一致：{report}"
    );
    assert_eq!(report["entries"][0]["externalId"], json!("m1"));
    // `status` 是开放字符串（不是枚举）：历史回复与重跑回复都是 `pong:你好`。
    assert_eq!(
        report["entries"][0]["status"],
        json!("match"),
        "同一份代码重跑应当一致：{report}"
    );
    assert_eq!(report["entries"][0]["historical"], json!(["pong:你好"]));
    assert_eq!(report["entries"][0]["replayed"], json!(["pong:你好"]));
    assert_eq!(report["bot"], json!(bot_id.to_string()));
    assert_eq!(report["limit"], json!(null), "没给 limit = 全量");
}

/// ④ `limit = 0` → `-32002`，文案说明"要全量就别给 limit"。
#[tokio::test]
async fn replay_with_zero_limit_is_invalid() {
    let harness = Harness::new("zero-limit");
    harness.scaffold();
    harness.open_project().await;
    let message = assert_error_code(
        harness
            .call(Command::ReplayRun {
                bot_id: BotId::from_uuid(Uuid::new_v4()),
                from: None,
                limit: Some(0),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("limit"),
        "文案该提到 limit，实际：{message}"
    );
    assert!(
        message.contains("全量"),
        "文案该告诉调用方要全量就别给 limit，实际：{message}"
    );
}
