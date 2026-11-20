//! `crates/hbot-parity` 的公共脚手架：临时目录、假平台适配器、录制执行器、回复搬运。
//!
//! 原则（`docs/04-implementation-plan.md` §4.1）：**只把「外部世界」换成假货**
//! ——平台适配器（假）。SQLite、supervisor、每会话队列、outbox、QuickJS 沙箱
//! 一律是真的，禁止 mock 掉被验证的那一层。
//!
//! 为什么每个用例一个文件而不是 `src/lib.rs`：见 `crates/hbot-parity/Cargo.toml`。
#![allow(dead_code)]

pub mod gateway;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_bot::{
    AdapterSubscription, BotAdapter, BotSpec, ConversationRef, HandlerExecutor, MessageCtx,
    OutboundEnvelope, OutboundRequest, PlatformEvent, Supervisor, SupervisorDeps,
};
use hbot_core::{BotId, ConversationKind, HbotError, ProjectId, SystemClock};
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::AdapterFactory;
use hbot_rpc::app::runtime::config::BotConfig;
use hbot_rpc::app::runtime::sandbox_handler::{
    REPLY_CAPACITY, SandboxHandler, SandboxHandlerConfig,
};
use hbot_rpc::{Event, EventBus, LogLevel};
use hbot_store::{Bot, LogRow, Project, Store};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::task::JoinHandle;

/// 单个用例的硬上限。超过它就算挂住（任务是「每条 30 秒内结束」，这里留 5 秒余量）。
pub const DEADLINE: Duration = Duration::from_secs(25);

/// 假适配器的事件通道容量。真实适配器是 `ADAPTER_EVENT_CAPACITY = 256`，
/// 这里取同量级，够本套件所有用例用。
const EVENT_CAPACITY: usize = 256;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 独立前缀的临时目录，`Drop` 时清理。
///
/// 前缀固定 `hbot-parity-<label>-<pid>-<seq>`：并行跑多个用例时互不干扰，
/// 也绝不可能碰到仓库里的真实项目目录（`std::env::temp_dir()` 之外不写任何东西）。
pub struct TempDir {
    path: PathBuf,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    /// 建一个空的临时根目录。
    pub fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("hbot-parity-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    /// 临时根目录。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 根目录下的一个路径（不创建）。
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    /// 写一个文件（父目录自动创建）。
    pub fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建父目录");
        }
        std::fs::write(&path, body).expect("写文件");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 清理失败不算测试失败：Windows 上还有句柄时删不掉，但目录在 %TEMP% 下，
        // 泄漏一个几十 KB 的目录比让用例在收尾处 panic 更无害。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 在临时目录里打开**真的** SQLite（`Store` 自己会建表 + 跑迁移 + 开 WAL）。
pub fn open_store(dir: &TempDir) -> Arc<Store> {
    Arc::new(Store::open(dir.join("hbot.sqlite3")).expect("打开临时 SQLite"))
}

/// 打开同一个库文件（模拟「重启后新进程重新打开」）。
pub fn reopen_store(db_path: &Path) -> Arc<Store> {
    Arc::new(Store::open(db_path).expect("重新打开同一个 SQLite 库"))
}

/// 把阻塞的仓储调用挪到 `spawn_blocking`：`Store` 的 API 是同步的，从 async 里
/// 直接调用会占住 tokio worker（真实调用方也是这么做的，见 `AppState::blocking`）。
pub async fn blocking<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .expect("spawn_blocking 任务 panic")
}

/// 插入 `projects` 行（`bots.project_id` 的外键依赖它）。
pub async fn seed_project(store: &Arc<Store>, project_id: ProjectId, path: &str) {
    let store = Arc::clone(store);
    let path = path.to_owned();
    blocking(move || {
        store.insert_project(&Project {
            id: project_id,
            name: "parity".to_owned(),
            path,
            created_at: 1_700_000_000_000,
        })
    })
    .await
    .expect("插入 projects 行");
}

/// 轮询等待一个条件成立（最多 20 秒）。不靠「睡一下大概就好了」：条件不成立就带着
/// label 失败，说明是哪个不变量没发生。
pub async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..2_000 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("等待条件超时：{label}");
}

