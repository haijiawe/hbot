//! 出站能力（`C3.send⟧BT⟧ / `C3.notify⟧BT⟧）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_egress⟧BT⟧）、沙箱槽位（`hbot-code::send⟧BT⟧）、宿主端口
//! （`crates/hbot-rpc/src/app/runtime/send.rs⟧BT⟧）各有单测；本文件补的是它们之间唯一没有别处
//! 覆盖的那一段：
//!
//! `AppState::start_bot⟧BT⟧ → `SandboxHandler::with_send⟧BT⟧ → `worker()⟧BT⟧ 的 `set_egress⟧BT⟧
//! → `ctx.send⟧BT⟧ / `ctx.notify⟧BT⟧ → `StoreSendQueue⟧BT⟧ → supervisor 的 `replies⟧BT⟧ 通道 →
//! `outbound⟧BT⟧ → `local⟧BT⟧ 适配器把正文写成 `[reply] …⟧BT⟧ 日志行。
//!
//! 七条断言：
//!
//! 1. **没声明就没得用**：不写 `capabilities.send⟧BT⟧ → `HBOT_SEND_NOT_DECLARED⟧BT⟧；
//! 2. **声明了真能发**：`ctx.send⟧BT⟧ 的正文作为 `[reply] …⟧BT⟧ 行进 run log；
//! 3. **没有 `kind⟧BT⟧ 时查库**：`local:default⟧BT⟧ 是 `bot_start⟧BT⟧ 建过的会话，能发出；
//! 4. **库和调用点都没有 `kind⟧BT⟧ → 拒绝而不是猜**：`HBOT_SEND_NO_TARGET⟧BT⟧；
//! 5. **显式 `kind⟧BT⟧ 不需要会话行**：补建 `conversations⟧BT⟧ 行（外键要求），类型照调用点；
//! 6. **没声明 `notify⟧BT⟧ → `HBOT_NOTIFY_NOT_DECLARED⟧BT⟧**（与 `send⟧BT⟧ 分开判）；
//! 7. **`notify⟧BT⟧ 用清单目标**：正文发到 `capabilities.notify.conversation⟧BT⟧，不看参数。

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

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-sendcap-{label}-{}-{seq}", std::process::id()));
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

/// 主动发一条；结果写进 `[reply] ⟧BT⟧ 行，便于断言到底哪一步被拒。
fn send_handler(body: &'static str) -> String {
    format!(
        r#"
import {{ defineBot, reply }} from 'hbot';

export default defineBot({{
  async onMessage(ctx) {{
    try {{
      {body}
      reply('sent');
    }} catch (error) {{
      reply('send-error:' + error.message);
    }}
  }},
}});
"#
    )
}

