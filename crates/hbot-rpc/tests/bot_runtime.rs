//! 机器人运行链路的集成测试：`store` → 会话队列 → QuickJS 沙箱 → `outbound`。
//!
//! 这里**不碰 socket**：`BotAdapter` 换成可观察的 fake 实现，OneBot 连接器留给
//! `hbot-adapters` 自己的单测。被测的是 `bot_start`/`bot_stop`/`bot_inject`/
//! `logs_tail` 四个命令与真实 `SandboxHandler` 的接线：
//!
//! 1. 启动两次 → `-32003`（冲突）；
//! 2. 停止幂等（连停两次都成功）；
//! 3. 一条消息走完 store → 沙箱 → outbox，回复在 `outbound` 里是 `pending`；
//! 4. 生成代码编译失败 → 命令/日志里看得到错误，且 inbound **不**被标记已处理；
//! 5. `wss://` 已被接受（TLS 在连接期由 rustls 完成，配置校验不再拒绝）；
//! 6. 没有 `bots.json` → `-32002` 且不留下"运行中"的僵尸。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hbot_adapters::SystemClock;
use hbot_bot::{AdapterSubscription, BotAdapter, ConversationRef, OutboundEnvelope, PlatformEvent};
use hbot_core::{BotId, BotStatus, ConversationId, ConversationKind, HbotError, ProjectId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::AdapterFactory;
use hbot_rpc::app::runtime::config::BotConfig;
use hbot_rpc::dto::{Command, Event, Response};
use tokio::sync::mpsc;

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_UUID: &str = "11111111-1111-4111-8111-111111111111";
const GROUP_EXTERNAL_ID: &str = "90001";

const HANDLER_REPLY: &str = r#"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.log('收到 ' + ctx.text);
    ctx.reply('pong');
  },
});
"#;

const HANDLER_BROKEN: &str = r#"
// 裸包名在编译期就该被拒绝（HBOT_BARE_IMPORT），沙箱永远不该跑到它。
import lodash from 'lodash';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('never');
  },
});
"#;

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
            std::env::temp_dir().join(format!("hbot-botrt-{label}-{}-{seq}", std::process::id()));
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

fn project_id() -> ProjectId {
    ProjectId::from_uuid(uuid::Uuid::parse_str(PROJECT_UUID).expect("project uuid"))
}

// ---------------------------------------------------------------------------
// fake 适配器：不建 socket，只把订阅句柄交出去并记录发出文本
// ---------------------------------------------------------------------------

struct FakeAdapter {
    /// 订阅句柄只交一次；握手后为 `None`。
    subscriber: Mutex<Option<mpsc::Receiver<PlatformEvent>>>,
    /// 保持事件通道活着，否则 supervisor 的读循环会立刻看到"连接关闭"。
    _keep_alive: mpsc::Sender<PlatformEvent>,
    sent: Arc<Mutex<Vec<String>>>,
    /// D3：每次发送带的被动回复上下文（与 `sent` 同序，只在真正尝试发送时记录）。
    sent_reply_to: Arc<Mutex<Vec<Option<String>>>>,
    /// 每次发送的出站行主键：测试按 id 回查 `outbound` 行，**不依赖 outbox 状态**
    /// （行可能在重试用尽后变成 `failed`，那时 `list_pending_outbound*` 已经查不到它）。
    sent_ids: Arc<Mutex<Vec<hbot_store::OutboundId>>>,
    fail_send: bool,
}

#[async_trait]
impl BotAdapter for FakeAdapter {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn declared_conversations(&self) -> Vec<ConversationRef> {
        vec![ConversationRef::new(
            GROUP_EXTERNAL_ID,
            ConversationKind::Group,
        )]
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        let receiver = self
            .subscriber
            .lock()
            .expect("订阅锁")
            .take()
            .ok_or_else(|| HbotError::conflict("fake adapter 已经订阅过了"))?;
        Ok(AdapterSubscription::new(receiver))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        self.sent.lock().expect("发送记录锁").push(envelope.text);
        self.sent_reply_to
            .lock()
            .expect("回复上下文锁")
            .push(envelope.reply_to);
        self.sent_ids.lock().expect("出站 id 锁").push(envelope.id);
        if self.fail_send {
            // 失败让 outbox 行停在 `pending`，测试才能断言"尚未发出但已入账"。
            return Err(HbotError::internal("fake adapter 故意发送失败"));
        }
        Ok(())
    }
}

struct FakeFactory {
    sent: Arc<Mutex<Vec<String>>>,
    /// D3：适配器收到的 `reply_to`（与 `sent` 同序）。
    sent_reply_to: Arc<Mutex<Vec<Option<String>>>>,
    /// 适配器收到的出站行主键（与 `sent` 同序）。
    sent_ids: Arc<Mutex<Vec<hbot_store::OutboundId>>>,
    /// 把事件塞进"平台 → supervisor"的通道（模拟平台推送一条带平台消息 id 的消息）。
    sender: Mutex<Option<mpsc::Sender<PlatformEvent>>>,
    fail_send: bool,
    created: AtomicU64,
}