/// 同一个轮询器，但谓词本身是 async（例如要 `spawn_blocking` 读库）。
pub async fn wait_until_async<F, Fut>(label: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..2_000 {
        if predicate().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("等待条件超时：{label}");
}

/// 一个群消息事件（`payload.text` 是 handler 能读到的那句话）。
pub fn group_event(conversation_external_id: &str, external_id: &str, text: &str) -> PlatformEvent {
    PlatformEvent::new(
        external_id,
        ConversationRef::new(conversation_external_id, ConversationKind::Group),
        "user-1",
        serde_json::json!({ "text": text }),
    )
}

/// 一个私聊消息事件。
pub fn private_event(
    conversation_external_id: &str,
    external_id: &str,
    text: &str,
) -> PlatformEvent {
    PlatformEvent::new(
        external_id,
        ConversationRef::new(conversation_external_id, ConversationKind::Private),
        "user-1",
        serde_json::json!({ "text": text }),
    )
}

/// 组装 `BotSpec`（`config_json` 只放非敏感字段，与生产路径一致）。
pub fn bot_spec(bot_id: BotId, project_id: ProjectId, platform: &str) -> BotSpec {
    let mut spec = BotSpec::new(bot_id, project_id, "parity 测试 bot", platform);
    spec.config_json = serde_json::json!({ "wsUrl": "ws://127.0.0.1:3001" }).to_string();
    spec
}

/// 用真实 `SupervisorDeps::new` + 可选调参组装一个真实 supervisor。
///
/// 泛型而不是写死 [`FakeAdapter`]：真实协议用例（`parity_14`）要把**真的**
/// `OneBotAdapter` 交给 supervisor，只换「外部世界」，supervisor 那一层不许假。
pub fn build_supervisor<A>(
    store: Arc<Store>,
    adapter: Arc<A>,
    executor: Arc<dyn HandlerExecutor>,
    spec: BotSpec,
    tune: impl FnOnce(&mut SupervisorDeps),
) -> Arc<Supervisor>
where
    A: BotAdapter + 'static,
{
    let adapter: Arc<dyn BotAdapter> = adapter;
    let mut deps = SupervisorDeps::new(
        store,
        Arc::new(UuidIds),
        Arc::new(SystemClock),
        adapter,
        executor,
    );
    tune(&mut deps);
    Arc::new(Supervisor::new(spec, deps))
}

/// 假平台适配器的「延迟就绪」闸门。
///
/// `entered` 在 `subscribe()` 被调用时放行一个许可，`go` 由测试放行后才真正返回
/// 订阅——这样测试能在「bot 行已写、会话已登记、但事件流还没交出去」这个窗口里
/// 往库里插一条未处理入站（parity #2 的起点）。
#[derive(Clone)]
pub struct SubscribeGate {
    /// `subscribe()` 进入时 `add_permits(1)`。
    pub entered: Arc<Semaphore>,
    /// 测试 `add_permits(1)` 后 `subscribe()` 继续。
    pub go: Arc<Semaphore>,
}

impl SubscribeGate {
    /// 两扇门都关着的闸门。
    pub fn closed() -> Self {
        Self {
            entered: Arc::new(Semaphore::new(0)),
            go: Arc::new(Semaphore::new(0)),
        }
    }

    /// 等 `subscribe()` 真的被调到。
    pub async fn wait_entered(&self) {
        let permit = self
            .entered
            .clone()
            .acquire_owned()
            .await
            .expect("entered 闸门已关闭");
        permit.forget();
    }

    /// 放 `subscribe()` 过去。
    pub fn release(&self) {
        self.go.add_permits(1);
    }
}

#[derive(Default)]
struct AdapterInner {
    sender: Option<mpsc::Sender<PlatformEvent>>,
    sent: Vec<OutboundEnvelope>,
    subscribe_calls: u32,
    send_calls: u32,
    send_failures_left: u32,
}

/// 假平台适配器：唯一的「外部世界」。
///
/// 模拟的真实协议行为：
/// * `declared_conversations()` —— 平台声明的会话清单（supervisor 启动时据此建 conversations 行）；
/// * `subscribe()` —— 事件流只能交出去一次（真 OneBot 适配器第二条订阅返回 -32003）；
/// * `send()` —— 记录完整 `OutboundEnvelope`（含 `reply_to`），可配置成失败以驱动 outbox 重试。
///
/// **没有**模拟：OneBot 的 WebSocket 握手 / echo / 心跳 / 消息段编码 / 限流 /
/// `self_id` 过滤。这些属于 `hbot-adapters` 的协议层，本套件不覆盖。
pub struct FakeAdapter {
    declared: Vec<ConversationRef>,
    gate: Mutex<Option<SubscribeGate>>,
    inner: Mutex<AdapterInner>,
}

impl FakeAdapter {
    /// 一个立即就绪的假适配器。
    pub fn new(declared: Vec<ConversationRef>) -> Arc<Self> {
        Self::build(declared, None)
    }

    /// 一个「延迟就绪」的假适配器：`subscribe()` 会停在 [`SubscribeGate`] 上。
    pub fn with_gate(declared: Vec<ConversationRef>, gate: SubscribeGate) -> Arc<Self> {
        Self::build(declared, Some(gate))
    }

    fn build(declared: Vec<ConversationRef>, gate: Option<SubscribeGate>) -> Arc<Self> {
        Arc::new(Self {
            declared,
            gate: Mutex::new(gate),
            inner: Mutex::new(AdapterInner::default()),
        })
    }

    /// 往事件流里投一条（`subscribe()` 之前返回 false）。
    pub fn push(&self, event: PlatformEvent) -> bool {
        let inner = lock(&self.inner);
        match inner.sender.as_ref() {
            Some(sender) => sender.try_send(event).is_ok(),
            None => false,
        }
    }

    /// 收到过的出站信封（按投递顺序）。
    pub fn sent(&self) -> Vec<OutboundEnvelope> {
        lock(&self.inner).sent.clone()
    }

    /// 收到过的出站正文。
    pub fn sent_texts(&self) -> Vec<String> {
        lock(&self.inner)
            .sent
            .iter()
            .map(|sent| sent.text.clone())
            .collect()
    }

    /// 收到过的 `reply_to`（`None` = 主动消息）。
    pub fn sent_reply_to(&self) -> Vec<Option<String>> {
        lock(&self.inner)
            .sent
            .iter()
            .map(|sent| sent.reply_to.clone())
            .collect()
    }

    /// `send()` 被调用的次数（含失败的）。
    pub fn send_calls(&self) -> u32 {
        lock(&self.inner).send_calls
    }

    /// `subscribe()` 被调用的次数。
    pub fn subscribe_calls(&self) -> u32 {
        lock(&self.inner).subscribe_calls
    }

    /// 接下来 `count` 次 `send()` 返回错误（`u32::MAX` = 一直失败）。
    pub fn fail_sends(&self, count: u32) {
        lock(&self.inner).send_failures_left = count;
    }
}

#[async_trait]
impl BotAdapter for FakeAdapter {
    fn id(&self) -> &'static str {
        "parity-fake"
    }

    fn declared_conversations(&self) -> Vec<ConversationRef> {
        self.declared.clone()
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
        {
            let mut inner = lock(&self.inner);
            inner.subscribe_calls += 1;
            // 真 OneBot 适配器只允许订阅一次；这里照样只换发新 sender，
            // 上一份 receiver 已被 supervisor 持有（本套件没有第二条订阅路径）。
            inner.sender = Some(tx);
        }
        let gate = lock(&self.gate).clone();
        if let Some(gate) = gate {
            gate.entered.add_permits(1);
            let permit = gate
                .go
                .acquire()
                .await
                .map_err(|_| HbotError::internal("parity 假适配器：就绪闸门已关闭"))?;
            permit.forget();
        }
        Ok(AdapterSubscription::new(rx))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        let mut inner = lock(&self.inner);
        inner.send_calls += 1;
        if inner.send_failures_left > 0 {
            if inner.send_failures_left != u32::MAX {
                inner.send_failures_left -= 1;
            }
            return Err(HbotError::internal(
                "parity 假适配器：模拟平台拒收（驱动 outbox 重试）",
            ));
        }
        inner.sent.push(envelope);
        Ok(())
    }
}

