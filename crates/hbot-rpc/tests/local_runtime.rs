//! `platform = "local"`（本地试跑）的集成测试：**不联网**跑通"写代码 → 启动 → 注入 → 看回复"。
//!
//! 这个文件钉的是产品闭环里缺的那一环：在没有真实平台（NapCat / QQ 开放平台）的机器上，
//! 一个刚生成出来的 bot 也要能 `bot_start` 成功、`bot_inject` 一条消息、把 `reply()`
//! 的结果写进 run log —— 否则 IDE 里"试跑"这个按钮只能是灰的。
//!
//! 这里**没有** fake 适配器、也**没有** mock：走的是生产路径
//! （[`hbot_rpc::app::runtime::PlatformRouter`] → [`LocalAdapter`](hbot_rpc::app::runtime::local)
//! → 真 `SandboxHandler`（QuickJS 编译 + 执行 `src/main.ts`）→ outbox → 适配器 `send`）。
//! 唯一"假"的东西是项目目录：每个用例在自己的临时目录里造一个真项目。
//!
//! 六类断言（对应任务书的 D1–D6）：
//!
//! 1. `local` 能启动：`bots.json` 只有 `id` + `platform`，没有 `url` / 凭据；
//! 2. 注入一条 → 生成的代码真的执行了 → run log 里出现 `[reply] ` 前缀的回复；
//! 3. 注入两条不同文本 → `inbound.external_id` 不冲突（各自独立）；
//! 4. `bot_stop` 干净返回（不挂在 `STOP_GRACE` 上）且幂等；
//! 5. `bots.json` 校验：`local` 缺 `url` 不报错、`onebot11` 缺 `url` 仍 -32002、
//!    未知平台 -32002 且消息里列出 `local`；
//! 6. 端到端：`hbot_code::scaffold` 造一个**真的**项目 → 打开 → 启动 → 注入 → 断言回复。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// 本地试跑用的固定外部会话 ID（与 `hbot-adapters` 的常量必须一字不差）。
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
        let path =
            std::env::temp_dir().join(format!("hbot-local-{label}-{}-{seq}", std::process::id()));
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

// ---------------------------------------------------------------------------
// 夹具：一个真项目 + 真 AppState（默认 PlatformRouter，不注入假工厂）
// ---------------------------------------------------------------------------

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
}

impl Harness {
    /// 造项目目录 + 写 `src/main.ts` + 写 `bots.json`，然后打开项目。
    ///
    /// `bots_json` 传 `None` 时不写配置文件（用来测"缺配置"的那条）。
    async fn new(label: &str, handler_source: &str, bots_json: Option<&str>) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        if let Some(contents) = bots_json {
            std::fs::write(project.join("bots.json"), contents).expect("写 bots.json");
        }
        Self::from_project(dir, &project).await
    }

    /// 从一个**已存在**的项目目录起步（端到端用例先 `scaffold` 再调它）。
    async fn from_project(dir: TempDir, project: &Path) -> Self {
        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState");
        state.open_project(project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot: bot_id(),
        }
    }

    /// `local` 平台的最小配置：只有 `id` 与 `platform`。
    fn local_config() -> String {
        format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"本地试跑"}}]}}"#)
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

    async fn inject(&self, conversation_id: ConversationId, text: &str) -> Response {
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: self.bot,
                conversation_id,
                text: text.to_owned(),
            })
            .await
    }

    /// 启动后由 `bot_start` 幂等建好的那一行会话（`local:default`）。
    fn local_conversation(&self) -> hbot_store::Conversation {
        self.store()
            .get_conversation_by_external(&self.bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .unwrap_or_else(|| {
                panic!("bot_start 之后应存在 external_id = {LOCAL_EXTERNAL_ID} 的会话行")
            })
    }

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), 256)
            .expect("读日志")
    }

    /// run log 里所有以 `[reply] ` 开头的行（去掉前缀后的文本）。
    fn replies(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    fn status(&self) -> Option<String> {
        self.store()
            .get_bot_status(&self.bot)
            .expect("读 bot 状态")
            .map(|row| row.status)
    }

    fn inbound(&self) -> Vec<hbot_store::InboundReplayRow> {
        self.store()
            .list_inbound_history_for_bot(&self.bot, None, 64)
            .expect("读 inbound 历史")
    }
}

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

