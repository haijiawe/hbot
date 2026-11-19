//! 具名计划（`onSchedule`）在**生产装配路径**上的端到端测试。
//!
//! `onSchedule` 与 `onTimer` 是**同一条执行路径的两个名字**（`onSchedule` 是正式名，
//! `onTimer` 是首个发布用的旧名）。这里钉的正是「两个名字合并成一条派发」这件事，
//! 以及它在真装配链路上的行为：
//!
//! `AppState::start_bot` 起 tick 循环 → `SandboxHandler::tick` 读
//! `capabilities.timer` → `TimerSlot::due` 拿到点条目 → 在同一个 isolate 上派发 →
//! 日志落库。
//!
//! 六类断言：
//!
//! 1. 写 `onSchedule` 的具名计划 → 到点真的跑起来，`ctx.log` 落 `logs` 表；
//! 2. 没声明 → tick 一秒一拍也不干活；
//! 3. 声明了却没写钩子 → 点名 `onSchedule`（不是静默成功，也不是只提旧名）；
//! 4. 两个名字都写 → **启动失败**并说清「它们是一个钩子的两个名字」，不静默挑一个；
//! 5. 只写旧名 `onTimer` → 仍然触发（改名不破坏已有项目）；
//! 6. 重启核心后计划仍按清单重新对齐并再次触发（触发点是从 `now` 派生的，不落库）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hbot_adapters::SystemClock;
use hbot_core::BotId;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};

const BOT_UUID: &str = "77777777-7777-4777-8777-777777777777";

/// 定时器日志行的前缀（handler 自己写，测试用它筛出计划产生的行）。
const SCHEDULE_PREFIX: &str = "sched:";

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
        let path = std::env::temp_dir().join(format!(
            "hbot-schedcap-{label}-{}-{seq}",
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

// ---------------------------------------------------------------------------
// handler 源码
// ---------------------------------------------------------------------------

fn schedule_handler() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('msg:' + ctx.text);
  },
  async onSchedule(ctx, name) {
    log('sched:' + name + ':' + ctx.name);
  },
});
"#
}

fn legacy_handler() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('msg:' + ctx.text);
  },
  async onTimer(ctx, name) {
    log('sched:' + name + ':' + ctx.name);
  },
});
"#
}

fn handler_without_hook() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('msg:' + ctx.text);
  },
});
"#
}

fn handler_with_both_names() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('msg:' + ctx.text);
  },
  async onSchedule(ctx, name) {
    log('sched:' + name + ':' + ctx.name);
  },
  async onTimer(ctx, name) {
    log('legacy:' + name + ':' + ctx.name);
  },
});
"#
}

/// `hbot.json`：平台 `local`（不联网），计划表按参数声明。
///
/// 用 `serde_json` 拼而不是手写 `format!`：手拼的 JSON 一旦漏一个引号，
/// 失败会出现在 `Manifest::load` 里而看不出是测试自己的拼接问题（坑 96 的同族）。
fn manifest(schedules: serde_json::Value) -> String {
    serde_json::json!({
        "name": "schedule-bot",
        "platform": { "kind": "local" },
        "capabilities": { "timer": schedules },
    })
    .to_string()
}

/// 一条每秒触发的具名计划（返回长度为一的数组，直接喂给 [`manifest`]）。
fn every_second(name: &str) -> serde_json::Value {
    serde_json::json!([{
        "name": name,
        "when": { "kind": "every", "everyMs": 1000 },
    }])
}

/// 空计划表。
fn no_schedules() -> serde_json::Value {
    serde_json::json!([])
}

/// `bots.json`：只列 id + platform，`local` 不需要 url。
fn bots_json() -> String {
    format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"计划能力"}}]}}"#)
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn bot_id(value: &str) -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(value).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    project: PathBuf,
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
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            project,
            state,
            dispatcher,
            bot: bot_id(BOT_UUID),
        }
    }

    /// 重开核心（同一个数据目录与项目目录），用来钉「重启后计划仍会触发」。
    async fn restart_core(&mut self) {
        self.dispatcher = Dispatcher::new();
        let data_dir = self._dir.path().join("data");
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("重开 AppState");
        state.open_project(&self.project).await.expect("重开项目");
        let state = Arc::new(state);
        self.dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        self.state = state;
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

    /// 这个 bot 的全部日志行（`ORDER BY id DESC`，最新在前）。
    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 512)
            .expect("读日志")
    }

    /// 计划钩子自己写的前缀行（`sched:<name>:<ctx.name>`）。
    fn schedule_lines(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.line.starts_with(SCHEDULE_PREFIX))
            .map(|row| row.line)
            .collect()
    }

    /// 所有 error 级别的行。
    fn errors(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.level == "error")
            .map(|row| row.line)
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
            )
        }
    }
}

