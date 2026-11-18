//! 状态能力（`ctx.kv` / `ctx.history`）在**生产装配路径**上的端到端测试。
//!
//! 三侧单元测试已各自覆盖：`hbot-core` 的契约与参数闸、`hbot-store` 的 `bot_kv` 表与
//! history 查询、`hbot-code` 的桥（`tests/s2_state.rs`）、`hbot-rpc` 的
//! `StoreBotState`（`src/app/state/mod.rs` 的 `mod tests`）。本文件补的是它们之间
//! **唯一没有别处覆盖的那一段**：
//!
//! `AppState::start_bot` → `SandboxHandlerConfig::state` → `SandboxHandler::worker`
//! 里那句 `source.handles(&project_root)` + `sandbox.set_state(...)` → isolate。
//!
//! 漏掉那句接线时，`ctx.kv` 会一路报 `HBOT_STATE_UNAVAILABLE`，而上面每一层的单元
//! 测试都还是绿的 —— 所以这里必须走 `Dispatcher` 真派发，不能直接构造 handler。
//!
//! 六类断言：
//!
//! 1. **没声明就没得用**：`hbot.json` 不写 `capabilities.state` → `ctx.kv.get` 抛
//!    `HBOT_STATE_NOT_DECLARED`；
//! 2. **声明了真能用**：写上 `capabilities.state.kv` → 同一个 handler 写出后读回；
//! 3. **状态跨 isolate 存活**：`bot_stop` → `bot_start`（换一个全新 isolate）后
//!    上一次写进去的值还在（证明状态在 SQLite 里，不在 JS 堆上）；
//! 4. **状态跨宿主进程存活**：整个 `AppState` 重开、`Store` 换新，值仍在；
//! 5. **只改清单也生效**：不动任何 `.ts`，只把 `capabilities.state.kv` 改成 `true`，
//!    新声明也会被用上（清单参与热重载指纹）；
//! 6. **两个 bot 互不可见**：同一份 `Store`、同一个键名，第二个 bot 读到的是 `null`。

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

const BOT_UUID: &str = "33333333-3333-4333-8333-333333333333";
const SECOND_BOT_UUID: &str = "44444444-4444-4444-8444-444444444444";

/// 本地试跑的固定外部会话 ID（与 `hbot-adapters` 的常量一字不差）。
const LOCAL_EXTERNAL_ID: &str = "local:default";

/// 回复前缀（与 `hbot-adapters` 的 `REPLY_PREFIX` 一致）。
const REPLY_PREFIX: &str = "[reply] ";

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
            "hbot-statecap-{label}-{}-{seq}",
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

/// 写一个键、再读回来，把结果回复出去。
///
/// 用 `error.message` 而不是 `String(error)`：`Exception::throw_message` 造的是
/// `Error` 实例，`String()` 会多一层 `Error: ` 前缀（`hbot-code` 的 `tests/s2_state.rs`
/// 踩过这个坑）。
fn kv_handler() -> String {
    r#"
import { defineBot, reply, kv } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      const before = await kv.get('probe');
      await kv.set('probe', { round: (before?.round ?? 0) + 1 });
      const after = await kv.get('probe');
      reply(`kv:${JSON.stringify(after)}`);
    } catch (error) {
      reply(`kv-error:${error.message}`);
    }
  },
});
"#
    .to_owned()
}

/// 只读历史条数（不关心内容，只证明 `ctx.history` 能通）。
fn history_handler() -> &'static str {
    r#"
import { defineBot, reply, history } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      const rows = await history(5);
      reply(`history:${rows.length}`);
    } catch (error) {
      reply(`history-error:${error.message}`);
    }
  },
});
"#
}

/// `hbot.json`：平台 `local`（不联网），状态能力按参数声明。
fn manifest(declare_kv: bool, declare_history: bool) -> String {
    format!(
        "{{\"name\":\"state-bot\",\"platform\":{{\"kind\":\"local\"}},\
         \"capabilities\":{{\"state\":{{\"kv\":{declare_kv},\"history\":{declare_history}}}}}}}\n"
    )
}

