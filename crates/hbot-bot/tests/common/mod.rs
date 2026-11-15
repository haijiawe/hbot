//! 集成测试的共享脚手架：临时库、注入端口（时钟/ID）、fake 适配器与 fake 执行器。
//!
//! 全部 fake 都是**确定性**的：没有随机数、没有真实网络、没有系统时间。测试因此可以
//! 断言顺序与并发上限，而不是“跑一下不报错”。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hbot_bot::adapter::{AdapterSubscription, BotAdapter, OutboundEnvelope};
use hbot_bot::event::{ConversationRef, PlatformEvent};
use hbot_bot::handler::{HandlerExecutor, MessageCtx};
use hbot_bot::supervisor::{BotSpec, Supervisor, SupervisorDeps};
use hbot_core::{
    ApprovalId, BotId, Clock, ConversationId, ConversationKind, EditId, HbotError, IdGenerator,
    InboundId, MessageId, OutboundId, ProjectId, SearchId, SessionId, Timestamp,
};
use hbot_store::{Project, Store};
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

/// 临时目录；`Drop` 时递归删除（Windows 上先让 `Store` 掉线，见字段顺序）。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 建一个唯一目录。
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("hbot-bot-it-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// 测试库路径。
    pub fn db_path(&self) -> PathBuf {
        self.path.join("hbot.sqlite3")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 固定起点的单调时钟：每次 `now()` 前进 1 ms，便于断言时间只增不减。
pub struct TestClock {
    next: AtomicI64,
}

impl TestClock {
    /// 从 1_700_000_000_000（2023-11）开始。
    pub fn new() -> Self {
        Self {
            next: AtomicI64::new(1_700_000_000_000),
        }
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.next.fetch_add(1, Ordering::SeqCst))
    }
}

/// 递增 ID 生成器：每次调用返回不同 UUID（同一次运行内不会碰撞）。
pub struct TestIds {
    next: AtomicU64,
}

impl TestIds {
    /// 从 1 开始。
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    fn uuid(&self) -> Uuid {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        Uuid::from_u128(u128::from(n) | (1 << 120))
    }
}

impl IdGenerator for TestIds {
    fn project_id(&self) -> ProjectId {
        ProjectId::from_uuid(self.uuid())
    }

    fn bot_id(&self) -> BotId {
        BotId::from_uuid(self.uuid())
    }

    fn conversation_id(&self) -> ConversationId {
        ConversationId::from_uuid(self.uuid())
    }

    fn session_id(&self) -> SessionId {
        SessionId::from_uuid(self.uuid())
    }

    fn message_id(&self) -> MessageId {
        MessageId::from_uuid(self.uuid())
    }

    fn inbound_id(&self) -> InboundId {
        InboundId::from_uuid(self.uuid())
    }

    fn outbound_id(&self) -> OutboundId {
        OutboundId::from_uuid(self.uuid())
    }

    fn approval_id(&self) -> ApprovalId {
        ApprovalId::from_uuid(self.uuid())
    }

    fn edit_id(&self) -> EditId {
        EditId::from_uuid(self.uuid())
    }

    fn search_id(&self) -> SearchId {
        SearchId::from_uuid(self.uuid())
    }
}

/// fake 适配器：可预先声明会话、可控 send 失败次数，记录所有已发送信封。
pub struct TestAdapter {
    declared: Vec<ConversationRef>,
    subscribe_calls: AtomicU32,
    send_calls: AtomicU32,
    /// 剩余“应失败”的发送次数（模拟平台抖动）。
    send_failures: AtomicU32,
    sender: Mutex<Option<mpsc::Sender<PlatformEvent>>>,
    sent: Mutex<Vec<OutboundEnvelope>>,
}

impl TestAdapter {
    /// 用“启动时应预建会话”的列表构造。
    pub fn new(declared: Vec<ConversationRef>) -> Self {
        Self {
            declared,
            subscribe_calls: AtomicU32::new(0),
            send_calls: AtomicU32::new(0),
            send_failures: AtomicU32::new(0),
            sender: Mutex::new(None),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// 前 `count` 次发送失败。
    pub fn fail_next_sends(&self, count: u32) {
        self.send_failures.store(count, Ordering::SeqCst);
    }

    /// 已发送信封（按发送顺序）。
    pub fn sent(&self) -> Vec<OutboundEnvelope> {
        self.sent.lock().expect("sent lock").clone()
    }

    /// 发送调用次数（含失败）。
    pub fn send_calls(&self) -> u32 {
        self.send_calls.load(Ordering::SeqCst)
    }

    /// 订阅建立次数（用于验证“崩溃后不自动重启”）。
    pub fn subscribe_calls(&self) -> u32 {
        self.subscribe_calls.load(Ordering::SeqCst)
    }

    /// 模拟平台推送一条事件；未订阅时返回 `false`。
    pub fn push(&self, event: PlatformEvent) -> bool {
        match self.sender.lock().expect("sender lock").as_ref() {
            Some(sender) => sender.try_send(event).is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl BotAdapter for TestAdapter {
    fn id(&self) -> &'static str {
        "test"
    }

    fn declared_conversations(&self) -> Vec<ConversationRef> {
        self.declared.clone()
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        let (sender, subscription) = AdapterSubscription::channel(64);
        *self.sender.lock().expect("sender lock") = Some(sender);
        Ok(subscription)
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        self.send_calls.fetch_add(1, Ordering::SeqCst);
        let remaining = self.send_failures.load(Ordering::SeqCst);
        if remaining > 0 {
            self.send_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(HbotError::internal("platform send failed on purpose"));
        }
        self.sent.lock().expect("sent lock").push(envelope);
        Ok(())
    }
}

/// fake 执行器：记录执行顺序与并发峰值，可加闸门、可按次数失败。
#[derive(Default)]
pub struct TestExecutor {
    seen: Mutex<Vec<Seen>>,
    gate: Mutex<Option<Arc<Semaphore>>>,
    running: AtomicUsize,
    max_running: AtomicUsize,
    fail_remaining: AtomicU32,
}

/// 一次执行记录。
#[derive(Clone, Debug)]
pub struct Seen {
    /// 平台事件 ID。
    pub external_id: String,
    /// 本地会话 ID（用于验证会话隔离）。
    pub conversation_id: ConversationId,
    /// 消息文本（`payload.text`）。
    pub text: Option<String>,
}

impl TestExecutor {
    /// 空执行器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 让每个 handler 卡在闸门上，直到测试放行（`Semaphore` 许可数即放行次数）。
    pub fn set_gate(&self, gate: Arc<Semaphore>) {
        *self.gate.lock().expect("gate lock") = Some(gate);
    }

    /// 前 `count` 次执行失败；`u32::MAX` 表示一直失败。
    pub fn fail_next(&self, count: u32) {
        self.fail_remaining.store(count, Ordering::SeqCst);
    }

    /// 已成功执行的记录（按完成顺序）。
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen lock").clone()
    }

    /// 当前同时在执行的 handler 数。
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    /// 观测到的并发峰值。
    pub fn max_running(&self) -> usize {
        self.max_running.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HandlerExecutor for TestExecutor {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(running, Ordering::SeqCst);

        // 先取出闸门再 await：临时 `MutexGuard` 不能跨 await（会破坏 `Send`）。
        let gate = self.gate.lock().expect("gate lock").clone();
        if let Some(gate) = gate {
            match gate.acquire().await {
                Ok(permit) => drop(permit),
                Err(_) => {
                    self.running.fetch_sub(1, Ordering::SeqCst);
                    return Err(HbotError::internal("executor gate closed"));
                }
            }
        }

        let remaining = self.fail_remaining.load(Ordering::SeqCst);
        if remaining > 0 {
            if remaining != u32::MAX {
                self.fail_remaining.fetch_sub(1, Ordering::SeqCst);
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
            return Err(HbotError::internal("executor failed on purpose"));
        }

        self.seen.lock().expect("seen lock").push(Seen {
            external_id: ctx.external_id().to_owned(),
            conversation_id: ctx.conversation_id(),
            text: ctx.text().map(ToOwned::to_owned),
        });
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 一套完整依赖：真 SQLite（临时文件）+ 全 fake 端口。
pub struct Fixture {
    /// 已迁移的库。
    pub store: Arc<Store>,
    /// 注入时钟。
    pub clock: Arc<TestClock>,
    /// 注入 ID。
    pub ids: Arc<TestIds>,
    /// fake 适配器。
    pub adapter: Arc<TestAdapter>,
    /// fake 执行器。
    pub executor: Arc<TestExecutor>,
    /// 预插入的 `projects.id`。
    pub project_id: ProjectId,
    /// 临时目录（最后声明 → 最后释放，保证 `Store` 先掉线）。
    temp: TempDir,
}

impl Fixture {
    /// 建库并插入 `projects` 行（`bots.project_id` 的外键目标）。
    pub fn new(declared: Vec<ConversationRef>) -> Self {
        let temp = TempDir::new();
        let store = Arc::new(Store::open(temp.db_path()).expect("open store"));
        let project_id = ProjectId::from_uuid(Uuid::from_u128(0x5151));
        store
            .insert_project(&Project {
                id: project_id,
                name: "it-project".to_owned(),
                path: "E:\\it-project".to_owned(),
                created_at: 0,
            })
            .expect("insert project");
        Self {
            store,
            clock: Arc::new(TestClock::new()),
            ids: Arc::new(TestIds::new()),
            adapter: Arc::new(TestAdapter::new(declared)),
            executor: Arc::new(TestExecutor::new()),
            project_id,
            temp,
        }
    }

    /// 默认依赖（测试按需改公开字段）。
    pub fn deps(&self) -> SupervisorDeps {
        SupervisorDeps::new(
            Arc::clone(&self.store),
            self.ids.clone(),
            self.clock.clone(),
            self.adapter.clone(),
            self.executor.clone(),
        )
    }

    /// 用给定依赖装配 supervisor。
    pub fn supervisor(&self, bot_id: BotId, deps: SupervisorDeps) -> Arc<Supervisor> {
        Arc::new(Supervisor::new(
            BotSpec::new(bot_id, self.project_id, "测试 bot", "test"),
            deps,
        ))
    }

    /// 临时目录路径（供“重启后同一库”的用例复用）。
    pub fn db_path(&self) -> PathBuf {
        self.temp.db_path()
    }
}

/// 构造一条入站事件（`payload.text` = `text`）。
pub fn event(conversation: &str, external_id: &str, text: &str) -> PlatformEvent {
    PlatformEvent::new(
        external_id,
        ConversationRef::new(conversation, ConversationKind::Group),
        "user-1",
        serde_json::json!({ "text": text }),
    )
}

/// 轮询等待条件成立（2 ms 一次，最多 5 s），失败时把 `label` 带进 panic。
pub async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..2_500 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition not reached within 5s: {label}");
}