/// 生产路径的 `AdapterFactory`：真 `bot_start` 会通过它建适配器。
/// 这里把「外部世界」换掉，其余（`read_bot_config` → `BotConfig` → `create`）是真的。
pub struct FakeFactory {
    adapter: Arc<FakeAdapter>,
    created: AtomicU32,
    /// 最近一次 `create()` 收到的配置（`access_token` 原样保留）。
    ///
    /// parity #9 用它证明「密钥确实走到了外部世界边界」——密钥进得来，日志却不许出现。
    seen_config: Mutex<Option<BotConfig>>,
}

impl FakeFactory {
    /// 用一个固定适配器应答每一次 `create()`。
    pub fn new(adapter: Arc<FakeAdapter>) -> Self {
        Self {
            adapter,
            created: AtomicU32::new(0),
            seen_config: Mutex::new(None),
        }
    }

    /// `create()` 被调用的次数。
    pub fn created(&self) -> u32 {
        self.created.load(Ordering::SeqCst)
    }

    /// 最近一次 `create()` 收到的 `access_token`（`None` = 配置里本来就没给）。
    pub fn seen_access_token(&self) -> Option<String> {
        lock(&self.seen_config)
            .as_ref()
            .and_then(|config| config.access_token.clone())
    }

    /// 最近一次 `create()` 收到的 `ws_url`（`BotConfig::ws_url` 是 `Option<String>`，
    /// QQ 官方平台没有用户可配的地址，所以这里用 `and_then` 摊平一层）。
    pub fn seen_ws_url(&self) -> Option<String> {
        lock(&self.seen_config)
            .as_ref()
            .and_then(|config| config.ws_url.clone())
    }
}

