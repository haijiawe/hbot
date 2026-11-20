//! `bot_jobs` / `bot_waits` 的集成测试：作业面板与等待面板的**唯一读路径**。
//!
//! 这两条命令是**只读快照**：不派发作业、不结算超时、不重发提示。这里钉住四件事：
//!
//! 1. **新的在前**：面板显示「最近发生了什么」，`limit` 截断时丢掉的必须是最旧的；
//! 2. **终态行也回**：`done` / `expired` 要是被过滤掉，用户就看不到「刚才那条失败了」；
//! 3. **字段名是 camelCase**（`budgetMs` / `remindersLeft` / `lastError` …）：契约层与
//!    TS 生成物同名，写错就是 UI 侧一片 `undefined`；
//! 4. **bot 不存在 → `-32001`**：返回空数组会让「id 写错了」看起来像「这个 bot 什么都没跑过」。
//!
//! 不走 runtime：直接往 store 里插行，测的是命令读到什么、怎么排、怎么映射。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId, JobId, ProjectId, WaitId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};
use hbot_store::{Conversation, NewJob, NewWait};
use serde_json::{Value, json};

const BOT_UUID: &str = "66666666-6666-4666-8666-666666666666";
const UNKNOWN_UUID: &str = "77777777-7777-4777-8777-777777777777";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-views-{label}-{}-{seq}", std::process::id()));
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

fn uuid(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState");
        let bot = seed_bot(&state);
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot,
        }
    }

    fn store(&self) -> &Arc<hbot_store::Store> {
        self.state.store()
    }

    async fn jobs(&self, bot: BotId, limit: usize) -> Response {
        self.dispatcher
            .dispatch(Command::BotJobs { bot_id: bot, limit })
            .await
    }

    async fn waits(&self, bot: BotId, limit: usize) -> Response {
        self.dispatcher
            .dispatch(Command::BotWaits { bot_id: bot, limit })
            .await
    }

    /// 插一行项目 + 一行 bot（不启动 runtime：这两条命令只读库）。
    fn seed_conversation(&self, seed: u128, external_id: &str, created_at: i64) -> ConversationId {
        let conversation = Conversation {
            id: ConversationId::from_uuid(uuid(seed)),
            bot_id: self.bot,
            external_id: external_id.to_owned(),
            kind: "private".to_owned(),
            created_at,
        };
        self.store()
            .insert_conversation(&conversation)
            .expect("插会话行");
        conversation.id
    }

    fn enqueue(&self, seed: u128, key: &str, created_at: i64) -> JobId {
        let job = NewJob {
            id: JobId::from_uuid(uuid(seed)),
            name: "digest".to_owned(),
            payload_json: json!({ "seed": seed }).to_string(),
            idempotency_key: key.to_owned(),
            budget_ms: 60_000,
            conversation: None,
            next_at: created_at,
            created_at,
        };
        self.store().enqueue_job(&self.bot, &job).expect("入队");
        job.id
    }

    fn register_wait(
        &self,
        seed: u128,
        conversation: ConversationId,
        external_id: &str,
        key: &str,
        at: i64,
        reminders_left: u8,
    ) -> WaitId {
        let wait = NewWait {
            id: WaitId::from_uuid(uuid(seed)),
            conversation_id: conversation,
            conversation_external_id: external_id.to_owned(),
            owner: "u1".to_owned(),
            key: key.to_owned(),
            prompt: "请把订单号发给我".to_owned(),
            scope: "sender".to_owned(),
            deadline: at + 60_000,
            created_at: at,
            reminders_left,
            timeout_ms: 60_000,
        };
        self.store()
            .register_wait(&self.bot, &wait)
            .expect("登记等待");
        wait.id
    }
}

/// 一行项目 + 一行 bot：`bot_jobs` / `bot_waits` 都要求 bot 行存在（外键 + 存在性检查）。
fn seed_bot(state: &AppState) -> BotId {
    let root = state.data_dir().to_path_buf();
    let project = hbot_store::Project {
        id: ProjectId::from_uuid(uuid(1)),
        name: "views".to_owned(),
        path: root.join("project").to_string_lossy().into_owned(),
        created_at: 1_000,
    };
    state.store().insert_project(&project).expect("插项目行");
    let bot = hbot_store::Bot {
        id: bot_id(),
        project_id: project.id,
        name: "面板 bot".to_owned(),
        platform: "local".to_owned(),
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: 1_000,
    };
    state.store().insert_bot(&bot).expect("插 bot 行");
    bot.id
}

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

