//! 夹具（fixture）的端到端行为：存下来 → 读回形状一致 → 用当前代码重跑。
//!
//! ## 测什么
//!
//! 1. **存下来的就是事件**：`save_fixture` 落的是 `hbot_bot::event_json` 形状的 JSON 数组，
//!    能被 `run_fixture` 原样喂回 isolate（不是"只给人看"的 payload 数组）；
//! 2. **`limit` 截断**：存的是截断后的那一批，条数与文件里的元素数一致；
//! 3. **坏元素不静默跳过**：夹具里只要有一条不是事件对象，整份就跑不起来（`Err`），
//!    不会"跳过第 3 条继续跑"；
//! 4. **重跑不写库**：跑前跑后的 inbound / outbound 行数一样；
//! 5. **路径守卫**：`../` 与项目外的绝对路径一律拒绝，且真的一个文件都不落；
//! 6. **没历史是 `NotFound`**（RPC 的 `-32001`），不是引擎错误。
//!
//! ## 为什么真编译
//!
//! 夹具的价值就是"用**当前**代码重跑"，所以 1/3/4 三条真的走
//! `compile_project_with_entry` + `Sandbox::spawn`（与 `hbot-replay` 的 `replay` 同一条路）。
//! 2/5/6 是纯路径与选批逻辑，不编译。

use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

use hbot_replay::ReplayOptions;
use hbot_replay::fixture::{FixtureError, resolve_fixture_path, run_fixture, save_fixture};
use hbot_store::{
    Bot, BotId, Conversation, ConversationId, InboundId, NewInbound, Project, ProjectId, Store,
};

/// 与 rpc / cli 用例同款的最小 bot：入站文本 → 一条被动回复。
const REPLY_SOURCE: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`pong:${ctx.text}`);
  },
});
"#;

/// 临时目录（`Drop` 递归删除）。不引 `tempfile`：本 crate 的依赖白名单里没有它。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-replay-fixture-{tag}-{}-{sequence}",
            std::process::id()
        ));
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

/// 一个脚手架化、能编译的项目目录 + 一份含 N 条历史入站的库。
struct Fixture {
    _dir: TempDir,
    /// 项目根（已 `canonicalize`：`save_fixture` 的路径守卫吃的就是这个形态）。
    project: PathBuf,
    store: Store,
    bot_id: BotId,
}

impl Fixture {
    /// `messages` 的每一项是 `(external_id, 文本, received_at 毫秒)`。
    fn new(tag: &str, messages: &[(&str, &str, i64)]) -> Self {
        let dir = TempDir::new(tag);
        let project = dir.path().join("project");
        hbot_code::manifest::scaffold(&project, "fixture-case").expect("脚手架应成功");
        std::fs::write(project.join("src").join("main.ts"), REPLY_SOURCE).expect("写 src/main.ts");
        let project = std::fs::canonicalize(&project).expect("canonicalize 项目目录");

        let store = Store::open(dir.path().join("hbot.db")).expect("打开测试库");
        let project_id =
            ProjectId::from_str("11111111-1111-4111-8111-111111111111").expect("固定 UUID");
        let bot_id = BotId::from_str("22222222-2222-4222-8222-222222222222").expect("固定 UUID");
        let conversation_id =
            ConversationId::from_str("33333333-3333-4333-8333-333333333333").expect("固定 UUID");
        store
            .insert_project(&Project {
                id: project_id,
                name: "fixture-case".to_owned(),
                path: dir.path().display().to_string(),
                created_at: 0,
            })
            .expect("插 projects 行");
        store
            .insert_bot(&Bot {
                id: bot_id,
                project_id,
                name: "fixture-bot".to_owned(),
                platform: "onebot11".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 0,
            })
            .expect("插 bots 行");
        store
            .insert_conversation(&Conversation {
                id: conversation_id,
                bot_id,
                external_id: "30001".to_owned(),
                kind: "group".to_owned(),
                created_at: 0,
            })
            .expect("插 conversations 行");
        for (index, (external_id, text, received_at)) in messages.iter().enumerate() {
            store
                .insert_inbound_with_sender(
                    NewInbound {
                        id: InboundId::from_str(&format!(
                            "00000000-0000-4000-8000-00000000000{index}"
                        ))
                        .expect("固定 UUID"),
                        bot_id,
                        external_id: (*external_id).to_owned(),
                        conversation_id,
                        // 只要 `text` 就够：`event_json` 的 `text` 就是从 payload 取的。
                        payload_json: json!({ "text": text }).to_string(),
                        received_at: *received_at,
                    },
                    "10086".to_owned(),
                )
                .expect("插 inbound 行");
        }
        Self {
            _dir: dir,
            project,
            store,
            bot_id,
        }
    }