impl AdapterFactory for FakeFactory {
    fn create(&self, _bot: &Bot, config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        *lock(&self.seen_config) = Some(config.clone());
        Ok(Arc::clone(&self.adapter) as Arc<dyn BotAdapter>)
    }
}

/// 执行器里看到的一条消息。
#[derive(Clone, Debug)]
pub struct SeenMessage {
    /// 平台消息 ID。
    pub external_id: String,
    /// `payload.text`。
    pub text: Option<String>,
    /// 会话外部 ID。
    pub conversation: String,
}

struct ExecutorInner {
    seen: Vec<SeenMessage>,
    running: usize,
    max_running: usize,
    hold: Option<Arc<Semaphore>>,
    /// 这个会话的 handler 永不返回（模拟「卡死的会话」，parity #13 用它制造
    /// 「全局在跑数永远不归零」的局面）。
    wedged: Option<String>,
    replies: Option<mpsc::Sender<OutboundRequest>>,
    fail_remaining: u32,
}

/// 录制型 `HandlerExecutor`：在 handler 内部记录「谁先进入」「同时有几条在跑」，
/// 可选地把回复交给一条真实的 `OutboundRequest` 通道（再由测试搬进 `supervisor.send`）。
///
/// 这是本套件唯一「假」的 handler 层：它替换的是**用户代码**（QuickJS 里的 bot 脚本），
/// 不是被验证的 supervisor / 队列 / outbox。需要真 QuickJS 的用例（#4/#5/#9）
/// 直接接 `SandboxHandler`。
pub struct RecordingExecutor {
    inner: Mutex<ExecutorInner>,
}

impl RecordingExecutor {
    /// 空的录制器。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ExecutorInner {
                seen: Vec::new(),
                running: 0,
                max_running: 0,
                hold: None,
                wedged: None,
                replies: None,
                fail_remaining: 0,
            }),
        })
    }

    /// 每条消息在 handler 里先申请一个许可再继续。
    ///
    /// 给满 `N` 个许可 = 「立即放行但保留并发观测窗」；给 `0` 个 = 「全部挂住，由测试逐条放行」。
    pub fn hold_with(&self, hold: Arc<Semaphore>) {
        lock(&self.inner).hold = Some(hold);
    }

    /// handler 产出的回复往这条通道送（测试再搬进真实的 `supervisor.send`）。
    pub fn with_replies(&self, replies: mpsc::Sender<OutboundRequest>) {
        lock(&self.inner).replies = Some(replies);
    }

    /// 接下来 `count` 条消息的 handler 返回错误（`u32::MAX` = 一直失败）。
    pub fn fail_next(&self, count: u32) {
        lock(&self.inner).fail_remaining = count;
    }

    /// 让某个会话的 handler 永不返回（模拟卡死）。这条会话会一直占着
    /// `max_in_flight` 的一个名额，于是「全局在跑数」永远不归零。
    pub fn wedge_conversation(&self, conversation_external_id: &str) {
        lock(&self.inner).wedged = Some(conversation_external_id.to_owned());
    }

    /// 执行过的消息（按进入顺序）。
    pub fn seen(&self) -> Vec<SeenMessage> {
        lock(&self.inner).seen.clone()
    }

    /// 执行过的平台消息 ID（按进入顺序）。
    pub fn external_ids(&self) -> Vec<String> {
        lock(&self.inner)
            .seen
            .iter()
            .map(|seen| seen.external_id.clone())
            .collect()
    }

    /// 峰值并发（同一会话串行时必然是 1）。
    pub fn max_running(&self) -> usize {
        lock(&self.inner).max_running
    }

    /// 当前在跑的数量。
    pub fn running(&self) -> usize {
        lock(&self.inner).running
    }
}