fn error_of(response: &Response) -> (i32, String) {
    match response {
        Response::Ok { .. } => panic!("期望失败，实际成功"),
        Response::Error { error } => (error.code, error.message.clone()),
    }
}

/// 轮询等待异步链路的副作用（队列、沙箱、outbox 都在别的任务里推进）。
///
/// 240 × 25ms = 6s：够 QuickJS 冷编译一次 handler（真机上约 100–300ms），
/// 又不至于让"卡住了"的用例磨到最后 30 秒才报错。
async fn wait_for(label: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..240 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 生成代码里最朴素的 handler：把收到的文本回一遍（`reply` 是 `hbot` 虚拟模块的导出）。
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

// ---------------------------------------------------------------------------
// D1：local 能启动（不需要 url，不联网）
// ---------------------------------------------------------------------------

/// `platform = "local"` 且 `bots.json` 里**没有 url**：`bot_start` 必须成功。
///
/// 这就是整件事的意义：在这条命令之前，同一个 `bots.json` 只可能拿到 -32002。
#[tokio::test]
async fn local_bot_starts_without_any_url() {
    let harness = Harness::new("start", echo_handler(), Some(&Harness::local_config())).await;

    assert_ok(&harness.start().await);

    assert!(
        harness.state.bots().is_running(&harness.bot),
        "启动成功后 registry 里应该有一个运行中的 runtime"
    );
    assert_eq!(
        harness.status().as_deref(),
        Some("running"),
        "库里 bots.status 应该是 running"
    );
    // 没有"连不上平台"的失败痕迹：本地试跑不该产生任何网络错误。
    let logs = harness.logs();
    assert!(
        !logs.iter().any(|row| row.line.contains("connect")
            || row.line.contains("Connection refused")
            || row.line.contains("TLS")),
        "local 平台不应该有任何连接/证书类日志：{:?}",
        logs.iter().map(|row| &row.line).collect::<Vec<_>>()
    );

    // 收尾：不留下还在跑的 runtime（否则后面 drop 时要等 STOP_GRACE）。
    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// C 项：bot_start 幂等建好 local:default 会话行
// ---------------------------------------------------------------------------

/// `local` 不订阅真实事件，所以没人会建 conversation 行 —— `bot_start` 必须自己建。
///
/// 没有这一行，`bot_inject` 会 -32001（`inject_inner` 先 `get_conversation`）。
#[tokio::test]
async fn start_creates_the_local_default_conversation_idempotently() {
    let harness = Harness::new(
        "conversation",
        echo_handler(),
        Some(&Harness::local_config()),
    )
    .await;

    assert_ok(&harness.start().await);
    let first = harness.local_conversation();
    assert_eq!(first.external_id, LOCAL_EXTERNAL_ID);
    assert_eq!(first.kind, "private", "试跑会话按私聊处理");
    assert_eq!(first.bot_id, harness.bot);

    // 会话 id 由 (bot_id, external_id) 确定性派生：重复启动不能换 id，也不能多出一行。
    assert_eq!(
        first.id,
        hbot_bot::conversation_id_for(&harness.bot, LOCAL_EXTERNAL_ID)
    );

    assert_ok(&harness.stop().await);
    assert_ok(&harness.start().await);
    let second = harness.local_conversation();
    assert_eq!(first.id, second.id, "重复 bot_start 不能换会话 id");

    // 日志里要有一行把 conversationId 交出来：wire DTO 目前没有这条通道，
    // `logs_tail` 是眼下唯一能让人/脚本拿到它的地方。
    let announced = harness
        .logs()
        .into_iter()
        .filter(|row| row.line.contains(&first.id.to_string()))
        .count();
    assert!(
        announced >= 1,
        "bot_start 应落一行带 conversationId 的日志（UI 侧要靠它试跑）"
    );

    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// D2：注入 → 生成的代码被执行 → 回复进 run log
// ---------------------------------------------------------------------------

/// 最关键的一条：注入一条消息，`src/main.ts` 里的 `reply()` 真的执行了，
/// 回复以 `[reply] ` 前缀出现在 run log 里（IDE 的日志面板读的就是这张表）。
#[tokio::test]
async fn inject_executes_the_generated_code_and_writes_the_reply_to_the_run_log() {
    let harness = Harness::new("inject", echo_handler(), Some(&Harness::local_config())).await;
    assert_ok(&harness.start().await);
    let conversation = harness.local_conversation();

    assert_ok(&harness.inject(conversation.id, "你好").await);

    wait_for("回复出现在 run log 里", || {
        harness.replies().iter().any(|text| text == "收到：你好")
    })
    .await;

    let replies = harness.replies();
    assert_eq!(
        replies,
        vec!["收到：你好".to_owned()],
        "一条消息只该产出恰好一条回复"
    );

    // 前缀必须是行首（UI 按前缀高亮/折叠），而不是行中某处。
    let row = harness
        .logs()
        .into_iter()
        .find(|row| row.line.contains("收到：你好"))
        .expect("回复行");
    assert!(
        row.line.starts_with(REPLY_PREFIX),
        "回复行必须以 `{REPLY_PREFIX}` 开头，实际是 {:?}",
        row.line
    );
    assert_eq!(row.bot_id, Some(harness.bot));
    assert_eq!(row.source, "bot", "回复写的是 bot 自己的 run log");

    // inbound 被标记已处理（没有"卡在队列里"的半条消息）。
    let inbound = harness.inbound();
    assert_eq!(inbound.len(), 1, "应该只有一条 inbound：{inbound:?}");
    // 注入来源看 payload（`inject_inner` 写的是 `senderId` + `injected`），不看
    // `inbound.sender_id` 列：那一列由 `Store::insert_inbound_if_new` 写，走 Inbox 的
    // 统一路径时是空串（平台侧发送者 ID 由 `insert_inbound_with_sender` 才带上）。
    assert!(
        inbound[0]
            .message
            .payload_json
            .contains("\"injected\":true"),
        "注入的消息应在 payload 里带 injected 标记：{}",
        inbound[0].message.payload_json
    );
    assert!(
        inbound[0]
            .message
            .payload_json
            .contains("\"senderId\":\"inject\""),
        "注入的发送者应在 payload 里是 inject：{}",
        inbound[0].message.payload_json
    );
    assert!(
        inbound[0].message.external_id.starts_with("inject-"),
        "注入消息的 external_id 应以 `inject-` 开头，实际 {:?}",
        inbound[0].message.external_id
    );
    // 处理完的 inbound 必须被标记（否则重启会重放这条消息）。用"未处理队列为空"
    // 来断言：`InboundRow` 不暴露 `processed_at`，而 `list_unprocessed_inbound` 是
    // 重启补偿真正读的那个视图。
    let unprocessed = harness
        .store()
        .list_unprocessed_inbound(64)
        .expect("读未处理 inbound");
    assert!(
        unprocessed.is_empty(),
        "处理完的消息不该还留在未处理队列里：{unprocessed:?}"
    );

    assert_ok(&harness.stop().await);
}

/// 多行回复：**每一行**都要带前缀（否则日志面板里第二行看起来像是别的来源）。
#[tokio::test]
async fn multi_line_reply_prefixes_every_line() {
    let handler = r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage() {
    reply('第一行\n第二行');
  },
});
"#;
    let harness = Harness::new("multiline", handler, Some(&Harness::local_config())).await;
    assert_ok(&harness.start().await);
    let conversation = harness.local_conversation();

    assert_ok(&harness.inject(conversation.id, "随便").await);

    wait_for("两行回复都出现在 run log 里", || {
        let replies = harness.replies();
        replies.iter().any(|text| text == "第一行") && replies.iter().any(|text| text == "第二行")
    })
    .await;

    let logs = harness.logs();
    let prefixed = logs
        .iter()
        .filter(|row| row.line.starts_with(REPLY_PREFIX))
        .count();
    assert!(
        prefixed >= 2,
        "多行回复应至少有两行带前缀，实际 {prefixed} 行：{:?}",
        logs.iter().map(|row| &row.line).collect::<Vec<_>>()
    );

    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// D3：两条注入的 external_id 不冲突
// ---------------------------------------------------------------------------

/// `inject_inner` 用 `inject-{message_id}` 当外部 ID：两条注入必须各自独立落库
/// （否则第二条会被去重丢弃，用户看到的是"第二条消息没反应"）。
#[tokio::test]
async fn two_injections_get_distinct_external_ids() {
    let harness = Harness::new("twice", echo_handler(), Some(&Harness::local_config())).await;
    assert_ok(&harness.start().await);
    let conversation = harness.local_conversation();

    assert_ok(&harness.inject(conversation.id, "第一条").await);
    assert_ok(&harness.inject(conversation.id, "第二条").await);

    wait_for("两条回复都出现", || {
        let replies = harness.replies();
        replies.iter().any(|text| text == "收到：第一条")
            && replies.iter().any(|text| text == "收到：第二条")
    })
    .await;

    let inbound = harness.inbound();
    assert_eq!(inbound.len(), 2, "两条注入应落两行 inbound：{inbound:?}");
    let first = &inbound[0].message.external_id;
    let second = &inbound[1].message.external_id;
    assert_ne!(first, second, "两条注入的 external_id 不能相同");
    for external_id in [first, second] {
        assert!(
            external_id.starts_with("inject-"),
            "注入消息的 external_id 应以 `inject-` 开头（一眼能认出来源），实际 {external_id:?}"
        );
    }
    // 两条都落在同一个会话行上（local:default），不是各建一个新会话。
    for row in &inbound {
        assert_eq!(row.message.conversation_id, conversation.id);
        assert_eq!(row.conversation_external_id, LOCAL_EXTERNAL_ID);
    }

    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// D4：bot_stop 干净
// ---------------------------------------------------------------------------

/// `local` 的读循环永远等不到事件，停止时不能被它挂住。
///
/// supervisor 的 `STOP_GRACE` 是 5s：这里断言 stop 远快于它，且幂等。
#[tokio::test]
async fn stop_is_clean_and_idempotent() {
    let harness = Harness::new("stop", echo_handler(), Some(&Harness::local_config())).await;
    assert_ok(&harness.start().await);
    let conversation = harness.local_conversation();
    // 先让一条消息走完，确保停止时队列/沙箱都是"用过又空"的状态。
    assert_ok(&harness.inject(conversation.id, "停之前").await);
    wait_for("注入的回复出现", || {
        harness.replies().iter().any(|text| text == "收到：停之前")
    })
    .await;

    let started = Instant::now();
    assert_ok(&harness.stop().await);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "bot_stop 不该挂在 STOP_GRACE(5s) 上，实际用了 {elapsed:?}"
    );
    assert!(
        !harness.state.bots().is_running(&harness.bot),
        "停止后 registry 里不该还有 runtime"
    );
    assert_eq!(harness.status().as_deref(), Some("stopped"));

    // 幂等：第二次停止仍然成功（UI 的"停止"按钮可能被连点）。
    assert_ok(&harness.stop().await);

    // 停止后再启动一次也能起来（试跑是迭代式的，不是一次性的）。
    assert_ok(&harness.start().await);
    assert!(harness.state.bots().is_running(&harness.bot));
    assert_ok(&harness.stop().await);
}

// ---------------------------------------------------------------------------
// D5：bots.json 校验（三条对照）
// ---------------------------------------------------------------------------

/// 未知平台：仍然 -32002，且消息里必须列出 `local`（用户得能发现这条路）。
#[tokio::test]
async fn unknown_platform_is_rejected_and_lists_local() {
    let bots_json = format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"telegram"}}]}}"#);
    let harness = Harness::new("unknown", echo_handler(), Some(&bots_json)).await;

    let (code, message) = error_of(&harness.start().await);
    assert_eq!(code, -32002, "{message}");
    assert!(message.contains("local"), "错误消息应列出 local：{message}");
    assert!(message.contains("onebot11"), "{message}");
    assert!(message.contains("qq_official"), "{message}");
    // 启动失败不许留下"运行中"的假象。
    assert!(!harness.state.bots().is_running(&harness.bot));
}

