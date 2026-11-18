//! 后台作业（`C4.jobs`）在**生产装配路径**上的端到端测试。
//!
//! 三侧单元测试已各自覆盖：`hbot-core` 的契约与参数闸（`bot_jobs.rs`）、`hbot-store` 的
//! `bot_jobs` 表与仓储（`repositories::jobs`）、`hbot-code` 的槽位与派发（`jobs.rs`）。
//! 本文件补的是它们之间**唯一没有别处覆盖的那一段**：
//!
//! `AppState::start_bot` → `SandboxHandlerConfig::jobs` → `SandboxHandler::worker` 里那句
//! `declared_jobs(&project_root)` + `sandbox.set_jobs(...)` → ticker 每拍调
//! `SandboxHandler::tick_jobs` → `claim_due_jobs_for_bot` → 在 isolate 上派发 →
//! `mark_job_done` / `reschedule_job` / `mark_job_failed`。
//!
//! 漏掉其中任何一处，`ctx.jobs.enqueue` 都会报 `HBOT_JOBS_UNAVAILABLE`（或入队了但永远
//! 不派发），而上面每一层的单元测试都还是绿的 —— 所以这里必须走 `Dispatcher` 真派发、
//! 真 tick、真库。
//!
//! 断言覆盖 `docs/07` 的 4.3 节规则表与 HALF 的验收清单：
//!
//! 1. **没声明就没得用**：不写 `capabilities.jobs` → `HBOT_JOBS_NOT_DECLARED`；
//! 2. **声明了真能跑**：`onMessage` 里入队 → 落库 → 下一拍认领 → `onJob` 真跑；
//! 3. **幂等键重复入队只留一行**：同名同键只跑一次；
//! 4. **作业回复进 outbox**：写了 `conversation` 时 `ctx.reply` 落成回复日志；
//! 5. **缺 `onJob` 点名报错**：声明了类型却没写钩子 → error 日志点名 `onJob`；
//! 6. **`bot_stop` 取消未完成作业**：`pending` 行进 `cancelled`，之后不再派发。

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

const BOT_UUID: &str = "77777777-7777-4777-8777-777777777777";
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";
const JOB_PREFIX: &str = "job:";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-jobscap-{label}-{}-{seq}", std::process::id()));
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

/// 入队一条作业，然后立刻回复（入队必须真的不等它跑完）。
fn enqueue_handler() -> &'static str {
    r#"
import { defineBot, reply, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      await ctx.jobs.enqueue('settle-war', { warId: ctx.text }, { conversation: 'local:default' });
      reply('queued:' + ctx.text);
    } catch (error) {
      reply('jobs-error:' + error.message);
    }
  },
  async onJob(ctx, name) {
    log('job:' + name + ':' + ctx.idempotencyKey);
    ctx.reply('job-reply:' + name);
  },
});
"#
}

/// 同一个幂等键入队两次：第二条必须被库里唯一约束吞掉。
fn duplicate_handler() -> &'static str {
    r#"
import { defineBot, reply, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      const options = { idempotencyKey: 'same-key', conversation: 'local:default' };
      await ctx.jobs.enqueue('settle-war', { round: 1 }, options);
      await ctx.jobs.enqueue('settle-war', { round: 2 }, options);
      reply('queued-twice');
    } catch (error) {
      reply('jobs-error:' + error.message);
    }
  },
  async onJob(ctx, name) {
    log('job:' + name + ':' + ctx.idempotencyKey);
  },
});
"#
}

/// 清单声明了作业类型，但代码**没有** `onJob`。
fn no_job_hook_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.jobs.enqueue('settle-war', { warId: 'x' });
    reply('queued');
  },
});
"#
}

/// `hbot.json`：平台 `local`（不联网），作业类型按参数声明。
fn manifest(specs: Option<&str>) -> String {
    match specs {
        None => "{\"name\":\"jobs-bot\",\"platform\":{\"kind\":\"local\"}}\n".to_owned(),
        Some(specs) => format!(
            "{{\"name\":\"jobs-bot\",\"platform\":{{\"kind\":\"local\"}},\"capabilities\":{{\"jobs\":[{specs}]}}}}\n"
        ),
    }
}

/// 一条作业类型声明。
fn spec(name: &str, budget_ms: u64, retries: u32) -> String {
    format!("{{\"name\":\"{name}\",\"budgetMs\":{budget_ms},\"retries\":{retries}}}")
}

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"作业能力\"}}]}}")
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