impl FakeFactory {
    fn new(fail_send: bool) -> Arc<Self> {
        Arc::new(Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            sent_reply_to: Arc::new(Mutex::new(Vec::new())),
            sent_ids: Arc::new(Mutex::new(Vec::new())),
            sender: Mutex::new(None),
            fail_send,
            created: AtomicU64::new(0),
        })
    }

    fn created(&self) -> u64 {
        self.created.load(Ordering::Relaxed)
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().expect("发送记录锁").clone()
    }

    /// D3：适配器收到的被动回复上下文。
    fn sent_reply_to(&self) -> Vec<Option<String>> {
        self.sent_reply_to.lock().expect("回复上下文锁").clone()
    }

    /// D3：适配器收到的出站行主键。
    fn sent_ids(&self) -> Vec<hbot_store::OutboundId> {
        self.sent_ids.lock().expect("出站 id 锁").clone()
    }

    /// 模拟平台推送一条事件；尚未订阅时返回 `false`。
    fn push(&self, event: PlatformEvent) -> bool {
        match self.sender.lock().expect("事件通道锁").as_ref() {
            Some(sender) => sender.try_send(event).is_ok(),
            None => false,
        }
    }
}

impl AdapterFactory for FakeFactory {
    fn create(
        &self,
        _bot: &hbot_store::Bot,
        _config: &BotConfig,
    ) -> Result<Arc<dyn BotAdapter>, HbotError> {
        self.created.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(8);
        *self.sender.lock().expect("事件通道锁") = Some(tx.clone());
        Ok(Arc::new(FakeAdapter {
            subscriber: Mutex::new(Some(rx)),
            _keep_alive: tx,
            sent: Arc::clone(&self.sent),
            sent_reply_to: Arc::clone(&self.sent_reply_to),
            sent_ids: Arc::clone(&self.sent_ids),
            fail_send: self.fail_send,
        }))
    }
}

// ---------------------------------------------------------------------------
// 测试夹具
// ---------------------------------------------------------------------------

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    factory: Arc<FakeFactory>,
    bot: BotId,
}

impl Harness {
    /// `bots_json` 为 `None` 时不写配置文件（用来测 `-32002`）。
    async fn new(
        label: &str,
        handler_source: &str,
        bots_json: Option<&str>,
        fail_send: bool,
    ) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        if let Some(contents) = bots_json {
            std::fs::write(project.join("bots.json"), contents).expect("写 bots.json");
        }

        let factory = FakeFactory::new(fail_send);
        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState")
        .with_adapter_factory(factory.clone());
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            factory,
            bot: bot_id(),
        }
    }

    fn config_json() -> String {
        format!(r#"{{"bots":[{{"id":"{BOT_UUID}","url":"ws://127.0.0.1:1"}}]}}"#)
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

    /// 群里那条会话的 id：supervisor 启动时会为 `declared_conversations` 建行。
    fn group_conversation(&self) -> ConversationId {
        hbot_bot::conversation_id_for(&self.bot, GROUP_EXTERNAL_ID)
    }

    async fn inject(&self, text: &str) -> Response {
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: self.bot,
                conversation_id: self.group_conversation(),
                text: text.to_owned(),
            })
            .await
    }

    fn pending_outbound(&self) -> Vec<hbot_store::OutboundRow> {
        self.store()
            .list_pending_outbound_for_bot(&self.bot, 32)
            .expect("读 pending outbound")
    }

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store().list_logs(Some(&self.bot), 64).expect("读日志")
    }

    fn unprocessed_inbound(&self) -> usize {
        self.store()
            .list_unprocessed_inbound(32)
            .expect("读未处理 inbound")
            .len()
    }

    async fn shutdown(&self) {
        let _ = self.stop().await;
    }
}

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => panic!("期望成功，实际失败 {}：{}", error.code, error.message),
    }
}

fn error_code(response: &Response) -> Option<i32> {
    match response {
        Response::Ok { .. } => None,
        Response::Error { error } => Some(error.code),
    }
}