/// 轮询等到 `predicate` 为真。tick 循环是 1 秒一拍，所以这里要给足时间。
async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 等一段时间，用来证明「某件事没有发生」。
async fn settle(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

// ---------------------------------------------------------------------------
// ① 写 `onSchedule` 的具名计划 → 到点真的跑起来
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_named_schedule_fires_the_on_schedule_hook() {
    let harness = Harness::new(
        "fires",
        schedule_handler(),
        &manifest(every_second("every-minute")),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("计划写出第一条日志", || {
        !harness.schedule_lines().is_empty()
    })
    .await;
    assert_eq!(
        harness.schedule_lines().last().map(String::as_str),
        Some("sched:every-minute:every-minute"),
        "`ctx.name` 与第二个参数应当是同一条计划的名字，实际：{:?}",
        harness.schedule_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 没声明 → 一秒一拍也不干活
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_schedule_never_fires() {
    let harness = Harness::new("undeclared", schedule_handler(), &manifest(no_schedules())).await;

    assert_ok(&harness.start().await, "bot_start");
    settle(2_500).await;
    assert!(
        harness.schedule_lines().is_empty(),
        "没声明计划就不该有任何计划日志，实际：{:?}",
        harness.schedule_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 声明了却没写钩子 → 点名 `onSchedule`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_schedule_without_a_hook_names_on_schedule() {
    let harness = Harness::new(
        "missing-hook",
        handler_without_hook(),
        &manifest(every_second("tick")),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("报出缺钩子", || {
        harness
            .errors()
            .iter()
            .any(|line| line.contains("onSchedule") && line.contains("tick"))
    })
    .await;
    assert!(
        harness.schedule_lines().is_empty(),
        "没有钩子就不该有计划日志，实际：{:?}",
        harness.schedule_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 两个名字都写 → 启动失败，不静默挑一个
// ---------------------------------------------------------------------------

#[tokio::test]
async fn defining_both_hook_names_fails_the_start() {
    let harness = Harness::new(
        "both-names",
        handler_with_both_names(),
        &manifest(every_second("tick")),
    )
    .await;

    let response = harness.start().await;
    match response {
        Response::Ok { .. } => panic!("两个名字都写必须启动失败，实际成功了"),
        Response::Error { error } => {
            assert!(
                error.message.contains("onSchedule") && error.message.contains("onTimer"),
                "错误必须同时点名两个钩子，实际：{}",
                error.message
            );
        }
    }
    // 失败后不该留下一个看起来在跑的 bot。
    assert!(
        !harness.state.bots().is_running(&harness.bot),
        "启动失败后 bot 不该处于 running"
    );
}

// ---------------------------------------------------------------------------
// ⑤ 只写旧名 `onTimer` → 仍然触发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_legacy_timer_name_still_fires() {
    let harness = Harness::new("legacy", legacy_handler(), &manifest(every_second("tick"))).await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("旧名也能触发", || {
        !harness.schedule_lines().is_empty()
    })
    .await;
    assert_eq!(
        harness.schedule_lines().last().map(String::as_str),
        Some("sched:tick:tick"),
        "旧名的派发路径与新名完全一致，实际：{:?}",
        harness.schedule_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 重启核心后计划仍会触发（触发点按清单重新对齐，不落库）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_schedule_fires_again_after_the_core_restarts() {
    let mut harness = Harness::new(
        "restart",
        schedule_handler(),
        &manifest(every_second("tick")),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("重启前先触发一次", || {
        !harness.schedule_lines().is_empty()
    })
    .await;
    assert_ok(&harness.stop().await, "bot_stop");
    let before = harness.schedule_lines().len();

    harness.restart_core().await;
    assert_ok(&harness.start().await, "重启后 bot_start");
    wait_for("重启后仍然触发", || {
        harness.schedule_lines().len() > before
    })
    .await;
    assert!(
        harness.schedule_lines().len() > before,
        "重启后应当继续触发，实际：{:?}",
        harness.schedule_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}
