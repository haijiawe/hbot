//! 生命周期钩子（`C1.lifecycle`）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_lifecycle`）、沙箱侧（`hbot-code` 的 `onStart`/`onStop`）各有单测；
//! 本文件补的是它们之间唯一没有别处覆盖的那一段：
//!
//! `AppState::start_bot` → `SandboxHandler::run_lifecycle(Start)`（在 supervisor 起来之后、
//! 登记之前）→ `onStart`；`bot_stop` → `run_lifecycle(Stop, User)` → `onStop`。
//!
//! 五条断言：
//!
//! 1. **`onStart` 真的在启动时跑一次**（`ctx.log` 落库）；
//! 2. **`onStart` 抛错 = 启动失败**（`bot_start` 返回错误，而不是「起来了但没初始化完」）；
//! 3. **`onStop` 拿到 `reason = 'user'`**（第二个参数与 `ctx.reason` 两处都对）；
//! 4. **`onStop` 抛错不影响停机**（`bot_stop` 仍然成功，错误只落日志）；
//! 5. **没写钩子不是错误**（既不报错也不留错误日志）；
//! 6. **进程退出/切项目走 `shutdown` 而不是 `user`**（`AppState::shutdown()` 的整条链路）。

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

const BOT_UUID: &str = "44444444-4444-4444-8444-444444444444";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-lifecap-{label}-{}-{seq}", std::process::id()));
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

fn manifest() -> String {
    "{\"name\":\"life-bot\",\"platform\":{\"kind\":\"local\"}}\n".to_owned()
}

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"生命周期\"}}]}}")
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
    async fn new(label: &str, handler_source: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        std::fs::write(project.join("hbot.json"), manifest()).expect("写 hbot.json");
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
            bot: bot_id(),
        }
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

    fn logs(&self) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
            .into_iter()
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
            );
        }
    }
}

fn assert_err(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => panic!("{what} 期望失败，实际成功"),
        Response::Error { .. } => {}
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

// ---------------------------------------------------------------------------
// ① onStart 在启动时跑一次
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_start_runs_once_when_the_bot_starts() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onStart(ctx) {
    log('start:' + ctx.phase);
  },
  async onMessage(ctx) {
    log('message');
  },
});
"#;
    let harness = Harness::new("start", handler).await;
    assert_ok(&harness.start().await, "bot_start");

    wait_for("onStart 的日志", || {
        harness.logs().iter().any(|line| line == "start:start")
    })
    .await;
    assert_eq!(
        harness
            .logs()
            .iter()
            .filter(|line| line.starts_with("start:"))
            .count(),
        1,
        "onStart 只该跑一次"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② onStart 抛错 = 启动失败
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_throwing_on_start_fails_the_bot_start() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onStart(ctx) {
    ctx.log('start:about-to-throw');
    throw new Error('init failed');
  },
  async onMessage(ctx) {
    log('message');
  },
});
"#;
    let harness = Harness::new("startfail", handler).await;
    assert_err(&harness.start().await, "onStart 抛错时 bot_start");

    // 抛错前写的那行 log 也要落库：它是唯一的排障线索。
    wait_for("onStart 抛错前的日志", || {
        harness
            .logs()
            .iter()
            .any(|line| line == "start:about-to-throw")
    })
    .await;
    // 失败要落一行点名钩子的 error。
    assert!(
        harness.logs().iter().any(|line| line.contains("onStart")),
        "失败要点名 onStart，实际日志：{:?}",
        harness.logs()
    );
}

// ---------------------------------------------------------------------------
// ③ onStop 拿到 reason = user
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_stop_receives_the_user_reason() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('message');
  },
  async onStop(ctx, reason) {
    log('stop:' + ctx.phase + ':' + reason + ':' + ctx.reason);
  },
});
"#;
    let harness = Harness::new("stop", handler).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_ok(&harness.stop().await, "bot_stop");

    wait_for("onStop 的日志", || {
        harness.logs().iter().any(|line| line.starts_with("stop:"))
    })
    .await;
    let line = harness
        .logs()
        .into_iter()
        .find(|line| line.starts_with("stop:"))
        .expect("onStop 日志");
    assert_eq!(
        line, "stop:stop:user:user",
        "第二个参数与 ctx.reason 都必须是 user"
    );
}

// ---------------------------------------------------------------------------
// ④ onStop 抛错不影响停机
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_throwing_on_stop_does_not_break_the_stop() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('message');
  },
  async onStop(ctx, reason) {
    ctx.log('stop:before-throw:' + reason);
    throw new Error('cleanup failed');
  },
});
"#;
    let harness = Harness::new("stopfail", handler).await;
    assert_ok(&harness.start().await, "bot_start");
    // 停机本身必须成功：`onStop` 失败只落日志。
    assert_ok(&harness.stop().await, "bot_stop");

    wait_for("onStop 抛错前的日志", || {
        harness
            .logs()
            .iter()
            .any(|line| line.starts_with("stop:before-throw:"))
    })
    .await;
    assert!(
        harness.logs().iter().any(|line| line.contains("onStop")),
        "失败要点名 onStop，实际日志：{:?}",
        harness.logs()
    );
}

// ---------------------------------------------------------------------------
// ⑤ 没写钩子不是错误
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_project_without_lifecycle_hooks_starts_and_stops_quietly() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('message');
  },
});
"#;
    let harness = Harness::new("nohooks", handler).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_ok(&harness.stop().await, "bot_stop");

    assert!(
        harness
            .logs()
            .iter()
            .all(|line| !line.contains("onStart") && !line.contains("onStop")),
        "没写钩子不该留下任何点名钩子的错误，实际日志：{:?}",
        harness.logs()
    );
}

// ---------------------------------------------------------------------------
// ⑥ AppState::shutdown 把 reason = shutdown 交给 onStop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn app_state_shutdown_hands_the_shutdown_reason_to_on_stop() {
    let handler = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('message');
  },
  async onStop(ctx, reason) {
    log('stop:' + ctx.phase + ':' + reason + ':' + ctx.reason);
  },
});
"#;
    let harness = Harness::new("shutdown", handler).await;
    assert_ok(&harness.start().await, "bot_start");

    // 进程退出 / 切项目走的是这一条：`onStop` 必须收到 `shutdown`，
    // 否则「Ctrl-C 之后什么都没保存」会变成一类无解的现象。
    harness.state.shutdown().await;

    wait_for("onStop 的 shutdown 日志", || {
        harness.logs().iter().any(|line| line.starts_with("stop:"))
    })
    .await;
    let line = harness
        .logs()
        .into_iter()
        .find(|line| line.starts_with("stop:"))
        .expect("onStop 日志");
    assert_eq!(line, "stop:stop:shutdown:shutdown", "退出路径不能报成 user");
    assert!(
        !harness.state.bots().is_running(&harness.bot),
        "shutdown 之后 bot 必须已从登记表里摘掉"
    );
}
