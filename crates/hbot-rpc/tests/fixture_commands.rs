//! 夹具命令的接缝：`replay_save_fixture` / `fixture_run` 的参数校验、错误码与真实跑通。
//!
//! ## 这个文件测什么、不测什么
//!
//! 测 `Dispatcher::dispatch → commands::replay → hbot_replay::fixture` 这条链的**接缝**：
//!
//! - 没有项目 → `-32002`；
//! - 夹具路径逃出项目 → `-32002`（两条命令都要挡，写文件那条更该挡）；
//! - 存夹具时没有历史入站 → `-32001`；
//! - 存成功 → `events > 0`、返回的绝对路径真实存在、内容就是事件数组；
//! - **`fixture_run` 不查库**：库里连 bot 都没有（更不会有一条历史），夹具照样能跑完；
//! - 夹具坏掉（不是 JSON 数组 / 元素形状不对）→ `-32002`，不是 `-32006`。
//!
//! **不测**夹具格式的细节与 diff 语义（`hbot-replay/tests/fixture.rs` 已经逐条钉住），
//! 也不测 CLI 的打印与退出码。
//!
//! ## 为什么"不查库"值得一条专门的用例
//!
//! `fixture_run` 存在的理由就是**不需要历史**：把别处导出的夹具（CI 里的回归用例）拿来
//! 重跑。如果哪天有人"顺手"在里面补一句 `store.get_bot` 做校验，库里的 bot 一改名或换库，
//! 回归用例就全红了 —— 而夹具里明明带着 `botId`。所以这里故意造一个**库里什么都没有**的
//! 场景：只要它还能跑成功，就说明没人在背后查库。
//!
//! ## 库是哪一份
//!
//! `AppState` 里那份**应用库** `<data_dir>/hbot.db`（与 `replay_commands.rs` 一致）：
//! 存夹具从这份库读历史，这正是 IDE 场景的真相。

use std::path::{Path, PathBuf};
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

/// 临时目录（`Drop` 递归删除）。刻意不引 `tempfile`，与 `replay_commands.rs` 同款。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-fixture-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
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

    /// 写一份能编译的项目（脚手架 + 一句 handler）。
    fn scaffold(&self) {
        hbot_code::manifest::scaffold(&self.project, "fixture-case").expect("脚手架应成功");
        std::fs::write(self.project.join("src").join("main.ts"), REPLY_SOURCE)
            .expect("写 src/main.ts");
    }

    async fn call(&self, command: Command) -> Response {
        self.dispatcher.dispatch(command).await
    }

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
        // Windows 上 `project_root()` 是 canonicalize 之后的（`\\?\C:\...`），
        // 而 `temp_dir()` 给 8.3 短路径 —— 两边都规范化再比。
        let expected = std::fs::canonicalize(&self.project).expect("canonicalize 项目目录");
        assert_eq!(
            self.state.project_root().as_deref(),
            Some(expected.as_path())
        );
    }

    /// 只在库里写 project + bot（**不写**任何历史入站），返回 conversation id。
    fn insert_project_and_bot(&self, bot_id: BotId) -> ConversationId {
        let store = self.state.store();
        let project_id = ProjectId::from_uuid(Uuid::new_v4());
        store
            .insert_project(&Project {
                id: project_id,
                name: "fixture-case".to_owned(),
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
                name: "fixture-bot".to_owned(),
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
        conversation_id
    }

    /// project + bot + 一条历史入站（与它当时的出站回复）。
    fn insert_one_history(&self, bot_id: BotId) {
        let store = self.state.store();
        let conversation_id = self.insert_project_and_bot(bot_id);
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
                OutboundTarget::new(
                    json!({"groupId": GROUP_EXTERNAL_ID}).to_string(),
                    Some("m1".to_owned()),
                ),
            )
            .expect("enqueue_outbound");
    }

    /// 把一份"手写"的夹具写进项目（`rel` 是相对项目根的路径），返回绝对路径。
    fn write_fixture_file(&self, rel: &str, content: &str) -> PathBuf {
        let target = self.project.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("建夹具目录");
        }
        std::fs::write(&target, content).expect("写夹具文件");
        target
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

/// 一条 `event_json` 形状的事件（isolate 收到的那一份），供"没有历史也能跑"的用例。
fn event_value(bot_id: BotId, external_id: &str, text: &str) -> Value {
    json!({
        "botId": bot_id.to_string(),
        "conversationId": Uuid::new_v4().to_string(),
        "messageId": Uuid::new_v4().to_string(),
        "externalId": external_id,
        "senderId": "10086",
        "text": text,
        "kind": "group",
        "conversationExternalId": GROUP_EXTERNAL_ID,
        "receivedAt": AT,
        "payload": serde_json::from_str::<Value>(&payload_json(external_id, text))
            .expect("payload 是合法 JSON"),
    })
}

/// 命令失败且错误码是 `expected`，并返回错误文案（供断言关键词）。
fn assert_error_code(response: Response, expected: i32) -> String {
    match response {
        Response::Error { error } => {
            assert_eq!(
                error.code, expected,
                "错误码不对（期望 {expected}，实际 {}：{}）",
                error.code, error.message
            );
            error.message
        }
        Response::Ok { data } => panic!("期望 {expected}，实际成功：{data:?}"),
    }
}

/// 命令成功，返回 `data`。
fn assert_ok_data(response: Response) -> Value {
    match response {
        Response::Ok { data } => data.expect("成功时 data 必须存在"),
        Response::Error { error } => {
            panic!("期望成功，实际 {}：{}", error.code, error.message)
        }
    }
}

/// ① 两条命令在没有打开项目时都是 `-32002`，文案都指向 `project_open`。
#[tokio::test]
async fn fixture_commands_without_a_project_are_invalid() {
    let harness = Harness::new("no-project");
    let bot_id = BotId::from_uuid(Uuid::new_v4());

    let save_message = assert_error_code(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: None,
                path: ".hbot/fixtures/x.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        save_message.contains("project_open"),
        "存夹具的文案该让调用方先 project_open，实际：{save_message}"
    );

    let run_message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/x.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        run_message.contains("project_open"),
        "跑夹具的文案该让调用方先 project_open，实际：{run_message}"
    );
}