    /// 回放/夹具共用的那组入参；`path` 是命令里那个原始字符串。
    fn options(&self, limit: Option<u32>, path: &str) -> ReplayOptions {
        ReplayOptions {
            project_dir: self.project.clone(),
            database: self.project.join(".hbot").join("hbot.db"),
            bot: self.bot_id.to_string(),
            limit,
            from: None,
            save_fixture: Some(PathBuf::from(path)),
        }
    }

    /// 库里现在有多少条 inbound（重跑不该改变它）。
    fn inbound_count(&self) -> usize {
        self.store
            .list_inbound_history_for_bot(&self.bot_id, None, u32::MAX)
            .expect("读历史入站")
            .len()
    }

    /// 库里现在有多少条被动回复（重跑不该改变它）。
    fn outbound_count(&self) -> usize {
        self.store
            .list_outbound_by_reply_to(&self.bot_id, &["m1".to_owned(), "m2".to_owned()])
            .expect("读历史出站")
            .len()
    }

    fn read_fixture(&self, relative: &str) -> Value {
        let text = std::fs::read_to_string(self.project.join(relative)).expect("读夹具文件");
        serde_json::from_str(&text).expect("夹具应当是合法 JSON")
    }
}

/// ① 存下来 → 用当前代码重跑 → 回复与历史一致，且文件真的落在项目里。
#[test]
fn saved_fixture_runs_back_through_the_current_code() {
    let fixture = Fixture::new("roundtrip", &[("m1", "你好", 1_000), ("m2", "世界", 2_000)]);
    let saved = save_fixture(
        &fixture.store,
        &fixture.options(None, ".hbot/fixtures/case.json"),
        ".hbot/fixtures/case.json",
    )
    .expect("存夹具应当成功");
    assert_eq!(saved.events, 2, "两条入站 → 两条事件");
    assert!(
        fixture.project.join(".hbot/fixtures/case.json").is_file(),
        "文件真的该落盘：{}",
        saved.path
    );
    // 报告里的路径不含 Windows 的 `\\?\` 前缀，可以直接回显。
    assert!(
        !saved.path.contains(r"\\?\"),
        "路径应当是人能看的样子：{}",
        saved.path
    );

    let report = run_fixture(&fixture.project, Path::new(&saved.path)).expect("重跑应当成功");
    assert_eq!(report.events, 2);
    assert_eq!(report.replies, 2, "两条事件各一条回复：{report:?}");
    assert_eq!(report.errors, 0);
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[0].external_id, "m1");
    assert_eq!(report.entries[0].text.as_deref(), Some("你好"));
    assert_eq!(report.entries[0].replies, vec!["pong:你好".to_owned()]);
    assert_eq!(report.entries[0].index, 0);
    assert_eq!(report.entries[1].external_id, "m2");
    assert_eq!(report.entries[1].replies, vec!["pong:世界".to_owned()]);
    assert!(report.entries.iter().all(|entry| entry.error.is_none()));
}

/// ② 落盘形状 = 事件数组（`event_json` 的键全在），不是 payload 数组、也不是包装对象。
#[test]
fn saved_file_is_an_event_array() {
    let fixture = Fixture::new("shape", &[("m1", "你好", 1_000)]);
    let saved = save_fixture(
        &fixture.store,
        &fixture.options(None, ".hbot/fixtures/shape.json"),
        ".hbot/fixtures/shape.json",
    )
    .expect("存夹具应当成功");
    assert_eq!(saved.events, 1);

    let value = fixture.read_fixture(".hbot/fixtures/shape.json");
    let array = value.as_array().expect("顶层必须是 JSON 数组");
    assert_eq!(array.len(), 1, "一条入站 → 一条事件");
    let event = array[0].as_object().expect("元素必须是对象");
    for key in [
        "botId",
        "conversationId",
        "messageId",
        "externalId",
        "senderId",
        "text",
        "kind",
        "conversationExternalId",
        "receivedAt",
        "payload",
    ] {
        assert!(event.contains_key(key), "事件缺键 `{key}`：{event:?}");
    }
    assert_eq!(event["externalId"], json!("m1"));
    assert_eq!(event["text"], json!("你好"));
    assert_eq!(event["kind"], json!("group"));
    assert_eq!(event["conversationExternalId"], json!("30001"));
    assert_eq!(event["receivedAt"], json!(1_000));
    assert_eq!(event["senderId"], json!("10086"));
    // payload 是历史入站的原样载荷（不是被展平到顶层）。
    assert_eq!(event["payload"], json!({ "text": "你好" }));
}