/// `onebot11` 缺 `url`：仍然 -32002（加了 local 不许把 OneBot 的必填项放松）。
#[tokio::test]
async fn onebot_entry_without_url_is_still_rejected() {
    let bots_json = format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"onebot11"}}]}}"#);
    let harness = Harness::new("onebot", echo_handler(), Some(&bots_json)).await;

    let (code, message) = error_of(&harness.start().await);
    assert_eq!(code, -32002, "{message}");
    assert!(message.contains("url"), "{message}");
    assert!(!harness.state.bots().is_running(&harness.bot));
}

/// 完全没有 `bots.json`：仍然 -32002（local 不是"缺配置也能跑"的口子：
/// 试跑也要知道启动的是哪个 bot）。
#[tokio::test]
async fn missing_bots_json_is_still_rejected() {
    let harness = Harness::new("no-config", echo_handler(), None).await;

    let (code, message) = error_of(&harness.start().await);
    assert_eq!(code, -32002, "{message}");
    assert!(message.contains("bots.json"), "{message}");
}

// ---------------------------------------------------------------------------
// D6：端到端（scaffold 一个真项目 → 打开 → 启动 → 注入 → 断言回复）
// ---------------------------------------------------------------------------

/// 产品闭环的那一条：`hbot_code::scaffold` 生成的项目，不改一行代码，
/// 只用 `platform = "local"` 就能跑起来。
///
/// scaffold 的 `src/main.ts` 是 `reply(\`收到：${ctx.text}\`)`，所以回复文本可预期。
#[tokio::test]
async fn scaffolded_project_runs_end_to_end_without_network() {
    let dir = TempDir::new("e2e");
    let project = dir.path().join("demo-bot");
    hbot_code::manifest::scaffold(&project, "demo-bot").expect("scaffold 一个真项目");
    // scaffold 不写 bots.json（它是部署配置，不是项目源码）：本地试跑要自己补一份。
    // 注意这里**只**给 id + platform：没有 url、没有 token、没有任何凭据。
    std::fs::write(
        project.join("bots.json"),
        format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local"}}]}}"#),
    )
    .expect("写 bots.json");

    let harness = Harness::from_project(dir, &project).await;
    assert_ok(&harness.start().await);
    let conversation = harness.local_conversation();

    assert_ok(&harness.inject(conversation.id, "本地试跑").await);

    wait_for("scaffold 项目的回复出现在 run log 里", || {
        harness
            .replies()
            .iter()
            .any(|text| text == "收到：本地试跑")
    })
    .await;

    // 端到端断言：回复确实以日志行的形式落库（IDE 日志面板读的同一张表）。
    let replies = harness.replies();
    assert_eq!(replies, vec!["收到：本地试跑".to_owned()]);

    assert_ok(&harness.stop().await);
}