/// ② 夹具路径逃出项目 → `-32002`（两条命令都挡，且不留下半截文件）。
#[tokio::test]
async fn fixture_paths_outside_the_project_are_invalid() {
    let harness = Harness::new("escape");
    harness.scaffold();
    harness.open_project().await;
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    harness.insert_one_history(bot_id);

    // 相对路径用 `..` 跳出去。
    let message = assert_error_code(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: None,
                path: "../escape.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("项目内"),
        "文案该说明夹具必须落在项目内，实际：{message}"
    );

    // 绝对路径直接指到项目外（临时目录的兄弟）。
    let outside = harness._dir.path().join("outside.json");
    let message = assert_error_code(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: None,
                path: outside.display().to_string(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("项目内"),
        "绝对路径越界也该被挡，实际：{message}"
    );
    assert!(
        !outside.exists(),
        "越界路径不该真的写出文件：{}",
        outside.display()
    );

    // 跑夹具同样挡（它读的是同一类路径）。
    assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: outside.display().to_string(),
            })
            .await,
        -32002,
    );

    // 空路径也是 `-32002`，不是"文件不存在"。
    let message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: "   ".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("不能为空"),
        "空路径该直接说清楚，实际：{message}"
    );

    // 项目外**真实存在的、内容合法的**夹具：越界判据必须在"读文件/跑起来"之前生效。
    // 只断言 `-32002` 不够 —— 文件不存在报的也是 `-32002`，那样把越界判据删掉这条测试照样绿。
    let real_outside = harness._dir.path().join("real-outside.json");
    std::fs::write(
        &real_outside,
        serde_json::to_string(&vec![event_value(bot_id, "m1", "你好")]).expect("序列化事件"),
    )
    .expect("写项目外的真夹具");
    let message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: real_outside.display().to_string(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("项目内"),
        "项目外那份能读能跑的夹具也必须挡在「项目内」判据上，实际：{message}"
    );
}

/// ③ 库里有这个 bot 但一条历史都没有 → `-32001`（不是 `-32006`）。
#[tokio::test]
async fn saving_without_history_is_not_found() {
    let harness = Harness::new("no-history");
    harness.scaffold();
    harness.open_project().await;
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    harness.insert_project_and_bot(bot_id);

    let message = assert_error_code(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: None,
                path: ".hbot/fixtures/empty.json".to_owned(),
            })
            .await,
        -32001,
    );
    assert!(
        message.contains(&bot_id.to_string()),
        "文案该带上 bot id，实际：{message}"
    );
    assert!(
        !harness.project.join(".hbot/fixtures/empty.json").exists(),
        "没有历史时不该留下一个空夹具文件"
    );

    // `limit = 0` 是参数错（`-32002`），而且比"没有历史"更早判。
    let message = assert_error_code(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: Some(0),
                path: ".hbot/fixtures/zero.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("limit"),
        "文案该提到 limit，实际：{message}"
    );
}

