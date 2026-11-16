//! `bot_conversations` 的集成测试：**UI 拿 `conversationId` 的唯一正路**。
//!
//! 背景：`bot_inject` 必须带 `conversationId`，而 `bot_start` 的返回是 `null`。
//! 在这条命令之前，UI 只能去正则匹配 `bot_start` 落的那行 info 日志
//! （`local 试跑会话已就绪：conversationId = …`）—— 脏办法，且日志格式一改就断。
//!
//! 这里钉住四件事（对应任务书的 C1–C5）：
//!
//! 1. **没有会话 → 严格空数组**：不是 `null`、不是错误（`null` 会让 UI 的
//!    `for … of` 直接炸，错误会让"还没人说话"被当成故障）；
//! 2. **`local` 试跑 → 恰好一条 `local:default` / `private` / 合法 UUID**，且那个
//!    `conversationId` 在库里真有对应行（就是 `bot_inject` 能接受的那个值）；
//! 3. **`start → stop → start` 仍是 1 条**（幂等建会话，不堆孤儿行、不换 id）；
//! 4. **不存在的 bot → `-32001` + `data.entity == "bot"`**；
//! 5. **顺序确定性**：两条以上会话时顺序固定为 `(kind, external_id)`，两次调用
//!    逐字节相同（否则 UI 每次刷新列表都在跳，测试也会 flaky）。
//!
//! 第 2 条走的是生产路径（真 `LocalAdapter` + 真 `SandboxHandler` + 真 store），
//! 夹具照 `tests/local_runtime.rs`：临时目录 + `bots.json` 只有 `platform:"local"`
//! + `ProjectOpen` + `bot_start`。其余用例不需要起 runtime，直接往 store 里插行。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId, ProjectId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, ConversationSummary, Response};
use serde_json::{Value, json};

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// 本地试跑用的固定外部会话 ID（与 `hbot-adapters` 的常量必须一字不差）。
const LOCAL_EXTERNAL_ID: &str = "local:default";

// ---------------------------------------------------------------------------
// 临时目录
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-botconv-{label}-{}-{seq}", std::process::id()));
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

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

fn id(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}

// ---------------------------------------------------------------------------
// 夹具：一个真项目 + 真 AppState（默认 PlatformRouter）
// ---------------------------------------------------------------------------

/// 生成代码里最朴素的 handler（`bot_start` 要真编译 `src/main.ts`）。
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
    bot: BotId,
}

impl Harness {
    /// 造项目目录 + `src/main.ts` + 只有 `platform:"local"` 的 `bots.json`，然后打开项目。
    async fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), echo_handler()).expect("写 handler");
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

    async fn conversations(&self, bot: BotId) -> Response {
        self.dispatcher
            .dispatch(Command::BotConversations { bot_id: bot })
            .await
    }

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 256)
            .expect("读日志")
    }

    /// 直接插一行 bot（**不启动它**）：造"bot 行存在、但还没有会话"的状态。
    ///
    /// 只插行不起 runtime —— 这条链路要验的是"命令读到什么"，与 bot 在不在跑无关
    /// （`bot_list` 也允许上一进程留下的 `stopped` 行）。
    fn seed_bot_row(&self, seed: u128) -> BotId {
        let root = self.state.project_root().expect("已打开项目");
        let project = hbot_store::Project {
            id: ProjectId::from_uuid(id(seed + 1_000)),
            name: format!("seeded-{seed}"),
            path: root
                .join(format!("seeded-{seed}"))
                .to_string_lossy()
                .into_owned(),
            created_at: 1_000,
        };
        self.store().insert_project(&project).expect("插项目行");
        let bot = hbot_store::Bot {
            id: BotId::from_uuid(id(seed)),
            project_id: project.id,
            name: format!("seeded-bot-{seed}"),
            platform: "local".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 1_000,
        };
        self.store().insert_bot(&bot).expect("插 bot 行");
        bot.id
    }

    /// 直接插一行会话（绕过 runtime）：用来测排序 / kind 透传 / 过滤。
    fn seed_conversation(
        &self,
        bot: BotId,
        seed: u128,
        external_id: &str,
        kind: &str,
        created_at: i64,
    ) {
        self.store()
            .insert_conversation(&hbot_store::Conversation {
                id: ConversationId::from_uuid(id(seed)),
                bot_id: bot,
                external_id: external_id.to_owned(),
                kind: kind.to_owned(),
                created_at,
            })
            .expect("插会话行");
    }
}

// ---------------------------------------------------------------------------
// 断言辅助
// ---------------------------------------------------------------------------

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

