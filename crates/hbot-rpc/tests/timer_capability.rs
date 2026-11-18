//! 定时能力（`onTimer`）在**生产装配路径**上的端到端测试。
//!
//! 三侧单元测试已各自覆盖：`hbot-core` 的计划规则与参数闸（`bot_timer.rs`）、
//! `hbot-code` 的槽位与钩子派发（`src/timer.rs` + `tests/s2_timer.rs`）。本文件补的是
//! 它们之间**唯一没有别处覆盖的那一段**：
//!
//! `AppState::start_bot` 起 tick 循环 → `SandboxHandler::tick` 按
//! `declared_schedules(&project_root)` 决定要不要干活 → 取到点条目 → 在**同一个**
//! isolate 上派发 → 跑日志落库 / 回复进 outbox。
//!
//! 漏掉 tick 循环时，声明了计划的项目永远不会触发，而上面每一层的单元测试都还是绿的
//! —— 所以这里必须走 `Dispatcher` 真派发 + 真时钟，不能直接构造 handler。
//!
//! 七类断言：
//!
//! 1. 声明了计划 → 到点真的跑起来，`ctx.log` 的行落进 `logs` 表；
//! 2. 没声明 → tick 一秒一次也不编译、不起 isolate，日志里没有任何定时器痕迹；
//! 3. 声明了但没写 `onTimer` → 每次到点记一行说明缺钩子的 error；
//! 4. 计划声明了 `conversation` → `ctx.reply` 的回复真进 outbox（落成 `[reply]` 日志）；
//! 5. 没声明 `conversation` 却回复 → 明确报"计划没有声明 conversation"，不静默丢弃；
//! 6. 声明的会话还没出现过 → 明确报"这个会话从没收到过消息"；
//! 7. `bot_stop` 之后 tick 循环被 abort，不再产生新的定时器日志。

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

const BOT_UUID: &str = "55555555-5555-4555-8555-555555555555";

/// 本地试跑的固定外部会话 ID（与 `hbot-adapters` 的常量一字不差）。
const LOCAL_EXTERNAL_ID: &str = "local:default";

/// 回复前缀（与 `hbot-adapters` 的 `REPLY_PREFIX` 一致）。
const REPLY_PREFIX: &str = "[reply] ";

/// 定时器日志行的前缀（handler 里自己写的前缀，测试用它筛出定时器产生的行）。
const TIMER_PREFIX: &str = "timer:";

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
            "hbot-timercap-{label}-{}-{seq}",
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

/// 只记日志的定时器 handler（不回复，用来钉"到点真的跑了"）。
fn logging_timer() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log(`msg:${ctx.text}`);
  },
  async onTimer(ctx, name) {
    log(`timer:${name}`);
  },
});
"#
}

/// 会回复的定时器 handler（用来钉"回复进了哪个会话"）。
fn replying_timer() -> &'static str {
    r#"
import { defineBot, reply, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log(`msg:${ctx.text}`);
  },
  async onTimer(ctx, name) {
    log(`timer:${name}`);
    reply(`timer-reply:${name}`);
  },
});
"#
}

/// 只有 `onMessage` 的 handler（用来钉"声明了计划却没写钩子"）。
fn no_timer_hook() -> &'static str {
    r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log(`msg:${ctx.text}`);
  },
});
"#
}

/// `hbot.json`：平台 `local`（不联网），定时计划按参数声明。
fn manifest(schedules: &str) -> String {
    format!(
        "{{\"name\":\"timer-bot\",\"platform\":{{\"kind\":\"local\"}},\
         \"capabilities\":{{\"timer\":[{schedules}]}}}}\n"
    )
}

/// 一条每秒触发的计划，可带目标会话。
fn every_second(name: &str, conversation: Option<&str>) -> String {
    match conversation {
        Some(target) => format!(
            "{{\"name\":\"{name}\",\"when\":{{\"kind\":\"every\",\"everyMs\":1000}},\
             \"conversation\":\"{target}\"}}"
        ),
        None => format!("{{\"name\":\"{name}\",\"when\":{{\"kind\":\"every\",\"everyMs\":1000}}}}"),
    }
}

/// `bots.json`：只列 id + platform，`local` 不需要 url。
fn bots_json() -> String {
    format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"定时能力"}}]}}"#)
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn bot_id(value: &str) -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(value).expect("bot uuid"))
}

struct Harness {
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
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot: bot_id(BOT_UUID),
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