fn data_of(response: Response) -> Value {
    match response {
        Response::Ok { data } => data.expect("成功响应必须带 data"),
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

fn rows(data: &Value) -> &Vec<Value> {
    data.as_array().expect("data 必须是数组")
}

#[tokio::test]
async fn bot_jobs_lists_the_newest_first_and_caps_the_limit() {
    let harness = Harness::new("jobs");
    let bottom = harness.enqueue(7, "a", 1_000);
    let middle = harness.enqueue(8, "b", 2_000);
    let top = harness.enqueue(9, "c", 3_000);

    let data = data_of(harness.jobs(harness.bot, 10).await);
    let ids: Vec<&str> = rows(&data)
        .iter()
        .map(|row| row["id"].as_str().expect("每行都有 id"))
        .collect();
    assert_eq!(
        ids,
        vec![top.to_string(), middle.to_string(), bottom.to_string()],
        "面板新的在前（截断时丢最旧的）"
    );

    let first = &rows(&data)[0];
    assert_eq!(first["name"], json!("digest"));
    assert_eq!(first["state"], json!("pending"));
    assert_eq!(first["attempts"], json!(0));
    assert_eq!(first["budgetMs"], json!(60_000), "契约字段是 camelCase");
    assert_eq!(first["createdAt"], json!(3_000));
    assert_eq!(first["conversation"], Value::Null);
    assert_eq!(first["lastError"], Value::Null);

    let capped = data_of(harness.jobs(harness.bot, 2).await);
    assert_eq!(rows(&capped).len(), 2, "limit 就是上限");
    assert_eq!(rows(&capped)[1]["id"], json!(middle.to_string()));

    let oversized = data_of(harness.jobs(harness.bot, usize::MAX).await);
    assert_eq!(rows(&oversized).len(), 3, "超上限按上限算，不报错");
}

#[tokio::test]
async fn bot_waits_shows_terminal_rows_with_the_reminder_fields() {
    let harness = Harness::new("waits");
    let older = harness.seed_conversation(11, "local:a", 1_000);
    let newer = harness.seed_conversation(12, "local:b", 2_000);
    // 最早那条**没有**重发额度：到点就直接落 expired（有额度的会先被推后一轮）。
    let expired = harness.register_wait(21, older, "local:a", "order-no", 1_000, 0);
    let pending = harness.register_wait(22, newer, "local:b", "phone", 2_000, 1);

    // 让最早那条到点落终态：面板必须照样能看到它。
    let settled = harness
        .store()
        .expire_due_waits(&harness.bot, 61_000, 8)
        .expect("结算超时");
    assert_eq!(settled.len(), 1, "只有最早那条到点");
    assert_eq!(settled[0].id, expired);

    let data = data_of(harness.waits(harness.bot, 10).await);
    let list = rows(&data);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], json!(pending.to_string()), "新的在前");
    assert_eq!(list[0]["state"], json!("pending"));
    assert_eq!(list[0]["key"], json!("phone"));
    assert_eq!(list[0]["conversation"], json!("local:b"));
    assert_eq!(list[0]["scope"], json!("sender"));
    assert_eq!(list[0]["remindersLeft"], json!(1), "额度要透传给面板");
    assert_eq!(list[0]["timeoutMs"], json!(60_000));

    assert_eq!(list[1]["id"], json!(expired.to_string()));
    assert_eq!(list[1]["state"], json!("expired"), "终态行不许被过滤掉");
    assert_eq!(list[1]["prompt"], json!("请把订单号发给我"));

    assert!(
        harness
            .store()
            .current_wait(&harness.bot, &older)
            .expect("读等待")
            .is_none(),
        "只读快照不许改状态（这条已由 expire 落终态，再读一次仍是空）"
    );
}

#[tokio::test]
async fn an_unknown_bot_is_not_found_rather_than_an_empty_list() {
    let harness = Harness::new("unknown");
    let unknown = BotId::from_uuid(uuid::Uuid::parse_str(UNKNOWN_UUID).expect("uuid"));

    for (label, response) in [
        ("bot_jobs", harness.jobs(unknown, 10).await),
        ("bot_waits", harness.waits(unknown, 10).await),
    ] {
        match response {
            Response::Ok { data } => panic!("{label} 对不存在的 bot 必须失败，实际：{data:?}"),
            Response::Error { error } => {
                assert_eq!(error.code, -32001, "{label} 的 code");
                let entity = error
                    .data
                    .as_ref()
                    .and_then(|data| data["entity"].as_str().map(str::to_owned));
                assert_eq!(entity.as_deref(), Some("bot"), "{label} 的 entity");
            }
        }
    }
}

#[tokio::test]
async fn an_empty_history_is_a_strict_empty_array() {
    let harness = Harness::new("empty");
    assert_ok(&harness.jobs(harness.bot, 10).await);
    let jobs = data_of(harness.jobs(harness.bot, 10).await);
    assert_eq!(jobs, json!([]), "没有作业时必须是 []，不是 null");
    let waits = data_of(harness.waits(harness.bot, 10).await);
    assert_eq!(waits, json!([]));
}