/// 成功响应的 `data`；**缺失 data 直接失败**（空结果也必须是 `[]`，不是缺省/`null`）。
fn data_of(response: Response) -> Value {
    match response {
        Response::Ok { data } => data.expect("成功响应必须带 data（空结果也是 []，不是 null）"),
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

fn error_of(response: Response) -> (i32, String, Option<Value>) {
    match response {
        Response::Ok { data } => panic!("期望失败，实际成功：{data:?}"),
        Response::Error { error } => (error.code, error.message, error.data),
    }
}

fn external_ids(data: &Value) -> Vec<&str> {
    data.as_array()
        .expect("data 必须是数组")
        .iter()
        .map(|row| row["externalId"].as_str().expect("每行都有 externalId"))
        .collect()
}

fn kinds(data: &Value) -> Vec<&str> {
    data.as_array()
        .expect("data 必须是数组")
        .iter()
        .map(|row| row["kind"].as_str().expect("每行都有 kind"))
        .collect()
}

// ---------------------------------------------------------------------------
// C1：没有会话 → 严格空数组
// ---------------------------------------------------------------------------

/// bot 行存在、但一个会话都没有 → `data == []`。
///
/// 严格断言三层：是数组、长度 0、**等于 `json!([])`**（`null` / 缺省 / 错误都过不了）。
#[tokio::test]
async fn bot_without_conversations_returns_an_empty_array() {
    let harness = Harness::new("empty").await;
    let bot = harness.seed_bot_row(7);

    let data = data_of(harness.conversations(bot).await);
    assert!(data.is_array(), "data 必须是数组，实际是 {data}");
    assert_eq!(data, json!([]), "没有会话必须是严格空数组");

    // 再调一次同样严格：空结果不是"第一次碰巧"。
    assert_eq!(data_of(harness.conversations(bot).await), json!([]));

    // 反序列化成冻结的 DTO 也是空表（wire 形状没跑偏）。
    let parsed: Vec<ConversationSummary> =
        serde_json::from_value(data).expect("空数组应能反序列化成 Vec<ConversationSummary>");
    assert!(parsed.is_empty());
}

// ---------------------------------------------------------------------------
// C2：`local` 试跑 → 恰好一条 local:default / private / 合法 UUID
// ---------------------------------------------------------------------------

/// 端到端：`bot_start` 之后，UI 能从这里拿到 `bot_inject` 需要的 `conversationId`。
#[tokio::test]
async fn local_bot_start_exposes_the_trial_conversation() {
    let harness = Harness::new("local").await;
    assert_ok(&harness.start().await);

    let data = data_of(harness.conversations(harness.bot).await);
    let rows = data.as_array().expect("data 必须是数组");
    assert_eq!(rows.len(), 1, "local bot_start 之后应恰好一条会话：{data}");

    let row = &rows[0];
    // 线上形状：只有契约里的三个键，且是 camelCase。
    let mut keys: Vec<&str> = row
        .as_object()
        .expect("每行是对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["conversationId", "externalId", "kind"],
        "字段名是冻结契约，不许改名/加字段"
    );
    assert_eq!(row["externalId"], json!(LOCAL_EXTERNAL_ID));
    assert_eq!(row["kind"], json!("private"));

    let raw_id = row["conversationId"]
        .as_str()
        .expect("conversationId 是字符串（TS 侧 string，不是对象）");
    let parsed = uuid::Uuid::parse_str(raw_id).expect("conversationId 必须是合法 UUID 字符串");

    // 这个 id 就是 `bot_inject` 要的那一个：库里有同一行，且属于这个 bot。
    let stored = harness
        .store()
        .get_conversation(&ConversationId::from_uuid(parsed))
        .expect("按 id 读会话")
        .expect("命令返回的 conversationId 必须在库里存在");
    assert_eq!(stored.external_id, LOCAL_EXTERNAL_ID);
    assert_eq!(stored.bot_id, harness.bot);

    // 反序列化成 DTO，逐字段对上（证明线上值与冻结类型一致）。
    let summaries: Vec<ConversationSummary> =
        serde_json::from_value(data.clone()).expect("反序列化成 ConversationSummary");
    assert_eq!(
        summaries,
        vec![ConversationSummary {
            conversation_id: stored.id,
            external_id: LOCAL_EXTERNAL_ID.to_owned(),
            kind: "private".to_owned(),
        }]
    );

    // D 项：`bot_start` 那行 info 日志**保留**且打印的是真值 conversationId。
    // 它现在是"人看得见"的旁路，UI 不该再依赖它 —— 但值必须是真的。
    let announced = harness
        .logs()
        .iter()
        .filter(|log| log.line.contains(&format!("conversationId = {raw_id}")))
        .count();
    assert_eq!(
        announced, 1,
        "bot_start 的 info 日志必须打印真值 conversationId（不能是占位符）"
    );

    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// C3：start → stop → start 仍是 1 条（幂等，不堆孤儿行）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restarting_does_not_pile_up_orphan_rows() {
    let harness = Harness::new("restart").await;

    assert_ok(&harness.start().await);
    let first = data_of(harness.conversations(harness.bot).await);
    assert_eq!(first.as_array().map(Vec::len), Some(1), "首次启动后 1 条");
    let first_id = first[0]["conversationId"].clone();

    assert_ok(&harness.stop().await);
    assert_ok(&harness.start().await);

    let second = data_of(harness.conversations(harness.bot).await);
    let rows = second.as_array().expect("data 必须是数组");
    assert_eq!(
        rows.len(),
        1,
        "stop → start 之后仍只能有一条会话（幂等建会话）：{second}"
    );
    assert_eq!(
        second[0]["conversationId"], first_id,
        "重启不能换 conversationId（否则之前发给 UI 的 id 会失效）"
    );

    // 库里也只有一行：不是返回值去重，而是根本没插第二行。
    let stored = harness
        .store()
        .list_conversations_for_bot(&harness.bot)
        .expect("按 bot 列会话");
    assert_eq!(stored.len(), 1, "数据库里不许有孤儿会话行");

    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// C4：不存在的 bot → -32001 且 data.entity == "bot"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_bot_is_reported_as_not_found() {
    let harness = Harness::new("missing").await;
    let missing = BotId::from_uuid(id(4_242));

    let (code, message, data) = error_of(harness.conversations(missing).await);
    assert_eq!(
        code, -32001,
        "不存在的 bot 必须报 -32001，实际消息：{message}"
    );
    let data = data.expect("-32001 必须带结构化 data");
    assert_eq!(data["entity"], json!("bot"), "data.entity 必须是 bot");
    assert_eq!(
        data["id"],
        json!(missing.to_string()),
        "data.id 是那个 botId"
    );
}

// ---------------------------------------------------------------------------
// C5：两条以上 → 顺序固定为 (kind, external_id)，两次调用逐字节相同
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_is_ordered_and_byte_identical_across_calls() {
    let harness = Harness::new("order").await;
    let bot = harness.seed_bot_row(31);

    // 刻意按"乱序 + created_at 递增"插入：证明顺序既不是插入序、也不是 created_at 序。
    harness.seed_conversation(bot, 41, "chat-b", "private", 10);
    harness.seed_conversation(bot, 42, "group-9", "group", 20);
    harness.seed_conversation(bot, 43, "chat-a", "private", 5);

    // 另一个 bot 的会话：一行都不能混进来。
    let other = harness.seed_bot_row(32);
    harness.seed_conversation(other, 44, "chat-0", "private", 1);

    let first = data_of(harness.conversations(bot).await);
    let second = data_of(harness.conversations(bot).await);

    assert_eq!(
        external_ids(&first),
        vec!["group-9", "chat-a", "chat-b"],
        "顺序必须是 (kind, external_id) 升序：group < private，private 内 chat-a < chat-b"
    );
    assert_eq!(kinds(&first), vec!["group", "private", "private"]);
    assert_eq!(
        first.as_array().map(Vec::len),
        Some(3),
        "另一个 bot 的会话不能被列进来"
    );

    // 确定性：两次调用连 JSON 文本都相同（逐字节），不是"集合相等"。
    assert_eq!(first, second, "同一份数据两次查询必须一致");
    assert_eq!(
        serde_json::to_string(&first).expect("序列化"),
        serde_json::to_string(&second).expect("序列化"),
        "两次调用的 JSON 必须逐字节相同"
    );

    // 另一个 bot 的会话确实在库里 —— 只是不属于这个 bot。
    assert_eq!(
        harness
            .store()
            .list_conversations_for_bot(&other)
            .expect("列另一个 bot 的会话")
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// 附加：`kind` 原样透传（不发明枚举、不做归一化）
// ---------------------------------------------------------------------------

/// 契约里 `kind` 是开放字符串（`private` / `group` / `channel` …）。
/// 这里用库里已存在的第三种值 `channel` 证明命令**不归一化**：原样进去、原样出来。
#[tokio::test]
async fn kind_is_passed_through_verbatim() {
    let harness = Harness::new("kind").await;
    let bot = harness.seed_bot_row(51);
    harness.seed_conversation(bot, 61, "channel-7", "channel", 1);

    let data = data_of(harness.conversations(bot).await);
    assert_eq!(
        data,
        json!([{
            "conversationId": ConversationId::from_uuid(id(61)).to_string(),
            "externalId": "channel-7",
            "kind": "channel",
        }]),
        "kind 必须原样透传，且整行只有契约里的三个键"
    );
}