struct Harness {
    /// 持有临时目录的所有权：`Drop` 时它会删掉整份夹具。
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    async fn new(label: &str, handler_source: &str, manifest_json: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        std::fs::write(project.join("hbot.json"), manifest_json).expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir.clone(), Arc::new(SystemClock), Arc::new(UuidIds))
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

    fn conversation(&self) -> ConversationId {
        self.store()
            .get_conversation_by_external(&self.bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .expect("bot_start 之后应存在 local 会话行")
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

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
    }

    fn job_lines(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.line.starts_with(JOB_PREFIX))
            .map(|row| row.line)
            .collect()
    }

    fn replies(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    fn errors(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.level == "error")
            .map(|row| row.line)
            .collect()
    }

    /// 这个 bot 的作业行（按入队时刻升序）。
    fn jobs(&self) -> Vec<hbot_store::JobRow> {
        self.store()
            .list_jobs_for_bot(&self.bot, 64)
            .expect("读作业")
    }
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

async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

async fn settle(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

// ---------------------------------------------------------------------------
// ① 没声明 → 声明层拒绝
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_job_capability_reports_not_declared() {
    let harness = Harness::new("undeclared", enqueue_handler(), &manifest(None)).await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("war-1").await, "bot_inject");
    wait_for("收到一条回复", || harness.replies().len() > before).await;
    let reply = harness
        .replies()
        .into_iter()
        .next()
        .expect("刚等到的那条回复");
    assert!(
        reply.starts_with("jobs-error:HBOT_JOBS_NOT_DECLARED"),
        "没声明 capabilities.jobs 时必须报 NOT_DECLARED（不是 UNAVAILABLE、不是静默成功），实际：{reply}"
    );
    assert!(
        harness.jobs().is_empty(),
        "被拒的入队不许落库，实际：{:?}",
        harness.jobs()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 声明了 → 入队落库 → 下一拍真跑（日志 + 回复）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_job_runs_on_the_next_tick_and_replies() {
    let harness = Harness::new(
        "runs",
        enqueue_handler(),
        &manifest(Some(&spec("settle-war", 60_000, 0))),
    )
    .await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("war-1").await, "bot_inject");
    wait_for("onMessage 里的入队回复", || {
        harness.replies().len() > before
    })
    .await;

    // 入队是同步语义：消息一处理完，作业行就该已经在库里（`pending`）。
    wait_for("作业行落库", || !harness.jobs().is_empty()).await;
    assert_eq!(
        harness.jobs()[0].name,
        "settle-war",
        "落库的作业名应与声明一致：{:?}",
        harness.jobs()
    );

    // 下一拍（≤1 秒）认领并跑完：作业体写 `job:` 前缀的日志。
    wait_for("作业体真的跑了", || !harness.job_lines().is_empty()).await;
    let line = harness.job_lines().into_iter().next().expect("作业日志行");
    assert!(
        line.starts_with("job:settle-war:"),
        "作业日志应点名作业类型，实际：{line}"
    );

    // 跑完之后是 `done` 终态（不是永远停在 running）。
    wait_for("作业落 done 终态", || {
        harness
            .jobs()
            .first()
            .is_some_and(|row| row.state == "done")
    })
    .await;

    // 入队时写了 conversation，作业里的 ctx.reply 必须进 outbox。
    wait_for("作业回复进 outbox", || {
        harness
            .replies()
            .iter()
            .any(|text| text == "job-reply:settle-war")
    })
    .await;

    assert!(
        harness.errors().is_empty(),
        "正常跑完的作业不该留 error 日志，实际：{:?}",
        harness.errors()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 幂等键：入队两次只留一行、只跑一次
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_idempotency_key_leaves_one_row_and_runs_once() {
    let harness = Harness::new(
        "idempotent",
        duplicate_handler(),
        &manifest(Some(&spec("settle-war", 60_000, 0))),
    )
    .await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("twice").await, "bot_inject");
    wait_for("入队两次的回复", || harness.replies().len() > before).await;

    wait_for("作业行落库", || !harness.jobs().is_empty()).await;
    assert_eq!(
        harness.jobs().len(),
        1,
        "同一个 (bot, idempotencyKey) 只能留一行：{:?}",
        harness.jobs()
    );
    assert_eq!(harness.jobs()[0].idempotency_key, "same-key");

    wait_for("作业真的跑了", || !harness.job_lines().is_empty()).await;
    wait_for("作业落 done 终态", || {
        harness
            .jobs()
            .first()
            .is_some_and(|row| row.state == "done")
    })
    .await;
    settle(1_500).await;
    assert_eq!(
        harness.job_lines().len(),
        1,
        "同一幂等键只该跑一次，实际日志：{:?}",
        harness.job_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 声明了类型但没写 `onJob` → 点名报缺钩子（不静默标成功）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_job_hook_is_named_in_an_error_log() {
    let harness = Harness::new(
        "no-hook",
        no_job_hook_handler(),
        &manifest(Some(&spec("settle-war", 60_000, 0))),
    )
    .await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("x").await, "bot_inject");
    wait_for("入队回复", || harness.replies().len() > before).await;

    wait_for("点名缺钩子的 error 日志", || {
        harness.errors().iter().any(|line| line.contains("onJob"))
    })
    .await;
    let error = harness
        .errors()
        .into_iter()
        .find(|line| line.contains("onJob"))
        .expect("含 onJob 的 error 行");
    assert!(
        error.contains("settle-war"),
        "错误文案要点名是哪条作业，实际：{error}"
    );
    assert!(
        harness.job_lines().is_empty(),
        "没有钩子就不该有作业日志，实际：{:?}",
        harness.job_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ `bot_stop` 取消未完成的作业
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bot_stop_cancels_the_pending_jobs() {
    // 预算给得极小不可能的：这里要的是"停掉之后 pending 行不再被派发"。
    // 用 `local` 平台 + 一条永远不满足的 `onJob` 触发器做不到，所以改用另一种形状：
    // 入队后**立刻**停止 bot（tick 还来不及派发），断言行进了 `cancelled`。
    let harness = Harness::new(
        "cancel",
        enqueue_handler(),
        &manifest(Some(&spec("settle-war", 60_000, 0))),
    )
    .await;
    assert_ok(&harness.start().await, "bot_start");
    let before = harness.replies().len();
    assert_ok(&harness.inject("w").await, "bot_inject");
    wait_for("入队回复", || harness.replies().len() > before).await;
    assert_ok(&harness.stop().await, "bot_stop");

    // 停止会取消所有未完成作业；已经跑完的那些保持 `done`。
    wait_for("未完成作业落 cancelled", || {
        harness
            .jobs()
            .iter()
            .all(|row| row.state == "cancelled" || row.state == "done")
    })
    .await;
    assert!(
        harness.jobs().iter().any(|row| row.state == "cancelled"),
        "bot_stop 之后应有 cancelled 行，实际：{:?}",
        harness.jobs()
    );

    // 再等两拍：已取消的行不该被重新派发（重启的恢复也会跳过终态）。
    let lines_after_stop = harness.job_lines().len();
    settle(2_500).await;
    assert_eq!(
        harness.job_lines().len(),
        lines_after_stop,
        "取消之后不许再派发，实际日志：{:?}",
        harness.job_lines()
    );
}

// ---------------------------------------------------------------------------
// ⑥ 失败 → 按声明的重试次数退避（这里断言"落 failed 且写了 error"）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_job_ends_in_failed_and_writes_an_error_line() {
    // 默认 `retries: 0`：第一次失败就直接落 failed。
    let harness = Harness::new(
        "failing",
        failing_handler(),
        &manifest(Some(&spec("settle-war", 60_000, 0))),
    )
    .await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("f").await, "bot_inject");
    wait_for("入队回复", || harness.replies().len() > before).await;
    wait_for("作业落 failed 终态", || {
        harness
            .jobs()
            .first()
            .is_some_and(|row| row.state == "failed")
    })
    .await;
    assert!(
        harness
            .errors()
            .iter()
            .any(|line| line.contains("settle-war")),
        "失败要写一行点名作业的 error，实际：{:?}",
        harness.errors()
    );
    let row = &harness.jobs()[0];
    assert_eq!(row.attempts, 1, "第一次执行就落 failed，attempts 必须是 1");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|text| !text.is_empty()),
        "failed 行必须带错误正文，实际：{:?}",
        row.last_error
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

/// 作业体故意抛异常（`retries: 0` 时第一次就落 failed）。
fn failing_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.jobs.enqueue('settle-war', { warId: 'bad' });
    reply('queued');
  },
  async onJob(ctx, name) {
    throw new Error('作业里故意抛：' + name);
  },
});
"#
}