/// ③ `limit` 截断真的生效：文件里只剩前 N 条，`events` 也跟着变小。
#[test]
fn limit_truncates_the_saved_batch() {
    let fixture = Fixture::new(
        "limit",
        &[
            ("m1", "一", 1_000),
            ("m2", "二", 2_000),
            ("m3", "三", 3_000),
        ],
    );
    let saved = save_fixture(
        &fixture.store,
        &fixture.options(Some(2), ".hbot/fixtures/two.json"),
        ".hbot/fixtures/two.json",
    )
    .expect("存夹具应当成功");
    assert_eq!(saved.events, 2, "limit=2 → 只存两条");
    let value = fixture.read_fixture(".hbot/fixtures/two.json");
    let array = value.as_array().expect("顶层必须是 JSON 数组");
    assert_eq!(array.len(), 2, "文件里也只能有两条");
    let ids: Vec<&str> = array
        .iter()
        .map(|event| event["externalId"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(ids, ["m1", "m2"], "截断是从头截（与回放的选批一致）");
}

/// ④ 一条坏元素 → 整份夹具报错，**不静默跳过**（也绝不返回半份报告）。
#[test]
fn a_broken_element_fails_the_whole_run() {
    let fixture = Fixture::new("broken", &[("m1", "你好", 1_000)]);
    let saved = save_fixture(
        &fixture.store,
        &fixture.options(None, ".hbot/fixtures/good.json"),
        ".hbot/fixtures/good.json",
    )
    .expect("存夹具应当成功");
    // 拿真事件当第一条，第二条故意缺 `payload`。
    let mut events = fixture.read_fixture(".hbot/fixtures/good.json");
    let array = events.as_array_mut().expect("顶层必须是数组");
    let mut broken = array[0].clone();
    broken
        .as_object_mut()
        .expect("元素是对象")
        .remove("payload");
    array.push(broken);
    std::fs::write(
        fixture.project.join(".hbot/fixtures/mixed.json"),
        serde_json::to_string_pretty(&events).expect("序列化"),
    )
    .expect("写混合夹具");

    let error = run_fixture(
        &fixture.project,
        &fixture.project.join(".hbot/fixtures/mixed.json"),
    )
    .expect_err("坏元素必须让整份夹具失败");
    match error {
        FixtureError::Invalid(message) => {
            assert!(message.contains("第 2 条"), "文案该点明是哪一条：{message}");
            assert!(message.contains("payload"), "文案该点明缺哪个键：{message}");
        }
        other => panic!("坏元素应当是 Invalid（RPC -32002），实际：{other:?}"),
    }
    assert!(!saved.path.is_empty());
}

/// ⑤ 重跑只读夹具：库里的 inbound / outbound 一条都不变。
#[test]
fn running_a_fixture_does_not_touch_the_store() {
    let fixture = Fixture::new("readonly", &[("m1", "你好", 1_000), ("m2", "世界", 2_000)]);
    let saved = save_fixture(
        &fixture.store,
        &fixture.options(None, ".hbot/fixtures/ro.json"),
        ".hbot/fixtures/ro.json",
    )
    .expect("存夹具应当成功");
    let before = (fixture.inbound_count(), fixture.outbound_count());
    let report = run_fixture(&fixture.project, Path::new(&saved.path)).expect("重跑应当成功");
    assert_eq!(report.events, 2);
    assert_eq!(
        (fixture.inbound_count(), fixture.outbound_count()),
        before,
        "夹具重跑不许写库"
    );
}

/// ⑥ 路径守卫：`../` 与项目外的绝对路径都拒绝，且一个文件都不落。
#[test]
fn paths_outside_the_project_are_rejected() {
    let fixture = Fixture::new("escape", &[("m1", "你好", 1_000)]);
    let outside_relative = "../escape.json";
    let error =
        resolve_fixture_path(&fixture.project, outside_relative).expect_err("`../` 必须被拒绝");
    assert!(
        matches!(error, FixtureError::Invalid(_)),
        "越界应当是 Invalid（RPC -32002）：{error:?}"
    );

    let outside_absolute = fixture
        .project
        .parent()
        .expect("项目有父目录")
        .join("outside.json")
        .display()
        .to_string();
    let error = resolve_fixture_path(&fixture.project, &outside_absolute)
        .expect_err("项目外的绝对路径必须被拒绝");
    assert!(matches!(error, FixtureError::Invalid(_)));

    // `save_fixture` 自己也要挡住（不只靠调用方先校验）。
    let error = save_fixture(
        &fixture.store,
        &fixture.options(None, outside_relative),
        outside_relative,
    )
    .expect_err("存夹具也必须挡住越界路径");
    assert!(matches!(error, FixtureError::Invalid(_)));
    assert!(
        !fixture
            .project
            .parent()
            .unwrap()
            .join("escape.json")
            .exists(),
        "越界路径一个字节都不能写"
    );
    assert!(
        !fixture
            .project
            .parent()
            .unwrap()
            .join("outside.json")
            .exists(),
        "项目外的绝对路径一个字节都不能写"
    );

    // 空路径也是 Invalid，不是"写到项目根"。
    assert!(matches!(
        resolve_fixture_path(&fixture.project, "   "),
        Err(FixtureError::Invalid(_))
    ));
}

/// ⑦ 没有历史 → `NotFound`（RPC `-32001`），而不是"引擎错误"。
#[test]
fn saving_without_history_is_not_found() {
    let fixture = Fixture::new("empty", &[]);
    let error = save_fixture(
        &fixture.store,
        &fixture.options(None, ".hbot/fixtures/none.json"),
        ".hbot/fixtures/none.json",
    )
    .expect_err("没有历史应当失败");
    assert!(
        matches!(error, FixtureError::NotFound(_)),
        "没有历史是 -32001：{error:?}"
    );
    assert!(
        !fixture.project.join(".hbot/fixtures/none.json").exists(),
        "失败时不该留下空文件"
    );
}

/// ⑧ 空数组夹具：能跑，但零事件零回复（不白起一个 isolate）。
#[test]
fn an_empty_fixture_runs_zero_events() {
    let fixture = Fixture::new("vacant", &[]);
    std::fs::create_dir_all(fixture.project.join(".hbot/fixtures")).expect("建夹具目录");
    std::fs::write(fixture.project.join(".hbot/fixtures/vacant.json"), "[]\n").expect("写夹具");
    let report = run_fixture(
        &fixture.project,
        &fixture.project.join(".hbot/fixtures/vacant.json"),
    )
    .expect("空夹具也是合法夹具");
    assert_eq!(report.events, 0);
    assert_eq!(report.replies, 0);
    assert_eq!(report.errors, 0);
    assert!(report.entries.is_empty());
}

/// ⑨ 顶层不是数组 / 不是 JSON / 文件不存在 → 都是 `Invalid`（RPC `-32002`）。
#[test]
fn a_non_array_fixture_is_invalid() {
    let fixture = Fixture::new("notarray", &[]);
    let dir = fixture.project.join(".hbot/fixtures");
    std::fs::create_dir_all(&dir).expect("建夹具目录");

    for (name, content) in [
        ("object.json", "{\"version\":1,\"events\":[]}"),
        ("text.json", "not json at all"),
        ("string.json", "\"hello\""),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("写坏夹具");
        let error = run_fixture(&fixture.project, &path).expect_err("坏夹具必须失败");
        assert!(
            matches!(error, FixtureError::Invalid(_)),
            "{name} 应当是 Invalid（RPC -32002）：{error:?}"
        );
    }

    // 文件不存在也是 Invalid，不是"内部错误"。
    let error =
        run_fixture(&fixture.project, &dir.join("missing.json")).expect_err("不存在的夹具必须失败");
    assert!(matches!(error, FixtureError::Invalid(_)));
}