    /// 这个 bot 的全部日志行（`ORDER BY id DESC`，最新在前）。
    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 512)
            .expect("读日志")
    }

    /// 定时器自己写的前缀行（`timer:<name>`）。
    fn timer_lines(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.line.starts_with(TIMER_PREFIX))
            .map(|row| row.line)
            .collect()
    }

    /// 以 `[reply] ` 开头的行（去掉前缀）。
    fn replies(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
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

/// 等一段时间，用来证明"某件事没有发生"。
async fn settle(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

// ---------------------------------------------------------------------------
// ① 声明了计划 → 到点真的跑起来
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_timer_fires_and_writes_run_logs() {
    let harness = Harness::new(
        "fires",
        logging_timer(),
        &manifest(&every_second("tick", None)),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("定时器写出第一条日志", || {
        !harness.timer_lines().is_empty()
    })
    .await;
    assert_eq!(
        harness.timer_lines().last().map(String::as_str),
        Some("timer:tick"),
        "定时器日志应该是 handler 里那行 ctx.log，实际：{:?}",
        harness.timer_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 没声明 → 一秒一次也不干活
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_timer_never_fires() {
    let harness = Harness::new("undeclared", logging_timer(), &manifest("")).await;

    assert_ok(&harness.start().await, "bot_start");
    // 两秒足够 tick 循环跑两拍：真在干活的话至少落一行。
    settle(2_500).await;
    assert!(
        harness.timer_lines().is_empty(),
        "没声明计划就不该有任何定时器日志，实际：{:?}",
        harness.timer_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 声明了但没写 `onTimer` → 明确报缺钩子
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_timer_without_the_hook_names_the_missing_function() {
    let harness = Harness::new(
        "missing-hook",
        no_timer_hook(),
        &manifest(&every_second("tick", None)),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("报出缺钩子", || {
        harness
            .errors()
            .iter()
            .any(|line| line.contains("onTimer") && line.contains("tick"))
    })
    .await;
    // 静默成功是最坏的一种：用户以为计划在跑，其实每次到点都什么都没做。
    assert!(
        harness.timer_lines().is_empty(),
        "没有钩子就不该有定时器日志，实际：{:?}",
        harness.timer_lines()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 声明了 `conversation` → 回复进 outbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_timer_reply_reaches_the_declared_conversation() {
    let harness = Harness::new(
        "reply",
        replying_timer(),
        &manifest(&every_second("tick", Some(LOCAL_EXTERNAL_ID))),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    // `local` 平台的回复由适配器写成 `[reply] <text>` 的 run log 行，
    // 所以"进了 outbox 并真的发出去"在这里就是"出现这条日志"。
    wait_for("定时器的回复落地", || {
        harness
            .replies()
            .iter()
            .any(|line| line == "timer-reply:tick")
    })
    .await;
    assert!(
        harness
            .replies()
            .iter()
            .any(|line| line == "timer-reply:tick"),
        "回复丢了，实际 replies：{:?}",
        harness.replies()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ 没声明 `conversation` 却回复 → 明确报错，不静默丢
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_timer_reply_without_a_target_is_reported_not_swallowed() {
    let harness = Harness::new(
        "no-target",
        replying_timer(),
        &manifest(&every_second("tick", None)),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("报出没有目标会话", || {
        harness
            .errors()
            .iter()
            .any(|line| line.contains("conversation") && line.contains("tick"))
    })
    .await;
    assert!(
        !harness
            .replies()
            .iter()
            .any(|line| line == "timer-reply:tick"),
        "没有目标就不该真的发出去，实际 replies：{:?}",
        harness.replies()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 目标会话还不存在 → 明确报"这个会话从没收到过消息"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_timer_reply_to_an_unknown_conversation_is_reported() {
    let harness = Harness::new(
        "unknown-target",
        replying_timer(),
        &manifest(&every_second("tick", Some("99999999"))),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("报出目标会话还不存在", || {
        harness
            .errors()
            .iter()
            .any(|line| line.contains("99999999") && line.contains("从没收到过消息"))
    })
    .await;
    assert!(
        !harness
            .replies()
            .iter()
            .any(|line| line == "timer-reply:tick"),
        "会话不存在就不该往 outbox 里塞，实际 replies：{:?}",
        harness.replies()
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑦ `bot_stop` 之后不再触发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stopping_the_bot_stops_the_timer() {
    let harness = Harness::new(
        "stops",
        logging_timer(),
        &manifest(&every_second("tick", None)),
    )
    .await;

    assert_ok(&harness.start().await, "bot_start");
    wait_for("先触发至少一次", || {
        !harness.timer_lines().is_empty()
    })
    .await;
    assert_ok(&harness.stop().await, "bot_stop");
    let after_stop = harness.timer_lines().len();

    settle(2_500).await;
    assert_eq!(
        harness.timer_lines().len(),
        after_stop,
        "停止后 tick 循环应被 abort，实际又多出了日志：{:?}",
        harness.timer_lines()
    );
}