/// 轮询等待异步链路的副作用（队列、沙箱、outbox 都在别的任务里推进）。
async fn wait_for(label: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..240 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

/// 从事件流里等一条指定状态的 `bot.status`。
async fn wait_for_status(receiver: &mut tokio::sync::broadcast::Receiver<Event>, want: BotStatus) {
    for _ in 0..240 {
        match receiver.try_recv() {
            Ok(Event::BotStatus { status, .. }) if status == want => return,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("事件流已关闭：{error}"),
        }
    }
    panic!("没有收到 {want:?} 状态事件");
}

// ---------------------------------------------------------------------------
// 1. 重复启动是冲突
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_twice_is_conflict() {
    let harness = Harness::new(
        "start-twice",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    let mut events = harness.state.events().subscribe();

    assert_ok(&harness.start().await);
    wait_for_status(&mut events, BotStatus::Running).await;

    let second = harness.start().await;
    assert_eq!(error_code(&second), Some(-32003), "重复启动必须报冲突");
    assert!(harness.state.bots().is_running(&harness.bot));
    assert_eq!(harness.factory.created(), 1, "冲突时不该再建一个适配器");

    // 运行中不能注入到没有对话行的会话上：仍是明确的 not-found，而不是静默丢弃。
    let unknown = harness
        .dispatcher
        .dispatch(Command::BotInject {
            bot_id: harness.bot,
            conversation_id: ConversationId::from_uuid(
                uuid::Uuid::parse_str("99999999-9999-4999-8999-999999999999").expect("uuid"),
            ),
            text: "ping".to_owned(),
        })
        .await;
    assert_eq!(error_code(&unknown), Some(-32001));

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. 停止幂等
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_is_idempotent() {
    let harness = Harness::new(
        "stop-idempotent",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    let mut events = harness.state.events().subscribe();

    assert_ok(&harness.start().await);
    wait_for_status(&mut events, BotStatus::Running).await;

    assert_ok(&harness.stop().await);
    wait_for_status(&mut events, BotStatus::Stopped).await;
    assert!(!harness.state.bots().is_running(&harness.bot));

    // 第二次停止：仍然成功，且再发一次 stopped 状态。
    assert_ok(&harness.stop().await);
    assert!(!harness.state.bots().is_running(&harness.bot));

    let stored = harness
        .state
        .blocking({
            let store = Arc::clone(harness.store());
            let bot = harness.bot;
            move || {
                store
                    .get_bot_status(&bot)
                    .map_err(hbot_rpc::app::store_failed)
            }
        })
        .await
        .expect("读 bots.status")
        .map(|row| row.status);
    assert_eq!(stored.as_deref(), Some("stopped"));

    // 停掉之后注入必须是冲突（不是悄悄接受）。
    let injected = harness.inject("ping").await;
    assert_eq!(error_code(&injected), Some(-32003));
}

// ---------------------------------------------------------------------------
// 3. 一条消息走完 store → 沙箱 → outbound
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_reaches_outbox() {
    // 发送故意失败：行留在 `pending`，说明"已入账、尚未送达"。
    let harness = Harness::new(
        "message-flow",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        true,
    )
    .await;
    assert_ok(&harness.start().await);

    assert_ok(&harness.inject("ping").await);

    let store = Arc::clone(harness.store());
    let bot = harness.bot;
    wait_for("回复进 outbound", || {
        !store
            .list_pending_outbound_for_bot(&bot, 32)
            .expect("读 pending outbound")
            .is_empty()
    })
    .await;

    let pending = harness.pending_outbound();
    assert_eq!(pending.len(), 1, "一条消息只该产生一条回复");
    assert_eq!(pending[0].text, "pong");
    assert_eq!(pending[0].bot_id, harness.bot);

    // inbound 已被标记处理（RecordingHandler 在 handler 成功后写 processed_at）。
    wait_for("inbound 标记已处理", || {
        harness.unprocessed_inbound() == 0
    })
    .await;

    // handler 里的 ctx.log 也落了日志表。
    wait_for("日志落库", || {
        harness
            .logs()
            .iter()
            .any(|row| row.line.contains("收到 ping"))
    })
    .await;

    // fake 适配器确实被调用过（失败重试也在同一路径上）。
    wait_for("适配器收到发送请求", || {
        !harness.factory.sent().is_empty()
    })
    .await;
    assert_eq!(harness.factory.sent()[0], "pong");

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3b. D3：被动回复上下文（平台消息 id）落库，并在重启续发后仍然带着
// ---------------------------------------------------------------------------

/// 平台推来的被动回复必须有上下文：QQ/OneBot 的被动回复要带上原消息 id，
/// 否则适配器只能发主动消息（有独立且更严的配额）。
///
/// 这条用例走完整链路：平台事件 → `MessageCtx` → `outbound.reply_to` 落库 →
/// 重启时 `StorePendingOutbox` 读回 → `Outbox::recover` → 适配器收到的信封。
///
/// ## 为什么分两段、且第二段显式种一条 pending 行
///
/// 第一段让发送成功，靠适配器记下的出站行主键回查 `outbound` 行——**不依赖 outbox 状态**。
/// 曾经的写法是"发送故意失败 → 等行停在 pending → 重启续发"，那是一个**顺序依赖**：
/// 重试预算默认 3 次、间隔 200ms，整套并行跑时行很可能在 `stop()` 完成之前就用完预算变成
/// `failed`，而 `failed` 行按设计不进续发集合（`failed_outbound_is_excluded_from_pending`），
/// 于是"重启后没东西可续发"→ 超时。那是用例的时序假设错了，不是生产代码的顺序 bug。
/// 所以第二段用 store 的公开 API 显式种一条 `pending` 行：它精确表达"进程在发送前被杀"
/// 这个真实形状，且与调度快慢无关。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passive_reply_context_survives_restart() {
    let harness = Harness::new(
        "reply-to",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    // 平台消息 id 不是自增也不是 UUID：原样带到 reply_to，不做任何加工。
    let event = PlatformEvent::new(
        "msg-9001",
        ConversationRef::new(GROUP_EXTERNAL_ID, ConversationKind::Group),
        "user-7",
        serde_json::json!({ "text": "ping" }),
    );
    wait_for("适配器已订阅并接受事件", || {
        harness.factory.push(event.clone())
    })
    .await;
    wait_for("回复已发出", || !harness.factory.sent_ids().is_empty()).await;

    // 1) 被动回复上下文落库：按出站行主键回查（`get_outbound` 不看 `state`）。
    let reply_id = harness.factory.sent_ids()[0];
    let row = harness
        .store()
        .get_outbound(&reply_id)
        .expect("读 outbound 行")
        .expect("回复必须落库");
    assert_eq!(
        row.reply_to.as_deref(),
        Some("msg-9001"),
        "被动回复上下文必须落进 outbound.reply_to"
    );
    assert_eq!(
        harness.factory.sent_reply_to()[0].as_deref(),
        Some("msg-9001"),
        "适配器必须收到被动回复上下文"
    );

    harness.shutdown().await;

    // 2) 重启续发：种一条 pending 行 = "已入账、还没发出去就被重启"，重启时必须被捞回来。
    let seeded = hbot_store::OutboundId::from_uuid(uuid::Uuid::from_u128(
        0x9e00_0000_0000_0000_0000_0000_0000_0001,
    ));
    harness
        .store()
        .enqueue_outbound_with_target(
            hbot_store::NewOutbound {
                id: seeded,
                bot_id: harness.bot,
                conversation_id: harness.group_conversation(),
                text: "after-restart".to_owned(),
                created_at: 1,
            },
            hbot_store::OutboundTarget::new("{}", Some("msg-9002".to_owned())),
        )
        .expect("种一条 pending 行");

    let delivered_before = harness.factory.sent_ids().len();
    assert_ok(&harness.start().await);
    wait_for("重启续发", || {
        harness.factory.sent_ids().len() > delivered_before
    })
    .await;

    let delivered = harness.factory.sent_ids();
    assert!(
        delivered[delivered_before..].contains(&seeded),
        "续发的必须就是那条 pending 行：{:?}",
        &delivered[delivered_before..]
    );
    assert_eq!(
        harness
            .factory
            .sent_reply_to()
            .last()
            .cloned()
            .flatten()
            .as_deref(),
        Some("msg-9002"),
        "重启续发必须仍然回复到同一条平台消息"
    );
    let recovered_row = harness
        .store()
        .get_outbound(&seeded)
        .expect("读续发行")
        .expect("续发行仍在库里");
    assert_eq!(
        recovered_row.reply_to.as_deref(),
        Some("msg-9002"),
        "续发后行里的被动回复上下文不能被抹掉"
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. 编译错误必须诚实暴露，且不标记 inbound 已处理
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compile_error_is_reported_and_inbound_stays_unprocessed() {
    let harness = Harness::new(
        "compile-error",
        HANDLER_BROKEN,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    assert_ok(&harness.inject("ping").await);

    wait_for("失败日志落库", || {
        harness
            .logs()
            .iter()
            .any(|row| row.level == "error" && row.line.contains("HBOT_BARE_IMPORT"))
    })
    .await;

    // 关键的"不许假装成功"：inbound 仍未被处理，也没有伪造回复。
    assert_eq!(
        harness.unprocessed_inbound(),
        1,
        "编译失败必须留下未处理的 inbound"
    );
    assert!(
        harness.pending_outbound().is_empty(),
        "编译失败不该产生回复"
    );
    assert!(harness.factory.sent().is_empty(), "编译失败不该调用发送");

    // 机器人仍在运行（一次失败只是计入连续失败，不自动停）。
    assert!(harness.state.bots().is_running(&harness.bot));

    // logs_tail 能把这条错误取出来（最新 limit 行、数组内从旧到新）。
    let tail = harness
        .dispatcher
        .dispatch(Command::LogsTail {
            bot_id: harness.bot,
            limit: 10,
        })
        .await;
    let data = match &tail {
        Response::Ok { data } => data.clone().unwrap_or(serde_json::Value::Null),
        Response::Error { error } => panic!("logs_tail 失败 {}：{}", error.code, error.message),
    };
    let rows = data.as_array().expect("logs_tail 返回数组");
    assert!(!rows.is_empty());
    assert!(
        rows.iter().any(|row| row["line"]
            .as_str()
            .unwrap_or_default()
            .contains("HBOT_BARE_IMPORT")),
        "logs_tail 应该包含编译错误：{rows:?}"
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. wss:// 已被接受：TLS 由连接期的 rustls 完成，配置校验不再拒绝
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wss_is_supported_by_the_onebot_adapter() {
    let factory = hbot_rpc::app::runtime::onebot::OneBotAdapterFactory;
    let bot = hbot_store::Bot {
        id: bot_id(),
        project_id: project_id(),
        name: "测试".to_owned(),
        platform: hbot_rpc::app::runtime::onebot::PLATFORM.to_owned(),
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: 0,
    };

    // ① 配置期不再拒绝 `wss://`。旧断言是"必须被拒绝（没有 TLS 后端，
    //    不假装能连）"——适配器补上 rustls 之后那句话本身就是假的，所以反过来钉。
    //    真正的证书校验发生在连接期，由 `tests/tls_sockets.rs` 用真实 TLS 监听器覆盖。
    let adapter = factory
        .create(&bot, &BotConfig::new("wss://127.0.0.1:3001"))
        .expect("wss:// 必须被接受：适配器已支持 TLS");
    drop(adapter);

    // ② 非 ws/wss 的 scheme 仍在配置期被拒（`Blocked` → wire 上 -32005 denied）。
    let error = match factory.create(&bot, &BotConfig::new("http://127.0.0.1:3001")) {
        Err(error) => error,
        Ok(_) => panic!("http:// 必须被拒绝：合法 scheme 只有 ws:// 与 wss://"),
    };
    assert_eq!(error.code(), -32005, "非法 scheme 应映射为 denied");

    // ③ 连接期确实走 TLS，**没有**降级成明文：对一个"只说 TCP、不说 TLS"的回环
    //    监听器做一次探测，客户端发出的第一个字节必须是 TLS ClientHello 记录（0x16），
    //    失败必须是 Tls/Transport —— 不能是 `Unsupported`。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut head = [0_u8; 8];
        let _ = socket.read_exact(&mut head).await;
        // 回一句明文 HTTP：若客户端"降级成明文 WS"，它会拿这个当握手响应；
        // 真走 TLS 的话，rustls 会把它当非法记录并报错。
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await;
        let _ = socket.shutdown().await;
        head
    });

    let error = hbot_adapters::platforms::onebot::probe_connection(
        &hbot_adapters::platforms::onebot::OneBotConfig::new(format!("wss://{address}")),
    )
    .await
    .expect_err("明文服务器不可能完成 TLS 握手");
    assert!(
        matches!(
            error,
            hbot_adapters::AdapterError::Tls { .. } | hbot_adapters::AdapterError::Transport { .. }
        ),
        "TLS 握手指向明文服务器时应报 Tls/Transport，实际 {error:?}"
    );
    assert!(
        !matches!(error, hbot_adapters::AdapterError::Unsupported { .. }),
        "wss:// 不应再报 Unsupported（TLS 后端已存在）：{error:?}"
    );

    let head = server.await.expect("server task");
    assert_eq!(
        head[0], 0x16,
        "wss:// 必须先发 TLS ClientHello 记录（0x16），实际首字节 {:#04x}",
        head[0]
    );

    // ④ ws:// 依然可用（明文路径没有被 TLS 改动影响）。
    assert!(
        factory
            .create(&bot, &BotConfig::new("ws://127.0.0.1:1"))
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// 6. 没有可用配置 → -32002，且不留僵尸
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_bots_json_is_invalid() {
    let harness = Harness::new("missing-config", HANDLER_REPLY, None, false).await;
    let mut events = harness.state.events().subscribe();

    let response = harness.start().await;
    assert_eq!(error_code(&response), Some(-32002), "缺少配置应是 invalid");

    // 失败也要有状态事件（UI 靠它把"启动中"收回去），并且带错误文本。
    let mut failure = None;
    for _ in 0..240 {
        match events.try_recv() {
            Ok(Event::BotStatus {
                status: BotStatus::Stopped,
                error,
                ..
            }) => {
                failure = Some(error);
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("事件流已关闭：{error}"),
        }
    }
    let failure = failure.expect("启动失败必须发 bot.status");
    assert!(
        failure.as_deref().unwrap_or_default().contains("bots.json"),
        "失败事件要带原因：{failure:?}"
    );

    let message = match &response {
        Response::Error { error } => error.message.clone(),
        Response::Ok { .. } => String::new(),
    };
    assert!(
        message.contains("bots.json"),
        "错误信息要点名配置文件：{message}"
    );

    // 失败后预留必须释放，否则这个 bot 再也起不来。
    assert!(!harness.state.bots().is_running(&harness.bot));
    assert!(!harness.state.bots().is_reserved(&harness.bot));

    // 补上配置后可以正常启动（说明失败路径真的回滚干净了）。
    let root = harness.state.project_root().expect("项目根");
    std::fs::write(root.join("bots.json"), Harness::config_json()).expect("补写 bots.json");
    assert_ok(&harness.start().await);

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. logs_tail：取最新 limit 行，数组内从旧到新
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_tail_is_newest_limit_in_chronological_order() {
    // `logs.bot_id` 有外键，所以先真的把 bot 启起来（顺带证明启动会落 bots 行）。
    let harness = Harness::new(
        "logs-tail",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    let store = Arc::clone(harness.store());
    let bot = harness.bot;
    harness
        .state
        .blocking(move || {
            for (index, line) in ["first", "second", "third"].iter().enumerate() {
                store
                    .append_log(hbot_store::NewLog {
                        bot_id: bot,
                        level: "info".to_owned(),
                        line: (*line).to_owned(),
                        at: 1_700_000_000_000 + index as i64,
                    })
                    .map_err(hbot_rpc::app::store_failed)?;
            }
            Ok(())
        })
        .await
        .expect("写日志");

    let response = harness
        .dispatcher
        .dispatch(Command::LogsTail {
            bot_id: bot,
            limit: 2,
        })
        .await;
    let data = match &response {
        Response::Ok { data } => data.clone().unwrap_or(serde_json::Value::Null),
        Response::Error { error } => panic!("logs_tail 失败 {}：{}", error.code, error.message),
    };
    let lines: Vec<String> = data
        .as_array()
        .expect("数组")
        .iter()
        .map(|row| row["line"].as_str().unwrap_or_default().to_owned())
        .collect();
    // 最新两行是 second/third；数组内必须是旧→新（不是 SQL 的倒序）。
    assert_eq!(lines, vec!["second", "third"]);

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 8. 契约：`bot_inject` 的合成消息没有"被动回复"这回事
// ---------------------------------------------------------------------------

/// `bot_inject` 造出来的消息**不是**平台推来的，所以它没有可引用的平台消息 id。
///
/// 契约（`crates/hbot-code/src/dts.rs` 里 `reply` 的说明、`sandbox_handler` 的
/// `passive_reply_id`）：`reply_to` 只有在"被动回复"时才存在，值是被回复的那条平台
/// 消息 id。合成消息若把 `inject-<uuid>` 当成平台消息 id 填进去，OneBot 服务端会因为
/// 引用了一条不存在的消息而拒发（或者发出一条引用了空气的回复），而且这类错误在
/// 生产里极难复现（只有控制台点"注入"才走这条路径）。
///
/// 这里断言的是**整条链路**：`bot_inject` → 沙箱 → `outbound` 落库 → 适配器信封。
/// 反面（真实事件必须带 reply_to）由同一文件里的
/// `passive_reply_context_survives_restart` 与 `sandbox_handler` 的单测覆盖；本条用
/// "先注入、再推一条真事件"的方式把两者放在同一个进程里对照，避免"两边都是 None 却
/// 因为记录根本没写而蒙对"。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_messages_carry_no_passive_reply_context() {
    let harness = Harness::new(
        "inject-no-reply-to",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    // ① 合成消息（`bot_inject`）。
    assert_ok(&harness.inject("ping").await);
    wait_for("注入的回复已发出", || {
        !harness.factory.sent_ids().is_empty()
    })
    .await;

    let injected_id = harness.factory.sent_ids()[0];
    assert_eq!(
        harness.factory.sent_reply_to()[0],
        None,
        "合成消息不许带被动回复上下文：inject 的 external_id 不是平台消息 id"
    );
    let injected_row = harness
        .store()
        .get_outbound(&injected_id)
        .expect("读 outbound 行")
        .expect("注入的回复必须落库");
    assert_eq!(
        injected_row.reply_to, None,
        "`outbound.reply_to` 列也不许被写成合成 id"
    );

    // ② 对照：真实平台事件必须带 reply_to，且就是平台消息 id。
    let event = PlatformEvent::new(
        "msg-7001",
        ConversationRef::new(GROUP_EXTERNAL_ID, ConversationKind::Group),
        "user-7",
        serde_json::json!({ "text": "ping" }),
    );
    let before = harness.factory.sent_ids().len();
    wait_for("适配器接受真实事件", || {
        harness.factory.push(event.clone())
    })
    .await;
    wait_for("真实事件的回复已发出", || {
        harness.factory.sent_ids().len() > before
    })
    .await;

    let real_id = harness.factory.sent_ids()[before];
    assert_eq!(
        harness.factory.sent_reply_to()[before].as_deref(),
        Some("msg-7001"),
        "真实事件必须带被动回复上下文（证明上面的 None 是刻意的，不是记录没写）"
    );
    let real_row = harness
        .store()
        .get_outbound(&real_id)
        .expect("读 outbound 行")
        .expect("真实事件的回复必须落库");
    assert_eq!(real_row.reply_to.as_deref(), Some("msg-7001"));

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9. 契约：兜底幂等键不许当成 reply_to（出站载荷里连 reply 段都不该有）
// ---------------------------------------------------------------------------

/// 出站载荷里所有 `reply` 段的 id（没有就是空）。
///
/// 走的是真适配器的 `outbound_json`，不是自己拼 JSON：这样断言的是**平台真正收到的东西**
/// （`{"type":"reply","data":{"id":"…"}}` 是 OneBot 11 的引用语义）。
fn reply_segment_ids(payload: &serde_json::Value) -> Vec<String> {
    let Some(segments) = payload.as_array() else {
        // 纯文本是字符串形式，本来就不含任何段。
        return Vec::new();
    };
    segments
        .iter()
        .filter(|segment| segment.get("type").and_then(|kind| kind.as_str()) == Some("reply"))
        .filter_map(|segment| {
            segment
                .get("data")
                .and_then(|data| data.get("id"))
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect()
}

/// 出站载荷的纯文本正文（字符串形式直接取；段数组里拼 `text` 段）。
fn outbound_text(payload: &serde_json::Value) -> String {
    if let Some(text) = payload.as_str() {
        return text.to_owned();
    }
    payload
        .as_array()
        .map(|segments| {
            segments
                .iter()
                .filter(|segment| {
                    segment.get("type").and_then(|kind| kind.as_str()) == Some("text")
                })
                .filter_map(|segment| {
                    segment
                        .get("data")
                        .and_then(|data| data.get("text"))
                        .and_then(|text| text.as_str())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// **平台没给 `message_id`** 时，`external_id` 是适配器的兜底幂等键
/// （`{user_id}-{time}-{fnv1a64:016x}`）。它不是平台消息 id，**不许**当 `reply_to`：
/// 真网关收到 `{"type":"reply","data":{"id":"111-…"}}` 会判参数非法、整条发不出去，
/// 用户看到的是"机器人不回我了"，而不是"少了个引用"。
///
/// 这条同时钉住反向：真有 `message_id` 时引用段**必须仍在**（别为了修 bug 把被动回复
/// 整个关掉）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fallback_dedupe_key_never_becomes_a_reply_segment() {
    let harness = Harness::new(
        "fallback-reply-to",
        HANDLER_REPLY,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    // ① 无 message_id 的入站：external_id 是兜底键。
    //    形状与 `onebot::normalize::fallback_external_id` 一致（111 = user_id）。
    let fallback_id = "111-1794790031-06032ba7feff3e72";
    let fallback_event = PlatformEvent::new(
        fallback_id,
        ConversationRef::new(GROUP_EXTERNAL_ID, ConversationKind::Group),
        "user-7",
        serde_json::json!({ "text": "没有 message_id" }),
    );
    wait_for("适配器接受无 message_id 的事件", || {
        harness.factory.push(fallback_event.clone())
    })
    .await;
    wait_for("无 message_id 的回复发出", || {
        !harness.factory.sent_ids().is_empty()
    })
    .await;

    let fallback_sent = harness.factory.sent_ids()[0];
    let fallback_row = harness
        .store()
        .get_outbound(&fallback_sent)
        .expect("读 outbox 行")
        .expect("outbox 行存在");
    assert_eq!(
        fallback_row.reply_to, None,
        "兜底幂等键被写进了 outbound.reply_to：它会变成非法 reply 段"
    );

    let fallback_payload = hbot_adapters::platforms::onebot::outbound_json(
        &fallback_row.text,
        fallback_row.reply_to.as_deref(),
    );
    assert!(
        reply_segment_ids(&fallback_payload).is_empty(),
        "无 message_id 的回复不许带 reply 段，实际载荷：{fallback_payload}"
    );
    assert_eq!(
        outbound_text(&fallback_payload),
        "pong",
        "只该丢掉引用段，正文必须照发（别把回复整条吞了）"
    );

    // ② 有 message_id 的入站：引用段必须在，且 id 就是平台消息 id。
    let real_event = PlatformEvent::new(
        "msg-7001",
        ConversationRef::new(GROUP_EXTERNAL_ID, ConversationKind::Group),
        "user-7",
        serde_json::json!({ "text": "ping", "message_id": 7001 }),
    );
    wait_for("适配器接受真实事件", || {
        harness.factory.push(real_event.clone())
    })
    .await;
    wait_for("真实事件的回复发出", || {
        harness.factory.sent_ids().len() >= 2
    })
    .await;

    let real_sent = harness.factory.sent_ids()[1];
    let real_row = harness
        .store()
        .get_outbound(&real_sent)
        .expect("读 outbox 行")
        .expect("outbox 行存在");
    assert_eq!(
        real_row.reply_to.as_deref(),
        Some("msg-7001"),
        "真有平台消息 id 时必须保留被动回复"
    );
    let real_payload = hbot_adapters::platforms::onebot::outbound_json(
        &real_row.text,
        real_row.reply_to.as_deref(),
    );
    assert_eq!(
        reply_segment_ids(&real_payload),
        vec!["msg-7001".to_owned()],
        "有 message_id 时的 reply 段被修没了：{real_payload}"
    );
    assert_eq!(
        outbound_text(&real_payload),
        "pong",
        "引用段的正文也必须是原回复"
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 10. 契约：幂等键与平台消息 id 是两个命名空间
// ---------------------------------------------------------------------------

/// handler 在失败前先把自己的两个 id 打出来，再抛错——这样 inbound 行**留在未处理**，
/// 测试才能拿到行主键和它逐字比对（处理成功的行会被 `processed_at` 标走）。
const HANDLER_IDS_THEN_THROW: &str = r#"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.log('ids mid=' + ctx.messageId + ' ext=' + ctx.externalId);
    throw new Error('故意失败：让 inbound 停在未处理，便于比对行主键');
  },
});
"#;

/// 跨 crate 的幂等契约：`ctx.messageId` 是**落库那一层的行主键**（`inbound.id`），
/// `ctx.externalId` 才是平台消息 id，两者必须各自逐字对上原始数据。
///
/// ## 为什么不是"messageId == externalId"
///
/// 任务书写的是"`ctx.messageId` 必须与 `inbound.external_id` 逐字一致"，但那是把两个
/// 命名空间混成一个了，而且与本仓库**已发布**的契约冲突：
///
/// * `crates/hbot-code/src/dts.rs` 的 HOST_DTS 注释：「messageId: 消息 id，**幂等键**。
///   注意语义是 at-least-once……」，同处「externalId: 平台侧消息 id」；
/// * `crates/hbot-bot/src/dedupe.rs` 模块文档：「幂等键是 `inbound.id`（生成代码里的
///   `ctx.messageId`）。重复投递之所以安全，是因为唯一约束在**落库这一层**」；
/// * `MessageCtx::message_id()` 的实现就是 `self.message.id`。
///
/// 如果真按字面把它改成 external_id，`messageId` 就不再是"重放稳定"的键：同一个
/// external_id 在两次投递里是同一个值，而 inbound 行主键是首次落库时就定下的——把它换成
/// 平台 id 相当于让幂等键跟着平台漂移，`UNIQUE(bot_id, external_id)` 那层保证就断了。
///
/// 所以本条钉住三条**真实的不变量**：
///
/// 1. `ctx.messageId` == `inbound.id`（行主键，重放稳定）；
/// 2. `ctx.externalId` == `inbound.external_id`（平台消息 id，逐字一致）；
/// 3. 同一个 `external_id` 再投一次，handler **不会**执行第二次（"重复投递只执行一次"）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_id_is_the_stored_inbound_row_key_and_matches_on_replay() {
    let harness = Harness::new(
        "idempotency-key",
        HANDLER_IDS_THEN_THROW,
        Some(&Harness::config_json()),
        false,
    )
    .await;
    assert_ok(&harness.start().await);

    let event = PlatformEvent::new(
        "msg-4242",
        ConversationRef::new(GROUP_EXTERNAL_ID, ConversationKind::Group),
        "user-7",
        serde_json::json!({ "text": "ping" }),
    );
    wait_for("适配器接受事件", || {
        harness.factory.push(event.clone())
    })
    .await;
    wait_for("handler 打出了两个 id", || {
        harness
            .logs()
            .iter()
            .any(|row| row.line.contains("ids mid="))
    })
    .await;

    // 行主键：handler 故意失败，所以这条 inbound 还在"未处理"集合里。
    let pending = harness
        .store()
        .list_unprocessed_inbound(32)
        .expect("读未处理 inbound");
    assert_eq!(pending.len(), 1, "应该正好一条未处理入站");
    let row = &pending[0];

    let line = harness
        .logs()
        .into_iter()
        .find(|row| row.line.contains("ids mid="))
        .expect("handler 的那行日志")
        .line;
    let mid = line
        .split("mid=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .expect("日志里有 mid=");
    let ext = line
        .split("ext=")
        .nth(1)
        .expect("日志里有 ext=")
        .trim()
        .to_owned();

    // ① messageId 是落库的行主键，逐字一致（重放时也是这个值）。
    assert_eq!(
        mid,
        row.id.to_string(),
        "ctx.messageId 必须是 inbound.id（幂等键），实际 {mid} vs 行主键 {}",
        row.id
    );
    // ② externalId 是平台消息 id，逐字一致。
    assert_eq!(ext, "msg-4242", "ctx.externalId 必须是平台消息 id");
    assert_eq!(
        row.external_id, "msg-4242",
        "落库的 external_id 必须与平台事件逐字一致"
    );
    // ③ 两个命名空间不同：谁把 messageId 改成 external_id，这条立刻红。
    assert_ne!(
        mid, row.external_id,
        "messageId 与 external_id 是两个命名空间，不能混用"
    );

    // ④ 同一 external_id 再投一次：handler 不许执行第二次。
    let runs_before = harness
        .logs()
        .iter()
        .filter(|row| row.line.contains("ids mid="))
        .count();
    assert!(harness.factory.push(event));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let runs_after = harness
        .logs()
        .iter()
        .filter(|row| row.line.contains("ids mid="))
        .count();
    assert_eq!(
        runs_after, runs_before,
        "同一个 external_id 重复投递必须只执行一次（幂等键落在落库层）"
    );
    assert_eq!(
        harness.unprocessed_inbound(),
        1,
        "重复投递不该再多一条 inbound 行"
    );

    harness.shutdown().await;
}