/// `bots.json`：只列 id + platform，`local` 不需要 url。
fn bots_json(ids: &[&str]) -> String {
    let entries: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"id":"{id}","platform":"local","name":"状态能力"}}"#))
        .collect();
    format!(r#"{{"bots":[{}]}}"#, entries.join(","))
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn bot_id(value: &str) -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(value).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    /// 数据库所在目录：`reopen` 要拿它重新 `AppState::open`（同一个库，新进程）。
    data_dir: PathBuf,
    project: PathBuf,
    /// 清单的绝对路径：热重载测试要像用户那样直接改它。
    manifest: PathBuf,
    bots: Vec<BotId>,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    async fn new(label: &str, handler_source: &str, manifest_json: &str, bot_ids: &[&str]) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        std::fs::write(project.join("hbot.json"), manifest_json).expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json(bot_ids)).expect("写 bots.json");

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir.clone(), Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            data_dir,
            manifest: project.join("hbot.json"),
            project,
            bots: bot_ids.iter().map(|id| bot_id(id)).collect(),
            state,
            dispatcher,
            bot: bot_id(bot_ids[0]),
        }
    }

    /// 像用户在 IDE 里那样只改清单（一个 `.ts` 字节都不动）。
    fn write_manifest(&self, manifest_json: &str) {
        std::fs::write(&self.manifest, manifest_json).expect("改写 hbot.json");
    }

    /// 模拟宿主进程重启：把这一份 `AppState`（连同它的 `Store`、所有 isolate）整个丢掉，
    /// 在**同一个** `data_dir` 上重新 `AppState::open`，再用新的 `Dispatcher` 接管。
    ///
    /// 和 `bot_stop` → `bot_start` 的区别：那条只换 isolate，`Store` 与连接池还是同一份；
    /// 这条连进程内缓存都换了，剩下的只有磁盘上的 `hbot.db`。
    async fn reopen(&mut self) {
        for bot in self.bots.clone() {
            let _ = self.stop(bot).await;
        }
        let fresh = AppState::open(
            self.data_dir.clone(),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("重开 AppState");
        fresh.open_project(&self.project).await.expect("重开项目");
        let fresh = Arc::new(fresh);
        self.dispatcher = Dispatcher::new().with_state(Arc::clone(&fresh));
        self.state = fresh;
    }

    fn store(&self) -> &Arc<hbot_store::Store> {
        self.state.store()
    }

    async fn start(&self, bot: BotId) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: bot })
            .await
    }

    async fn stop(&self, bot: BotId) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: bot })
            .await
    }

    fn local_conversation(&self, bot: BotId) -> ConversationId {
        self.store()
            .get_conversation_by_external(&bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .unwrap_or_else(|| {
                panic!("bot_start 之后应存在 external_id = {LOCAL_EXTERNAL_ID} 的会话行")
            })
            .id
    }

    async fn inject(&self, bot: BotId, conversation: ConversationId, text: &str) -> Response {
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: bot,
                conversation_id: conversation,
                text: text.to_owned(),
            })
            .await
    }

    /// run log 里以 `[reply] ` 开头的行（去掉前缀），**只要这个 bot 的**。
    fn replies(&self, bot: BotId) -> Vec<String> {
        self.store()
            .list_logs(Some(&bot), 512)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    /// 库里 `probe` 这个键的原始文本（诊断与"真的落库了吗"的物证）。
    fn stored_probe(&self, bot: BotId) -> Option<String> {
        let now = self.state.clock().now().unix_millis();
        self.store().get_kv(&bot, "probe", now).expect("读库")
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

/// 轮询等到 `predicate` 为真（队列 / 沙箱 / outbox 都在别的任务里推进）。
async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 启动 → 注入一条 → 等到一条新回复 → 返回这条回复。
async fn run_once(harness: &Harness, bot: BotId, text: &str) -> String {
    assert_ok(&harness.start(bot).await, "bot_start");
    deliver(harness, bot, text).await
}

/// 给**已启动**的 bot 投一条消息并等回复。
///
/// 不能再调 `bot_start`：重复启动会撞 `-32003`（`commands::bots::start` 用占位挡并发）。
async fn deliver(harness: &Harness, bot: BotId, text: &str) -> String {
    let conversation = harness.local_conversation(bot);
    let before = harness.replies(bot).len();
    assert_ok(&harness.inject(bot, conversation, text).await, "bot_inject");
    wait_for("收到一条回复", || harness.replies(bot).len() > before).await;
    // 日志表是 `ORDER BY id DESC`：最后写入的那条排在最前。
    harness
        .replies(bot)
        .into_iter()
        .next()
        .expect("刚等到的那条回复")
}

// ---------------------------------------------------------------------------
// ① 没声明 → 声明层拒绝（且不是"未装配"，也不是静默成功）
// ---------------------------------------------------------------------------

/// `hbot.json` 没有 `capabilities.state`：`ctx.kv.get` 必须报 `NOT_DECLARED`。
///
/// 这条同时钉住"装配确实发生了"：如果 `SandboxHandler` 忘了 `set_state`，错误会是
/// `HBOT_STATE_UNAVAILABLE`（未装配），测试就会因为码不对而红 —— 也就是说它不能
/// 被"接线漏了"蒙混过去。
#[tokio::test]
async fn an_undeclared_kv_is_refused_by_the_declaration_check_not_by_a_missing_port() {
    let harness = Harness::new(
        "undeclared",
        &kv_handler(),
        &manifest(false, false),
        &[BOT_UUID],
    )
    .await;

    let reply = run_once(&harness, harness.bot, "读一下").await;
    assert!(
        reply.starts_with("kv-error:HBOT_STATE_NOT_DECLARED:"),
        "应该是声明层的拒绝，实际：{reply}"
    );
    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

/// 同样的项目，只声明 `history` 不声明 `kv`：两个能力各自独立判。
#[tokio::test]
async fn kv_and_history_are_declared_separately() {
    let harness = Harness::new(
        "separate",
        history_handler(),
        &manifest(false, true),
        &[BOT_UUID],
    )
    .await;

    let reply = run_once(&harness, harness.bot, "看看历史").await;
    // 本次事件刚被注入，历史里至少有它自己这一条。
    assert!(
        reply.starts_with("history:") && !reply.starts_with("history-error:"),
        "声明了 history 就该能用，实际：{reply}"
    );
    let count: usize = reply
        .trim_start_matches("history:")
        .parse()
        .unwrap_or_else(|_| panic!("回复格式应该是 history:<n>，实际：{reply}"));
    assert!(count >= 1, "至少应含本次注入那一条，实际：{reply}");

    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ② 声明了真能用（写 → 读回）
// ---------------------------------------------------------------------------

/// 声明 `capabilities.state.kv` 之后，同一个 handler 里 `set` 完立刻 `get` 能读回。
#[tokio::test]
async fn a_declared_kv_can_be_written_and_read_back_inside_one_handler() {
    let harness = Harness::new(
        "declared",
        &kv_handler(),
        &manifest(true, false),
        &[BOT_UUID],
    )
    .await;

    let reply = run_once(&harness, harness.bot, "写一个").await;
    assert_eq!(reply, r#"kv:{"round":1}"#, "第一次写应得到 round=1");
    assert_eq!(
        harness.stored_probe(harness.bot).as_deref(),
        Some(r#"{"round":1}"#),
        "写完之后库里应该有这一行（否则 ctx.kv.set 根本没落库）"
    );

    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ③ 状态跨 isolate 存活
// ---------------------------------------------------------------------------

/// `bot_stop` → `bot_start` 换一个全新 isolate，上一次写进去的值还在。
///
/// 这是"状态在 SQLite 里而不是在 JS 堆上"的直接证据：`bot_stop` 会把 isolate 连同
/// 它的 QuickJS 运行时整个丢掉（`Sandbox::Drop` 等 OS 线程退出），新一轮启动是全新
/// 的 `Sandbox` 与全新的 isolate。
#[tokio::test]
async fn kv_survives_a_bot_restart() {
    let harness = Harness::new(
        "survive",
        &kv_handler(),
        &manifest(true, false),
        &[BOT_UUID],
    )
    .await;

    let first = run_once(&harness, harness.bot, "第一次").await;
    assert_eq!(first, r#"kv:{"round":1}"#);
    assert_eq!(
        harness.stored_probe(harness.bot).as_deref(),
        Some(r#"{"round":1}"#),
        "第一轮结束后库里就应该有这一行"
    );
    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
    assert_eq!(
        harness.stored_probe(harness.bot).as_deref(),
        Some(r#"{"round":1}"#),
        "停掉 bot 不能把状态一起删掉"
    );

    let second = run_once(&harness, harness.bot, "第二次").await;
    assert_eq!(
        second, r#"kv:{"round":2}"#,
        "重启后应读到上一次写的值并 +1，实际：{second}"
    );

    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

/// 宿主进程重启后值仍在：换掉整个 `AppState`（连 `Store` 一起），同一个 `data_dir` 重开。
///
/// 比上一条更硬：`bot_stop`/`bot_start` 只是换 isolate，`Store` 还是同一份对象；这里连
/// 进程内的 sqlite 句柄都换了，能读回来只可能是真的落盘了。
#[tokio::test]
async fn kv_survives_a_host_restart() {
    let mut harness = Harness::new(
        "hostrestart",
        &kv_handler(),
        &manifest(true, false),
        &[BOT_UUID],
    )
    .await;

    let first = run_once(&harness, harness.bot, "重启前").await;
    assert_eq!(first, r#"kv:{"round":1}"#);

    harness.reopen().await;

    // 换了一份 `Store`，直接读库确认值在磁盘上（不依赖 handler 是否跑得起来）。
    assert_eq!(
        harness.stored_probe(harness.bot).as_deref(),
        Some(r#"{"round":1}"#),
        "重开 AppState 后库里应该还有这一行"
    );

    let second = run_once(&harness, harness.bot, "重启后").await;
    assert_eq!(
        second, r#"kv:{"round":2}"#,
        "宿主重启后应读到上一次写的值并 +1，实际：{second}"
    );

    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑤ 只改清单（不动源码）也让新声明生效
// ---------------------------------------------------------------------------

/// 用户把 `capabilities.state.kv` 从 `false` 改成 `true`，**一个 `.ts` 字节都不动**。
///
/// 这条钉的是「清单参与热重载指纹」（`app/runtime/fingerprint_cache.rs` 的
/// `manifest_change_alone_changes_the_fingerprint` 是它的单元版）：isolate 只在指纹变化时
/// 重建，而状态/设备能力是**重建时**按清单装配的。不把 `hbot.json` 算进指纹，用户改完声明
/// 会看到「旧 handler 照样报 NOT_DECLARED」，必须再动一行源码才生效 —— 那是很难解释的 bug。
///
/// 生产 TTL（元数据 2s / 内容地板 5s）不能改（`with_fingerprint_ttls` 是 `#[cfg(test)]`），
/// 所以这里靠重复投递跨过刷新窗口：拿到 `kv:` 前缀就算生效，超时则把最后一次错误原样报出来。
#[tokio::test]
async fn declaring_kv_in_the_manifest_only_takes_effect_without_touching_sources() {
    let harness = Harness::new(
        "manifestonly",
        &kv_handler(),
        &manifest(false, false),
        &[BOT_UUID],
    )
    .await;

    let before = run_once(&harness, harness.bot, "改之前").await;
    assert!(
        before.starts_with("kv-error:HBOT_STATE_NOT_DECLARED:"),
        "改清单之前必须是声明层拒绝，实际：{before}"
    );

    // 只改清单：声明 kv，`src/main.ts` 原样不动。
    harness.write_manifest(&manifest(true, false));

    let mut last = String::new();
    let mut applied = false;
    for round in 0..80 {
        last = deliver(&harness, harness.bot, &format!("改之后 {round}")).await;
        if last.starts_with("kv:") {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        applied,
        "只改 hbot.json 也必须让新声明生效（隔离只在指纹变化时重建），最后一次回复：{last}"
    );
    // 声明生效后 handler 是全新的：第一次读到的还是 null，所以写进去的是 round=1。
    assert_eq!(
        last, r#"kv:{"round":1}"#,
        "新声明生效后应是干净的第一次写，实际：{last}"
    );

    assert_ok(&harness.stop(harness.bot).await, "bot_stop");
}

// ---------------------------------------------------------------------------
// ⑥ 两个 bot 互不可见
// ---------------------------------------------------------------------------

/// 同一个 `Store` 上两个 bot 用同一个键名：第二个 bot 读到的是 `null`。
#[tokio::test]
async fn two_bots_on_one_store_do_not_share_kv() {
    let harness = Harness::new(
        "isolation",
        &kv_handler(),
        &manifest(true, false),
        &[BOT_UUID, SECOND_BOT_UUID],
    )
    .await;
    let second = bot_id(SECOND_BOT_UUID);

    let first_reply = run_once(&harness, harness.bot, "我是第一个").await;
    assert_eq!(first_reply, r#"kv:{"round":1}"#);
    assert_ok(&harness.stop(harness.bot).await, "bot_stop");

    // 第二个 bot 从零开始读：`before` 是 null，所以写进去的是 round=1 而不是 2。
    let second_reply = run_once(&harness, second, "我是第二个").await;
    assert_eq!(
        second_reply, r#"kv:{"round":1}"#,
        "第二个 bot 不该看到第一个 bot 的键，实际：{second_reply}"
    );

    assert_ok(&harness.stop(second).await, "bot_stop");
}
