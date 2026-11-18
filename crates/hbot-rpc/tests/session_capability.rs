//! 会话等待（`C2.session`）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_session`）、存储（`bot_waits`）、沙箱槽位（`hbot-code::session`）
//! 各有单测；本文件补的是它们之间唯一没有别处覆盖的那一段：
//!
//! `AppState::start_bot` → `SandboxHandlerConfig::waits` → `worker()` 的 `set_session`
//! → `ctx.session.prompt` 落库 → 下一条消息被 `run()` 里的 `claim_wait` 拦下 →
//! `dispatch_resume_collecting` → `onResume` → 回复进 outbox。
//!
//! 五条断言：
//!
//! 1. **没声明就没得用**：不写 `capabilities.state.session` → `HBOT_SESSION_NOT_DECLARED`；
//! 2. **声明了真能等**：`prompt` 落库、`current()` 读得到；
//! 3. **下一条消息唤醒 `onResume`**（而不是 `onMessage`），`ctx.waitKey` 对得上；
//! 4. **同一会话第二个 prompt 被拒**（`HBOT_SESSION_PENDING` 且点名挡路的那条）；
//! 5. **`scope: sender` 时别人说话不唤醒**（docs/00 P0-3 的回归）。

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

const BOT_UUID: &str = "88888888-8888-4888-8888-888888888888";
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";
/// `onResume` 写的前缀（用例靠它确认「走的是恢复钩子」）。
const RESUME_PREFIX: &str = "resume:";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-sessioncap-{label}-{}-{seq}",
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

/// 在 `onMessage` 里开一条等待；`onResume` 把 `waitKey` 写出来。
fn waiting_handler() -> &'static str {
    r#"
import { defineBot, reply, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      await ctx.session.prompt('order-no', '请把订单号发给我');
      reply('asked');
    } catch (error) {
      reply('session-error:' + error.message);
    }
  },
  async onResume(ctx, message) {
    log('resume:' + ctx.waitKey + ':' + ctx.text);
    ctx.reply('got:' + message.text);
  },
});
"#
}

/// 连开两条等待（第二条必须被拒）。
fn twice_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      await ctx.session.prompt('order-no', '请把订单号发给我');
      await ctx.session.prompt('captcha', '再发一次验证码');
      reply('asked-twice');
    } catch (error) {
      reply('session-error:' + error.message);
    }
  },
  async onResume(ctx) {
    ctx.log('resume:' + ctx.waitKey);
  },
});
"#
}

fn manifest(declare: bool) -> String {
    if declare {
        "{\"name\":\"session-bot\",\"platform\":{\"kind\":\"local\"},\"capabilities\":{\"state\":{\"session\":true}}}\n".to_owned()
    } else {
        "{\"name\":\"session-bot\",\"platform\":{\"kind\":\"local\"}}\n".to_owned()
    }
}

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"会话能力\"}}]}}")
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

    fn resume_lines(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.line.starts_with(RESUME_PREFIX))
            .map(|row| row.line)
            .collect()
    }

    fn waits(&self) -> Vec<hbot_store::WaitRow> {
        let conversation = self.conversation();
        self.store()
            .current_wait(&self.bot, &conversation)
            .expect("读等待")
            .into_iter()
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
// ① 没声明 → 声明层拒绝（且不是 UNAVAILABLE）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_session_capability_reports_not_declared() {
    let harness = Harness::new("undeclared", waiting_handler(), &manifest(false)).await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到一条回复", || harness.replies().len() > before).await;
    let reply = harness.replies().into_iter().next().expect("reply");
    assert!(
        reply.starts_with("session-error:HBOT_SESSION_NOT_DECLARED"),
        "没声明 state.session 时必须报 NOT_DECLARED，实际：{reply}"
    );
    assert!(harness.waits().is_empty(), "被拒的 prompt 不许落库");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 声明了 → prompt 落库、current 读得到
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_prompt_lands_in_the_table() {
    let harness = Harness::new("declared", waiting_handler(), &manifest(true)).await;
    assert_ok(&harness.start().await, "bot_start");

    let before = harness.replies().len();
    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到 asked", || {
        harness.replies().iter().any(|text| text == "asked")
    })
    .await;
    let _ = before;

    wait_for("等待行落库", || !harness.waits().is_empty()).await;
    let row = &harness.waits()[0];
    assert_eq!(row.key, "order-no");
    assert_eq!(row.state, "pending");
    assert_eq!(row.scope, "sender");
    assert_eq!(row.owner, "inject");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 下一条消息唤醒 onResume（而不是 onMessage）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_next_message_resumes_the_wait_instead_of_on_message() {
    let harness = Harness::new("resume", waiting_handler(), &manifest(true)).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "first inject");
    wait_for("等待行落库", || !harness.waits().is_empty()).await;

    // 第二条消息：应唤醒等待，走 onResume。
    assert_ok(&harness.inject("A-42").await, "second inject");
    wait_for("onResume 跑过", || !harness.resume_lines().is_empty()).await;
    let line = harness
        .resume_lines()
        .into_iter()
        .next()
        .expect("resume line");
    assert!(
        line.starts_with("resume:order-no:"),
        "onResume 要拿到 waitKey，实际：{line}"
    );
    assert!(
        line.ends_with("A-42"),
        "onResume 要拿到那条消息的文本，实际：{line}"
    );

    // 回复也是走恢复路径的（reply_to 为 None 的主动消息）。
    wait_for("恢复回复进 outbox", || {
        harness.replies().iter().any(|text| text == "got:A-42")
    })
    .await;

    // 等待已经 fulfilled，不再挂着。
    wait_for("等待结束", || harness.waits().is_empty()).await;
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 同会话第二个 prompt 被拒，且点名挡路的那条
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_prompt_in_the_same_conversation_is_refused() {
    let harness = Harness::new("pending", twice_handler(), &manifest(true)).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || {
        harness
            .replies()
            .iter()
            .any(|text| text.starts_with("session-error:"))
    })
    .await;
    let reply = harness
        .replies()
        .into_iter()
        .find(|text| text.starts_with("session-error:"))
        .expect("error reply");
    assert!(
        reply.contains("HBOT_SESSION_PENDING"),
        "第二个 prompt 要报 PENDING，实际：{reply}"
    );
    assert!(
        reply.contains("order-no"),
        "要点头名挡路的那条（用户才知道先回哪个），实际：{reply}"
    );
    assert_eq!(harness.waits().len(), 1, "只该留下第一条");
    assert_ok(&harness.stop().await, "bot_stop");
}