#[async_trait]
impl HandlerExecutor for RecordingExecutor {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        let hold = {
            let mut inner = lock(&self.inner);
            inner.seen.push(SeenMessage {
                external_id: ctx.external_id().to_owned(),
                text: ctx.text().map(str::to_owned),
                conversation: ctx.conversation.external_id.clone(),
            });
            inner.running += 1;
            inner.max_running = inner.max_running.max(inner.running);
            inner.hold.clone()
        };

        // 卡死的会话：计入「在跑」但永不返回（用来让全局在跑数永不归零）。
        let wedged = {
            let inner = lock(&self.inner);
            inner
                .wedged
                .as_deref()
                .is_some_and(|conversation| conversation == ctx.conversation.external_id)
        };
        if wedged {
            std::future::pending::<()>().await;
        }

        if let Some(hold) = hold {
            let permit = hold
                .acquire()
                .await
                .map_err(|_| HbotError::internal("parity 录制执行器：放行闸门已关闭"))?;
            // 让出执行权两次：若同一会话真的被并发派发，第二个 handler 会在这两个
            // 调度点里进入并把 max_running 抬到 2（串行实现永远看不到 2）。
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            permit.forget();
        }

        let (fail, replies) = {
            let mut inner = lock(&self.inner);
            inner.running -= 1;
            let fail = inner.fail_remaining > 0;
            if fail && inner.fail_remaining != u32::MAX {
                inner.fail_remaining -= 1;
            }
            (fail, inner.replies.clone())
        };

        if fail {
            return Err(HbotError::internal("parity 录制执行器：模拟 handler 失败"));
        }

        if let Some(replies) = replies {
            let reply = OutboundRequest {
                conversation: ctx.conversation.clone(),
                text: format!("reply-{}", ctx.text().unwrap_or("?")),
                reply_to: Some(ctx.external_id().to_owned()),
                attachment: None,
            };
            replies
                .send(reply)
                .await
                .map_err(|_| HbotError::internal("parity 录制执行器：回复通道已关闭"))?;
        }
        Ok(())
    }
}