/// ④ 保存成功后：`events > 0`、返回的绝对路径真的存在、内容就是事件数组；
///    紧接着用 `fixture_run` 把它跑一遍（端到端：存 → 跑）。
#[tokio::test]
async fn saving_then_running_a_fixture_round_trips() {
    let harness = Harness::new("round-trip");
    harness.scaffold();
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    harness.insert_one_history(bot_id);
    harness.open_project().await;

    let saved = assert_ok_data(
        harness
            .call(Command::ReplaySaveFixture {
                bot_id,
                from: None,
                limit: None,
                path: ".hbot/fixtures/m1.json".to_owned(),
            })
            .await,
    );
    assert_eq!(saved["events"], json!(1), "一条历史 → events 1：{saved}");
    let saved_path = saved["path"].as_str().expect("path 是字符串").to_owned();
    assert!(
        !saved_path.starts_with(r"\\?\"),
        "回显的路径不该带 Windows 的 \\\\?\\ 前缀：{saved_path}"
    );
    let on_disk = Path::new(&saved_path);
    assert!(on_disk.is_file(), "返回的路径必须真的存在：{saved_path}");

    let content: Value = serde_json::from_str(&std::fs::read_to_string(on_disk).expect("读夹具"))
        .expect("合法 JSON");
    let events = content.as_array().expect("夹具顶层是 JSON 数组");
    assert_eq!(events.len(), 1, "夹具里应当只有 1 条事件：{content}");
    assert_eq!(events[0]["externalId"], json!("m1"));
    assert_eq!(events[0]["text"], json!("你好"));

    // 用相对路径跑（证明相对路径按项目根解析，而不是按进程 CWD）。
    let report = assert_ok_data(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/m1.json".to_owned(),
            })
            .await,
    );
    assert_eq!(report["events"], json!(1), "events：{report}");
    assert_eq!(report["replies"], json!(1), "一条回复：{report}");
    assert_eq!(report["errors"], json!(0), "不该有异常：{report}");
    assert_eq!(report["entries"][0]["index"], json!(0));
    assert_eq!(report["entries"][0]["externalId"], json!("m1"));
    assert_eq!(report["entries"][0]["text"], json!("你好"));
    assert_eq!(report["entries"][0]["replies"], json!(["pong:你好"]));
    assert_eq!(report["entries"][0]["error"], json!(null));
}

/// ⑤ **`fixture_run` 不查库**：库里连 bot 都没有（更不会有一条历史），夹具照样跑完。
///
/// 这条用例就是这条命令存在的理由 —— 谁在里面偷偷加一句 `store.get_bot`，这里就红。
#[tokio::test]
async fn running_a_fixture_does_not_look_at_the_store() {
    let harness = Harness::new("no-store");
    harness.scaffold();
    harness.open_project().await;
    // 刻意不写任何行：`Project` / `Bot` / `Conversation` / `Inbound` 一条都没有。
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    let fixture = json!([
        event_value(bot_id, "m1", "你好"),
        event_value(bot_id, "m2", "世界"),
    ]);
    harness.write_fixture_file(".hbot/fixtures/two.json", &fixture.to_string());

    let report = assert_ok_data(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/two.json".to_owned(),
            })
            .await,
    );
    assert_eq!(report["events"], json!(2), "两条事件：{report}");
    assert_eq!(report["errors"], json!(0), "不该有异常：{report}");
    assert_eq!(report["replies"], json!(2));
    assert_eq!(
        report["entries"][1]["replies"],
        json!(["pong:世界"]),
        "第二条也要真的跑：{report}"
    );

    // 换个角度确认：库里确实没有这个 bot（否则上面的成功可能另有原因）。
    assert!(
        harness
            .state
            .store()
            .get_bot(&bot_id)
            .expect("查 bot")
            .is_none(),
        "这条用例的前提是库里没有这个 bot"
    );
}

/// ⑥ 夹具坏掉 → `-32002`：不是 JSON 数组、元素缺字段、文件不存在，三种都算"夹具不对"。
#[tokio::test]
async fn broken_fixtures_are_invalid() {
    let harness = Harness::new("broken");
    harness.scaffold();
    harness.open_project().await;
    let bot_id = BotId::from_uuid(Uuid::new_v4());

    // 不是数组（对象包一层）—— 这是最容易写错的一种。
    harness.write_fixture_file(".hbot/fixtures/object.json", r#"{"version":1,"events":[]}"#);
    let message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/object.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("JSON 数组"),
        "文案该说清顶层必须是数组，实际：{message}"
    );

    // 数组里混了一条缺字段的：整份夹具都不跑（不静默跳过）。
    let good = event_value(bot_id, "m1", "你好");
    let mut broken = event_value(bot_id, "m2", "世界");
    broken.as_object_mut().expect("对象").remove("payload");
    harness.write_fixture_file(
        ".hbot/fixtures/mixed.json",
        &json!([good.clone(), broken]).to_string(),
    );
    let message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/mixed.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("第 2 条"),
        "文案该指出是哪一条坏了，实际：{message}"
    );

    // 文件根本不存在。
    let message = assert_error_code(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/missing.json".to_owned(),
            })
            .await,
        -32002,
    );
    assert!(
        message.contains("读不了夹具文件"),
        "文案该说清是读不了文件，实际：{message}"
    );

    // 补一条：`good` 单独是能跑的（证明上面的失败来自 `broken`，不是 `good` 有问题）。
    harness.write_fixture_file(".hbot/fixtures/good.json", &json!([good]).to_string());
    let report = assert_ok_data(
        harness
            .call(Command::FixtureRun {
                bot_id,
                path: ".hbot/fixtures/good.json".to_owned(),
            })
            .await,
    );
    assert_eq!(report["entries"][0]["replies"], json!(["pong:你好"]));
}