fn manifest(capabilities: &str) -> String {
    format!(
        "{{\"name\":\"send-bot\",\"platform\":{{\"kind\":\"local\"}},\"capabilities\":{capabilities}}}\n"
    )
}

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"出站能力\"}}]}}")
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

    fn replies(&self) -> Vec<String> {
        self.store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    fn errors(&self) -> Vec<String> {
        self.replies()
            .into_iter()
            .filter(|text| text.starts_with("send-error:"))
            .collect()
    }

    /// 适配器收到的附件（local 平台写成 `[attachment] <kind> <file>` 行）。
    fn attachments(&self) -> Vec<String> {
        self.store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix("[attachment] ").map(str::to_owned))
            .collect()
    }

    fn project(&self) -> PathBuf {
        self._dir.path().join("project")
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
// ① 没声明 → 声明层拒绝
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_send_reports_not_declared() {
    let handler = send_handler("await ctx.send('local:default', 'hi');");
    let harness = Harness::new("undeclared", &handler, &manifest("{}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到一条错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("HBOT_SEND_NOT_DECLARED"),
        "没声明 capabilities.send 时必须报 NOT_DECLARED，实际：{error}"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 声明了 → 正文真的进了出站链路（local 适配器写成 `[reply] ⟧BT⟧ 行）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_send_reaches_the_outbox() {
    let handler = send_handler("await ctx.send('local:default', 'hello outbound');");
    let harness = Harness::new("declared", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到 sent", || {
        harness.replies().iter().any(|t| t == "sent")
    })
    .await;
    wait_for("正文进了出站链路", || {
        harness
            .replies()
            .iter()
            .any(|text| text == "hello outbound")
    })
    .await;
    assert!(harness.errors().is_empty(), "不该有错误回复");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 不给 `kind⟧BT⟧ → 查库补全（`local:default⟧BT⟧ 由 `bot_start⟧BT⟧ 建过）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_known_conversation_needs_no_explicit_kind() {
    let handler = send_handler("await ctx.send('local:default', 'looked up');");
    let harness = Harness::new("lookup", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    // `bot_start⟧BT⟧ 已经为 local 平台建了 `local:default⟧BT⟧ 行，所以调用点不必给 kind。
    assert!(
        harness
            .store()
            .get_conversation_by_external(&harness.bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .is_some(),
        "前提：bot_start 之后 local:default 行必须存在"
    );

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("正文进了出站链路", || {
        harness.replies().iter().any(|text| text == "looked up")
    })
    .await;
    assert!(harness.errors().is_empty(), "查得到类型时不该报错");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ④ 库和调用点都没有 `kind⟧BT⟧ → NO_TARGET（不猜）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_conversation_without_kind_is_refused() {
    let handler = send_handler("await ctx.send('ghost:42', 'nobody');");
    let harness = Harness::new("notarget", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("HBOT_SEND_NO_TARGET"),
        "类型不能确定时必须报 NO_TARGET，实际：{error}"
    );
    assert!(
        error.contains("kind"),
        "错误要给出补救动作（补 kind），实际：{error}"
    );
    assert!(
        harness
            .store()
            .get_conversation_by_external(&harness.bot, "ghost:42")
            .expect("读会话")
            .is_none(),
        "被拒的发送不许留下会话行"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ 显式 `kind⟧BT⟧ → 补建会话行（外键要求），类型照调用点
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_kind_creates_the_missing_conversation_row() {
    let handler = send_handler("await ctx.send('group:999', 'into the group', { kind: 'group' });");
    let harness = Harness::new("explicit", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("正文进了出站链路", || {
        harness
            .replies()
            .iter()
            .any(|text| text == "into the group")
    })
    .await;

    let row = harness
        .store()
        .get_conversation_by_external(&harness.bot, "group:999")
        .expect("读会话")
        .expect("主动消息必须补建会话行（outbound 有外键）");
    assert_eq!(row.kind, "group", "类型必须照调用点给的，不许猜成私聊");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 没声明 `notify⟧BT⟧ → NOT_DECLARED（与 send 分开判）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_notify_reports_not_declared() {
    let handler = send_handler("await ctx.notify('notice');");
    let harness = Harness::new("nonotify", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("HBOT_NOTIFY_NOT_DECLARED"),
        "只声明 send 时 notify 仍必须报 NOT_DECLARED，实际：{error}"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑦ 声明了 `notify⟧BT⟧ → 正文发到清单目标
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notify_uses_the_declared_target() {
    let handler = send_handler("await ctx.notify('admin notice');");
    let capabilities = "{\"notify\":{\"conversation\":\"local:default\",\"kind\":\"private\"}}";
    let harness = Harness::new("notify", &handler, &manifest(capabilities)).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("通知正文进了出站链路", || {
        harness.replies().iter().any(|text| text == "admin notice")
    })
    .await;
    assert!(harness.errors().is_empty(), "声明了目标时不该报错");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑧ 附件真的走到了适配器（base64 段的构造在适配器那侧，这里只验链路）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_attachment_reaches_the_adapter() {
    let handler = send_handler("await ctx.sendImage('local:default', 'assets/logo.png');");
    let harness = Harness::new("attach", &handler, &manifest("{\"send\":true}")).await;
    std::fs::create_dir_all(harness.project().join("assets")).expect("创建 assets");
    // 一段最小的 PNG 头：内容是无关的，这里钉的是「字节从项目里读出来了」。
    std::fs::write(
        harness.project().join("assets").join("logo.png"),
        [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    )
    .expect("写附件");
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("附件到了适配器", || {
        harness
            .attachments()
            .iter()
            .any(|line| line == "image logo.png")
    })
    .await;
    assert!(harness.errors().is_empty(), "声明了附件路径合法时不该报错");
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑨ 参数闸：绝对路径 / 越界路径在入队前就被拒（HBOT_SEND_INVALID）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absolute_attachment_path_is_refused() {
    let handler = send_handler("await ctx.sendImage('local:default', 'C:/Windows/win.ini');");
    let harness = Harness::new("attach-abs", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("HBOT_SEND_INVALID"),
        "绝对路径必须报 HBOT_SEND_INVALID，实际：{error}"
    );
    assert!(harness.attachments().is_empty(), "被拒的附件不该走到适配器");
    assert_ok(&harness.stop().await, "bot_stop");
}

#[tokio::test]
async fn an_attachment_path_that_escapes_the_project_is_refused() {
    let handler = send_handler("await ctx.sendFile('local:default', 'assets/../../outside.txt');");
    let harness = Harness::new("attach-esc", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("HBOT_SEND_INVALID"),
        "带 .. 的路径必须报 HBOT_SEND_INVALID，实际：{error}"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑩ 宿主闸：文件不存在 / 超大小上限（这两条参数闸看不出来）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_attachment_is_refused_by_the_host() {
    let handler = send_handler("await ctx.sendImage('local:default', 'assets/missing.png');");
    let harness = Harness::new("attach-missing", &handler, &manifest("{\"send\":true}")).await;
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("都不存在") || error.contains("不存在"),
        "不存在的附件必须报出来，实际：{error}"
    );
    assert!(
        harness.attachments().is_empty(),
        "不存在的附件不该走到适配器"
    );
    assert_ok(&harness.stop().await, "bot_stop");
}

#[tokio::test]
async fn an_oversized_attachment_is_refused_by_the_host() {
    let handler = send_handler("await ctx.sendFile('local:default', 'assets/huge.bin');");
    let harness = Harness::new("attach-huge", &handler, &manifest("{\"send\":true}")).await;
    std::fs::create_dir_all(harness.project().join("assets")).expect("创建 assets");
    let too_big = vec![0u8; (hbot_adapters::onebot::MAX_OUTBOUND_ATTACHMENT_BYTES + 1) as usize];
    std::fs::write(harness.project().join("assets").join("huge.bin"), too_big).expect("写大文件");
    assert_ok(&harness.start().await, "bot_start");

    assert_ok(&harness.inject("hello").await, "bot_inject");
    wait_for("收到错误回复", || !harness.errors().is_empty()).await;
    let error = harness.errors().into_iter().next().expect("error reply");
    assert!(
        error.contains("上限"),
        "超限的附件必须报出上限，实际：{error}"
    );
    assert!(harness.attachments().is_empty(), "超限的附件不该走到适配器");
    assert_ok(&harness.stop().await, "bot_stop");
}