/// 回复搬运任务：`handler 产出 → supervisor.send() → outbox 行 → 适配器发送`。
///
/// 与 `hbot-rpc` 的 `bot_start` 里那条单向搬运同形——supervisor 自己不调 handler，
/// 这条单行道保证「回复必须经过 outbox」。
pub struct ReplyRelay {
    requests: Arc<Mutex<Vec<OutboundRequest>>>,
    errors: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl ReplyRelay {
    /// 起一条搬运任务。
    pub fn spawn(
        mut replies: mpsc::Receiver<OutboundRequest>,
        supervisor: Arc<Supervisor>,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let sink = Arc::clone(&errors);
        let task = tokio::spawn(async move {
            while let Some(request) = replies.recv().await {
                lock(&recorded).push(request.clone());
                if let Err(error) = supervisor.send(request).await {
                    // 不 panic（spawned task 里 panic 只会静默杀掉这条搬运），
                    // 记下来让用例断言为空。
                    lock(&sink).push(error.to_string());
                }
            }
        });
        Self {
            requests,
            errors,
            task,
        }
    }

    /// 交给 `supervisor.send` 的每一条回复（含 `reply_to`）。
    ///
    /// parity #4 用它看「恢复出来的 `PendingOutbound` 带的是哪个 `reply_to`」——
    /// 这是重启后 outbox 真正拿去发送的那份数据。
    pub fn requests(&self) -> Vec<OutboundRequest> {
        lock(&self.requests).clone()
    }

    /// 搬运失败的错误（用例结束时断言为空）。
    pub fn errors(&self) -> Vec<String> {
        lock(&self.errors).clone()
    }
}

impl Drop for ReplyRelay {
    fn drop(&mut self) {
        // 有界 join：搬运任务是死循环，只能 abort；不要在这里 await。
        self.task.abort();
    }
}

/// 往项目根写一份 bot 脚本（`<root>/src/main.ts`）。**QuickJS 是真跑的**，
/// 这里只是把用户代码放到真实位置：编译、isolate、内存上限、超时全是
/// `hbot-code` / `SandboxHandler` 的真实现。
pub fn write_bot_script(project_root: &Path, source: &str) {
    let src = project_root.join("src");
    std::fs::create_dir_all(&src).expect("创建 src 目录");
    std::fs::write(src.join("main.ts"), source).expect("写 main.ts");
}

/// 组装**真的** `SandboxHandler`（QuickJS 沙箱 + 真 store + 真事件总线）。
///
/// 返回的 receiver 就是 handler 产出的回复流：真实路径里它由 `bot_start` 搬进
/// `supervisor.send`，本套件用 [`ReplyRelay`] 做同一件事。
pub fn sandbox_executor(
    store: Arc<Store>,
    events: Arc<EventBus>,
    bot_id: BotId,
    project_root: PathBuf,
    limits: hbot_code::Limits,
) -> (Arc<SandboxHandler>, mpsc::Receiver<OutboundRequest>) {
    let (replies, replies_rx) = mpsc::channel(REPLY_CAPACITY);
    let handler = SandboxHandler::new(SandboxHandlerConfig {
        bot_id,
        project_root,
        limits,
        store,
        events,
        clock: Arc::new(SystemClock),
        replies,
        // 对等性测试不装状态能力：本文件的新断言都不依赖 `ctx.kv`。
        state: None,
        // 同样不装智能能力：这些用例不调 `ctx.ai.complete`，装了反而多一层不确定性。
        ai: None,
        // 同理不装作业端口：对等性用例不调 `ctx.jobs.enqueue`。
        jobs: None,
        waits: None,
        // 也不装 SQL 后端：对等性用例不调 `ctx.sql.*`（不装配时调用会报 `HBOT_SQL_UNAVAILABLE`）。
        sql: None,
        files: None,
    })
    // 热重载防抖在生产里是 300ms；测试里关掉，否则每条消息都要多等一拍。
    .with_debounce(Duration::ZERO);
    (Arc::new(handler), replies_rx)
}

/// `Event::RunLog` 采集器：把事件总线上的日志行收下来，供用例断言「里面没有密钥」。
pub struct LogCapture {
    entries: Arc<Mutex<Vec<(LogLevel, String)>>>,
    task: JoinHandle<()>,
}

impl LogCapture {
    /// 订阅事件总线并开始收集。
    pub fn spawn(mut events: broadcast::Receiver<Event>) -> Self {
        let entries = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&entries);
        let task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::RunLog { level, line, .. }) => lock(&sink).push((level, line)),
                    Ok(_) => {}
                    // 落后就跳过：用例要证的是「日志里没有密钥」，不是事件一条不丢
                    // （事件总线的丢失语义由 `EventBus` 自己的用例守着）。
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self { entries, task }
    }

    /// 收到的日志行（按到达顺序）。
    pub fn lines(&self) -> Vec<String> {
        lock(&self.entries)
            .iter()
            .map(|(_, line)| line.clone())
            .collect()
    }

    /// 收到的 `(级别, 正文)`。
    pub fn entries(&self) -> Vec<(LogLevel, String)> {
        lock(&self.entries).clone()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 读一个 bot 的全部日志行（最旧的在前，便于断言）。
pub async fn logs_of(store: &Arc<Store>, bot_id: BotId) -> Vec<LogRow> {
    let store = Arc::clone(store);
    blocking(move || store.list_logs(Some(&bot_id), 200))
        .await
        .expect("读 logs 表")
        .into_iter()
        .rev()
        .collect()
}

/// 某个 bot 还没被处理掉的入站 `external_id`（按 `(received_at, rowid)` 升序）。
///
/// 注意 store 没有「列出全部 inbound」的读接口：`list_inbound_replay_for_bot` 与
/// `list_unprocessed_inbound` **都只返回 `processed_at IS NULL` 的行**（见
/// `crates/hbot-store/src/repositories/inbound.rs:79-120`）。所以这里只能断言「谁还欠着」；
/// 「谁已经处理过」要靠「handler 真的跑过它」反推（先落库再处理 ⇒ 跑过即落过库）。
pub async fn unprocessed_inbound_ids(store: &Arc<Store>, bot_id: BotId) -> Vec<String> {
    let store = Arc::clone(store);
    blocking(move || store.list_inbound_replay_for_bot(&bot_id, None, 500))
        .await
        .expect("读 inbound 表")
        .into_iter()
        .map(|row| row.message.external_id)
        .collect()
}
