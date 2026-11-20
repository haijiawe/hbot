//! 每个 bot 一个 supervisor：生命周期状态机 + 启动/停止顺序 + 崩溃降级。
//!
//! ## 状态机
//!
//! ```text
//! ┌─────────┐  start()   ┌──────────┐  就绪   ┌─────────┐
//! │ Stopped │ ─────────► │ Starting │ ──────► │ Running │
//! └─────────┘            └──────────┘         └─────────┘
//!      ▲                       │失败               │ 连续 3 次 handler 崩溃
//!      │                       ▼                   ▼
//!      │                    Stopped          ┌──────────┐
//!      │                                     │ Degraded │
//!      │        stop()  ┌──────────┐         └──────────┘
//!      └─────────────── │ Stopping │ ◄─────────────┘（也可直接从 Running 进入）
//!                       └──────────┘
//! ```
//!
//! * `Starting` 期间不接受 `inject`（返回 `Conflict`）；任何一步失败都回到 `Stopped`
//!   并把错误上报，不留下半个 runtime。
//! * `Degraded` 是**粘性**的：连续 [`SupervisorDeps::failure_threshold`] 次 handler
//!   崩溃后进入，停止读取新事件，但不自动重启（`docs/02-architecture.md` §12，避免崩溃
//!   循环）。只有显式 `start()` 才会解除（先 `stop()` 再 `start()`）。
//! * 关停过程中如果排空超时，仍然继续取消 worker / flush outbox / 标记 `Stopped`，并把
//!   错误返回给调用方：状态不能卡在 `Stopping`。
//!
//! ## 启动顺序（不可换）
//!
//! ```text
//! 1. bot 行          ensure_bot_row      —— 后面所有 FK 都指向 bots
//! 2. conversations   ensure_conversation —— inbound 的 FK 指向 conversations
//! 3. 订阅适配器       adapter.subscribe   —— 先有队列再收事件（AGENTS.md §7.1）
//! 4. 恢复 outbox     outbox.recover      —— 上次崩溃留下的 pending 先续发
//! 5. 重放入站         replay_unprocessed  —— 上次没处理完的 inbound 重新入队
//! 6. 开始读事件       read_loop           —— 最后才把事件放进队列
//! ```
//!
//! 换成别的顺序的后果是具体的：先订阅后建会话 → 第一条消息写 `inbound` 撞外键；先读事件
//! 后恢复 outbox → 新消息的回复排在旧 pending 之前；先订阅后建队列 → 事件在内存里堆着、
//! 队列满时无处背压；先读事件后重放 → 同一会话里旧消息排在新消息之后（违背会话内 FIFO）。
//!
//! 第 5 步是 **at-least-once**：重放只保证"不丢"，不保证"不重复"（进程在
//! `on_message` 成功与 `processed_at` 落库之间被杀时，那条消息会二次执行）。重复投递
//! 的兜底是 `inbound` 的 `(bot_id, external_id)` 唯一约束 + 适配器侧的消息 id 去重。
//! 代价换来的是"进程崩了消息还在"，这比"宁可丢也不重"重要得多。
//!
//! ## 停止顺序（不可换）
//!
//! ```text
//! 1. 停止读取          stop_tx 触发，read_loop 退出（在途消息已落库）
//! 2. 等队列排空        queues.wait_drained(drain_timeout)，超时继续往下
//! 3. 取消 worker       queues.shutdown(shutdown_timeout)，超时 abort
//! 4. flush outbox      outbox.stop(shutdown_timeout)，未发出的保留 pending
//! 5. 标记 Stopped      状态与快照更新（finally 语义，不卡 Stopping）
//! ```

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{
    BotId, BotStatus, Clock, ConversationKind, HbotError, IdGenerator, InboundMessage, MessageId,
    ProjectId, Timestamp,
};
use hbot_store::{Bot as StoreBot, NewLog, Store};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use tokio::task::JoinHandle;

use crate::adapter::{AdapterSubscription, BotAdapter};
use crate::dedupe::{Admission, Inbox};
use crate::event::{ConversationRef, PlatformEvent};
use crate::handler::{HandlerExecutor, MessageCtx};
use crate::outbox::{
    NoPendingOutbox, OutboundRequest, Outbox, OutboxConfig, OutboxStats, PendingOutboxSource,
};
use crate::queue::{FailureTracker, QueueConfig, QueueError, SessionQueues};
use crate::store_ops::run_store;

/// 启动步骤，按实际执行顺序记录，供 UI 与测试断言“顺序没被打乱”。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStep {
    /// 写入/读取 `bots` 行。
    BotRow,
    /// 补建适配器声明的 `conversations` 行。
    Conversations,
    /// 建立入站订阅。
    Subscribe,
    /// 恢复未发送的 outbound 行。
    RecoverOutbox,
    /// 补偿上次运行遗留的未处理入站消息（`processed_at IS NULL`）。
    ///
    /// 排在这里是因为它必须先有 queues（重放要走与实时流量完全相同的入队路径），
    /// 且必须在 `Reading` 之前完成——否则新事件会插到旧消息前面，破坏会话内顺序。
    ReplayInbound,
    /// 开始读取事件（状态进入 `Running`）。
    Reading,
}

impl StartupStep {
    /// 稳定字符串（日志/快照展示用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BotRow => "bot_row",
            Self::Conversations => "conversations",
            Self::Subscribe => "subscribe",
            Self::RecoverOutbox => "recover_outbox",
            Self::ReplayInbound => "replay_inbound",
            Self::Reading => "reading",
        }
    }
}

/// supervisor 启动一个 bot 所需的静态信息。
#[derive(Clone, Debug)]
pub struct BotSpec {
    /// `bots.id`。
    pub bot_id: BotId,
    /// `bots.project_id`（`projects` 行必须已存在）。
    pub project_id: ProjectId,
    /// 展示名（`bots.name`）。
    pub name: String,
    /// 平台标识（`bots.platform`）。
    pub platform: String,
    /// 平台配置（`bots.config_json`）。
    pub config_json: String,
}

impl BotSpec {
    /// 构造一个默认开启的 bot 规格。
    pub fn new(
        bot_id: BotId,
        project_id: ProjectId,
        name: impl Into<String>,
        platform: impl Into<String>,
    ) -> Self {
        Self {
            bot_id,
            project_id,
            name: name.into(),
            platform: platform.into(),
            config_json: "{}".to_owned(),
        }
    }
}

/// supervisor 的外部依赖（组合根注入；测试里全部是 fake）。
pub struct SupervisorDeps {
    /// 存储（写 bot/conversation/inbound/outbound）。
    pub store: Arc<Store>,
    /// ID 生成端口（inbound/outbound 行主键）。
    pub ids: Arc<dyn IdGenerator>,
    /// 时钟端口。
    pub clock: Arc<dyn Clock>,
    /// 平台适配器。
    pub adapter: Arc<dyn BotAdapter>,
    /// 消息执行器（下一批接 QuickJS）。
    pub executor: Arc<dyn HandlerExecutor>,
    /// 恢复未发 outbound 的来源。
    pub pending_outbox: Arc<dyn PendingOutboxSource>,
    /// 会话队列配置。
    pub queue_config: QueueConfig,
    /// outbox 配置。
    pub outbox_config: OutboxConfig,
    /// 连续失败多少次进入 `Degraded`。
    pub failure_threshold: u32,
    /// 停止时等待队列排空的超时。
    pub drain_timeout: Duration,
    /// 停止时等待 worker/outbox 退出的超时。
    pub shutdown_timeout: Duration,
}

impl SupervisorDeps {
    /// 用默认配置构造；测试按需覆盖字段。
    pub fn new(
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        adapter: Arc<dyn BotAdapter>,
        executor: Arc<dyn HandlerExecutor>,
    ) -> Self {
        Self {
            store,
            ids,
            clock,
            adapter,
            executor,
            pending_outbox: Arc::new(NoPendingOutbox),
            queue_config: QueueConfig::default(),
            outbox_config: OutboxConfig::default(),
            failure_threshold: 3,
            drain_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

/// 一次性读出的 bot 状态快照。
#[derive(Clone, Debug)]
pub struct BotSnapshot {
    /// 当前状态。
    pub status: BotStatus,
    /// 已完成的启动步骤（按顺序）。
    pub startup_steps: Vec<StartupStep>,
    /// 所有会话未完成消息总数。
    pub queue_depth: usize,
    /// 当前连续 handler 失败次数。
    pub consecutive_failures: u32,
    /// 是否已降级。
    pub degraded: bool,
    /// 本次运行恢复的未发 outbound 条数。
    pub restored_outbound: usize,
    /// outbox 计数器（未启动时为 `None`）。
    pub outbox: Option<OutboxStats>,
}

/// `inject` 的结果。
#[derive(Clone, Debug)]
pub enum InjectOutcome {
    /// 首次收到并已入队。
    Queued {
        /// 幂等键（`inbound.id`）。
        message_id: MessageId,
    },
    /// 重复投递：已 ACK，不执行。
    Duplicate {
        /// 平台事件 ID。
        external_id: String,
    },
}

/// 读循环退出的原因。
///
/// `StopRequested` 是正常停机（`bot_stop` / 切项目 / 进程退出）；其余三种都是**意外**结束，
/// 共同后果是「bot 还登记在运行表里，但已经不会再收到任何新消息」。上层必须据此收尾
/// （`onStop` 的 `crash` 原因就是从这里来的），否则面板会一直显示 Running 而实际已瞎。
///
/// 为什么这个判断必须由 supervisor 给、而不是让上层去看队列深度：订阅被关掉、入队失败
/// 都不影响任何计数器，从外面看与"安静"完全一样（`AGENTS.md` §7.1 坑 24 同族）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadLoopExit {
    /// 收到停止信号（正常停机）。
    StopRequested,
    /// 适配器的订阅通道被关闭：平台侧连接没了，不会再有事件进来。
    SubscriptionClosed,
    /// 入站落库失败到无法继续（`admit_with_retry` 只在停止时返回，这里是兜底）。
    AdmitFailed {
        /// 原始错误文案。
        error: String,
    },
    /// 入队失败（`QueueError::Closed` 之外的错误）。
    EnqueueFailed {
        /// 原始错误文案。
        error: String,
    },
}

impl ReadLoopExit {
    /// 是不是意外结束（正常停机返回 `false`）。
    pub fn is_unexpected(&self) -> bool {
        !matches!(self, Self::StopRequested)
    }

    /// 一行可读的原因：日志、`onStop` 的记录与测试断言都用它。
    pub fn describe(&self) -> String {
        match self {
            Self::StopRequested => "read loop stopped by request".to_owned(),
            Self::SubscriptionClosed => "adapter subscription closed".to_owned(),
            Self::AdmitFailed { error } => format!("persisting inbound failed: {error}"),
            Self::EnqueueFailed { error } => format!("enqueue failed: {error}"),
        }
    }
}

/// 读循环结束后留下的退出原因（只写一次，`notify_one` 唤醒等待者）。
///
/// 用 `Notify` 而不是再开一条通道：等待者只有一个（应用层的崩溃收尾任务），
/// 而 `notify_one` 会留下一个 permit，所以"先结束、后等待"也不会漏。
#[derive(Default)]
struct ReadExitSlot {
    exit: Mutex<Option<ReadLoopExit>>,
    notify: Notify,
}

/// 一次运行期的资源集合（stop 时整体拆掉）。
struct Runtime {
    inbox: Arc<Inbox>,
    queues: Arc<SessionQueues>,
    outbox: Arc<Outbox>,
    stop_tx: watch::Sender<bool>,
    read_task: JoinHandle<ReadLoopExit>,
    watcher_task: JoinHandle<()>,
}

struct Inner {
    status: BotStatus,
    runtime: Option<Runtime>,
    steps: Vec<StartupStep>,
    subscribed: u32,
    restored: usize,
}

impl Inner {
    fn new() -> Self {
        Self {
            status: BotStatus::Stopped,
            runtime: None,
            steps: Vec::new(),
            subscribed: 0,
            restored: 0,
        }
    }
}

/// 每个 bot 一个 supervisor。
pub struct Supervisor {
    spec: BotSpec,
    deps: Arc<SupervisorDeps>,
    /// `start`/`stop` 互斥：并发调用按状态机报 `Conflict`，不会交叉执行。
    lifecycle: AsyncMutex<()>,
    inner: Arc<Mutex<Inner>>,
    failures: Arc<FailureTracker>,
    /// 读循环的退出原因（写一次，见 [`ReadExitSlot`]）。
    read_exit: Arc<ReadExitSlot>,
}

impl Supervisor {
    /// 构造 supervisor（不启动）。
    pub fn new(spec: BotSpec, deps: SupervisorDeps) -> Self {
        let failures = Arc::new(FailureTracker::new(deps.failure_threshold));
        Self {
            spec,
            deps: Arc::new(deps),
            lifecycle: AsyncMutex::new(()),
            inner: Arc::new(Mutex::new(Inner::new())),
            failures,
            read_exit: Arc::new(ReadExitSlot::default()),
        }
    }

    /// 这个 supervisor 管理的 bot ID。
    pub fn bot_id(&self) -> BotId {
        self.spec.bot_id
    }

    /// 当前状态（无锁读，供 UI 轮询）。
    pub fn status(&self) -> BotStatus {
        self.lock_inner().status
    }

    /// 当前快照。
    pub fn snapshot(&self) -> BotSnapshot {
        let inner = self.lock_inner();
        BotSnapshot {
            status: inner.status,
            startup_steps: inner.steps.clone(),
            queue_depth: inner
                .runtime
                .as_ref()
                .map_or(0, |runtime| runtime.queues.total_depth()),
            consecutive_failures: self.failures.consecutive(),
            degraded: self.failures.is_degraded(),
            restored_outbound: inner.restored,
            outbox: inner.runtime.as_ref().map(|runtime| runtime.outbox.stats()),
        }
    }

    /// 所有会话的未完成消息数（UI 显示用）。
    pub fn queue_depth(&self) -> usize {
        self.lock_inner()
            .runtime
            .as_ref()
            .map_or(0, |runtime| runtime.queues.total_depth())
    }

    /// 等读循环结束，并取回退出原因（只返回一次）。
    ///
    /// 应用层用它区分「这个 bot 是正常停的」还是「它自己瞎了」：`StopRequested` 是正常
    /// 停机，其余值说明读循环意外死亡。**意外结束时 supervisor 已经把状态改成
    /// `Degraded`**，但收尾（`onStop(crash)`、注销 webhook、取消作业）只能在应用层做，
    /// 所以这里只如实报告，不自己拆资源。
    ///
    /// 只返回一次：第二次调用会一直等到下一次读循环结束（同一个 supervisor 重启后才会
    /// 有第二个读循环），所以调用方应当只起一个等待者。
    pub async fn wait_read_loop_exit(&self) -> ReadLoopExit {
        loop {
            if let Some(exit) = self
                .read_exit
                .exit
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return exit;
            }
            // `notify_one` 会留 permit：读循环先结束、这个等待者后到也不会漏。
            self.read_exit.notify.notified().await;
        }
    }

    /// 启动 bot：按 [`StartupStep`] 顺序建资源，成功后才进入 `Running`。
    pub async fn start(&self) -> Result<(), HbotError> {
        let _guard = self.lifecycle.lock().await;
        let current = self.status();
        if current != BotStatus::Stopped {
            return Err(HbotError::conflict(format!(
                "cannot start bot {} from {current:?}",
                self.spec.bot_id
            )));
        }
        // 显式重启解除降级（“不自动重启”指的是崩溃后不自动拉起）。
        self.failures.reset();
        {
            let mut inner = self.lock_inner();
            inner.status = BotStatus::Starting;
            inner.steps.clear();
            inner.subscribed = 0;
            inner.restored = 0;
        }
        match self.start_inner().await {
            Ok(runtime) => {
                let mut inner = self.lock_inner();
                inner.runtime = Some(runtime);
                inner.status = BotStatus::Running;
                drop(inner);
                tracing::info!(bot = %self.spec.bot_id, "bot running");
                Ok(())
            }
            Err(error) => {
                self.lock_inner().status = BotStatus::Stopped;
                tracing::error!(
                    bot = %self.spec.bot_id,
                    %error,
                    "bot start failed; rolled back to stopped"
                );
                Err(error)
            }
        }
    }

    /// 停止 bot：停止读 → 排空 → 取消 worker → flush outbox → 标记 Stopped。
    pub async fn stop(&self) -> Result<(), HbotError> {
        let _guard = self.lifecycle.lock().await;
        let runtime = {
            let mut inner = self.lock_inner();
            match inner.status {
                BotStatus::Stopped => return Ok(()),
                BotStatus::Starting | BotStatus::Stopping => {
                    return Err(HbotError::conflict(format!(
                        "cannot stop bot {} from {:?}",
                        self.spec.bot_id, inner.status
                    )));
                }
                BotStatus::Running | BotStatus::Degraded => {}
            }
            inner.status = BotStatus::Stopping;
            inner.runtime.take()
        };
        let Some(runtime) = runtime else {
            self.lock_inner().status = BotStatus::Stopped;
            return Ok(());
        };

        // 1. 停止读取：read_loop 在下次 select 退出；在途消息已落库。
        let _ = runtime.stop_tx.send(true);
        let mut first_error: Option<HbotError> = None;
        let read_joined = tokio::time::timeout(self.deps.shutdown_timeout, runtime.read_task).await;
        if read_joined.is_err() {
            tracing::warn!(bot = %self.spec.bot_id, "read loop did not stop in time");
            first_error = Some(HbotError::timeout(
                "stop reading adapter events",
                millis_u64(self.deps.shutdown_timeout),
            ));
        }

        // 2. 等队列排空（超时继续，不能卡在 Stopping）。
        if let Err(error) = runtime.queues.wait_drained(self.deps.drain_timeout).await {
            tracing::warn!(bot = %self.spec.bot_id, %error, "drain timed out; cancelling workers");
            first_error.get_or_insert(error);
        }
        // 3. 取消 worker。
        if let Err(error) = runtime.queues.shutdown(self.deps.shutdown_timeout).await {
            tracing::warn!(bot = %self.spec.bot_id, %error, "session workers cancelled after timeout");
            first_error.get_or_insert(error);
        }
        // 4. flush outbox（未发出的保持 pending，下次启动续发）。
        if let Err(error) = runtime.outbox.stop(self.deps.shutdown_timeout).await {
            tracing::warn!(bot = %self.spec.bot_id, %error, "outbox flush timed out");
            first_error.get_or_insert(error);
        }
        // 降级监视任务在停止后没有意义，直接取消。
        runtime.watcher_task.abort();

        // 5. 标记 stopped（无论上面是否报错）。
        self.lock_inner().status = BotStatus::Stopped;
        tracing::info!(bot = %self.spec.bot_id, "bot stopped");
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 直接注入一条归一化事件（RPC / 测试用），走与适配器完全相同的链路。
    ///
    /// 队列满时返回 [`HbotError::Timeout`]（背压），**消息已落库**：调用方退避后可用同一
    /// `external_id` 重试，届时会得到 [`InjectOutcome::Duplicate`]。
    pub async fn inject(&self, event: PlatformEvent) -> Result<InjectOutcome, HbotError> {
        let (inbox, queues) = {
            let inner = self.lock_inner();
            match (&inner.status, &inner.runtime) {
                (BotStatus::Running, Some(runtime)) => {
                    (Arc::clone(&runtime.inbox), Arc::clone(&runtime.queues))
                }
                (status, _) => {
                    return Err(HbotError::conflict(format!(
                        "bot {} is {status:?}; inject requires Running",
                        self.spec.bot_id
                    )));
                }
            }
        };
        let admission = inbox.admit(&self.spec.bot_id, &event).await?;
        match admission {
            Admission::Duplicate { external_id } => {
                tracing::debug!(bot = %self.spec.bot_id, %external_id, "duplicate delivery acknowledged");
                Ok(InjectOutcome::Duplicate { external_id })
            }
            Admission::New(message) => {
                let message_id = message.id;
                let ctx = MessageCtx::new(self.spec.bot_id, *message, event.conversation);
                match queues.try_enqueue(ctx) {
                    Ok(()) => Ok(InjectOutcome::Queued { message_id }),
                    Err(error) => Err(error.into_hbot_error()),
                }
            }
        }
    }

    /// 入队一条出站消息（先落库，再交给 outbox 串行发送）。
    pub async fn send(&self, request: OutboundRequest) -> Result<hbot_core::OutboundId, HbotError> {
        let outbox = {
            let inner = self.lock_inner();
            match (&inner.status, &inner.runtime) {
                (BotStatus::Running | BotStatus::Degraded, Some(runtime)) => {
                    Arc::clone(&runtime.outbox)
                }
                (status, _) => {
                    return Err(HbotError::conflict(format!(
                        "bot {} is {status:?}; send requires a running bot",
                        self.spec.bot_id
                    )));
                }
            }
        };
        outbox.enqueue(request).await
    }

    async fn start_inner(&self) -> Result<Runtime, HbotError> {
        let bot_id = self.spec.bot_id;

        // 1. bot 行（后面的 conversations/inbound/outbound 都靠它过外键）。
        self.ensure_bot_row().await?;
        self.record_step(StartupStep::BotRow);

        let inbox = Arc::new(Inbox::new(
            Arc::clone(&self.deps.store),
            Arc::clone(&self.deps.ids),
            Arc::clone(&self.deps.clock),
        ));

        // 2. conversations 行。
        let declared: Vec<ConversationRef> = self.deps.adapter.declared_conversations();
        for conversation in &declared {
            inbox.ensure_conversation(&bot_id, conversation).await?;
        }
        tracing::debug!(
            bot = %bot_id,
            conversations = declared.len(),
            "declared conversations ensured"
        );
        self.record_step(StartupStep::Conversations);

        // 3. 订阅（此刻队列还没建，但事件先进订阅通道；下一步就绪后才开始读）。
        let subscription = self.deps.adapter.subscribe().await?;
        {
            let mut inner = self.lock_inner();
            inner.subscribed = inner.subscribed.saturating_add(1);
        }
        self.record_step(StartupStep::Subscribe);

        // 4. 恢复未发 outbox。先起 outbox worker，再把 pending 交回去。
        let outbox = Arc::new(Outbox::spawn(
            Arc::clone(&self.deps.store),
            Arc::clone(&self.deps.ids),
            Arc::clone(&self.deps.clock),
            Arc::clone(&self.deps.adapter),
            bot_id,
            self.deps.outbox_config.clone(),
        ));
        let pending = {
            let source = Arc::clone(&self.deps.pending_outbox);
            let bot_id = self.spec.bot_id;
            match tokio::task::spawn_blocking(move || source.pending(&bot_id)).await {
                Ok(Ok(pending)) => pending,
                Ok(Err(error)) => {
                    let _ = outbox.stop(self.deps.shutdown_timeout).await;
                    return Err(error);
                }
                Err(join_error) => {
                    let _ = outbox.stop(self.deps.shutdown_timeout).await;
                    return Err(HbotError::internal(format!(
                        "pending outbox lookup failed: {join_error}"
                    )));
                }
            }
        };
        let restored = match outbox.recover(pending).await {
            Ok(restored) => restored,
            Err(error) => {
                let _ = outbox.stop(self.deps.shutdown_timeout).await;
                return Err(error);
            }
        };
        self.lock_inner().restored = restored;
        self.record_step(StartupStep::RecoverOutbox);

        // 5. 队列 + 读循环。handler 链路：executor → mark_processed。
        let handler: Arc<dyn HandlerExecutor> = Arc::new(RecordingHandler {
            executor: Arc::clone(&self.deps.executor),
            inbox: Arc::clone(&inbox),
        });
        let queues = Arc::new(SessionQueues::new(
            self.deps.queue_config.clone(),
            handler,
            Arc::clone(&self.failures),
        ));
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let read_task = tokio::spawn(read_loop(
            bot_id,
            subscription,
            Arc::clone(&inbox),
            Arc::clone(&queues),
            stop_rx.clone(),
            Arc::clone(&self.inner),
            Arc::clone(&self.read_exit),
        ));
        let watcher_task = tokio::spawn(degraded_watch(
            bot_id,
            Arc::clone(&self.failures),
            Arc::clone(&self.inner),
            stop_tx.clone(),
            self.deps.failure_threshold,
        ));

        // 5. 补偿上次运行遗留的未处理入站消息。必须在 `Reading` 之前、队列就绪之后：
        //    队列没就绪无处入队；在 `Reading` 之后则新事件会插到旧消息前面。
        let replayed = self
            .replay_unprocessed_inbound(&queues, &mut stop_rx)
            .await?;
        if replayed > 0 {
            tracing::info!(
                bot = %bot_id,
                replayed,
                "replayed unprocessed inbound messages after restart"
            );
        }
        self.record_step(StartupStep::ReplayInbound);
        self.record_step(StartupStep::Reading);

        Ok(Runtime {
            inbox,
            queues,
            outbox,
            stop_tx,
            read_task,
            watcher_task,
        })
    }

    /// 启动补偿：把上次运行遗留的未处理入站行（`processed_at IS NULL`）重新入队。
    ///
    /// ## 为什么必须补偿
    ///
    /// `AGENTS.md` §4.1 的顺序是“先落库再处理”，所以 `inbound` 行可能就是唯一证据：
    /// 落库之后、handler 执行之前进程被 kill / 崩溃 / 因为 [`QueueError::Closed`] 丢掉入队，
    /// 那行永远停在 `processed_at IS NULL`。不补偿就是**静默丢消息**（这正是
    /// `read_loop` 里那句“下次启动会重放”承诺的东西）。
    ///
    /// ## 走的是同一条路
    ///
    /// 每行都构造成 [`MessageCtx`] 后交给 [`SessionQueues::enqueue`]，与实时流量完全一致：
    /// 同样受每会话串行、吞吐上限与背压约束，同样经 `RecordingHandler` 写 `processed_at`
    /// （所以在 handler 成功后才会被标记，中途崩溃会再次补偿）。
    ///
    /// ## 语义：at-least-once（`docs/02-architecture.md` §5.1）
    ///
    /// 这是有意接受的成本，不是缺陷：如果进程在“handler 执行完、写 `processed_at` 之前”
    /// 崩溃，重启后这条消息会再执行一次，**生成代码可能重复回复**。要换成 exactly-once
    /// 就得把标记与副作用放进同一个事务，而副作用在平台侧（QQ），做不到。
    ///
    /// ## 分批
    ///
    /// 按 `(received_at, rowid)` keyset 分页，每批 [`REPLAY_BATCH_SIZE`] 条；游标前进后
    /// 继续，直到空批或不足一批。不用一次取全量：崩溃时积压可能有几十万行，一次取回
    /// 会把整个结果集压在内存里（同 `AGENTS.md` §5 “通道/批处理一律有界”）。
    async fn replay_unprocessed_inbound(
        &self,
        queues: &Arc<SessionQueues>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<usize, HbotError> {
        let bot_id = self.spec.bot_id;
        let mut cursor: Option<(i64, i64)> = None;
        let mut replayed = 0usize;
        // 一次启动内同一条消息只入队一次：批边界可能让同一行出现两次（游标在
        // `received_at` 相同的行之间靠 rowid，理论上不会，但这里不依赖那个推理）。
        let mut seen: HashSet<MessageId> = HashSet::new();

        loop {
            if *stop.borrow() {
                // 启动途中被要求停止（例如 handler 已经连续失败进入 Degraded）。
                break;
            }
            let store = Arc::clone(&self.deps.store);
            let batch = run_store("list_inbound_replay", move || {
                store.list_inbound_replay_for_bot(&bot_id, cursor, REPLAY_BATCH_SIZE)
            })
            .await?;
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();
            for row in batch {
                let message_id = MessageId::from_uuid(*row.message.id.as_uuid());
                cursor = Some((row.message.received_at, row.rowid));
                if !seen.insert(message_id) {
                    tracing::warn!(
                        bot = %bot_id,
                        message = %message_id,
                        "replay 游标重复覆盖同一行：跳过，避免一次启动内重复执行"
                    );
                    continue;
                }
                // payload 由我们自己 `serde_json::to_string` 写入，解析失败说明内存/HBOT 版本
                // 不一致。这里**不**中断启动、也**不**标记已处理：保持可见（下轮启动还会再试），
                // 只记一条错误日志，避免一行坏数据把整个 bot 钉死在 StartFailed 上。
                let payload = match serde_json::from_str::<Value>(&row.message.payload_json) {
                    Ok(payload) => payload,
                    Err(error) => {
                        tracing::error!(
                            bot = %bot_id,
                            message = %message_id,
                            error = %error,
                            "replay: inbound payload 不是合法 JSON，跳过（保持未处理）"
                        );
                        continue;
                    }
                };
                let message = InboundMessage {
                    id: message_id,
                    bot_id,
                    conversation_id: row.message.conversation_id,
                    external_id: row.message.external_id.clone(),
                    sender_id: row.message.sender_id.clone(),
                    payload,
                    received_at: Timestamp::from_unix_millis(row.message.received_at),
                    processed_at: None,
                };
                let conversation = ConversationRef::new(
                    row.conversation_external_id.clone(),
                    conversation_kind_of(&row.conversation_kind),
                );
                let ctx = MessageCtx::new(bot_id, message, conversation);
                match queues.enqueue(ctx, stop).await {
                    Ok(()) => replayed += 1,
                    // 停止中：行仍是未处理态，下次启动继续补偿。
                    Err(QueueError::Closed { .. }) => return Ok(replayed),
                    Err(error) => return Err(error.into_hbot_error()),
                }
            }
            if batch_len < REPLAY_BATCH_SIZE as usize {
                break;
            }
        }

        if replayed > 0 {
            // 一条可观测记录：重启后补偿了未处理消息说明上次是非正常退出，值得让人看见
            // （`AGENTS.md` §5：不许静默吞）。
            let store = Arc::clone(&self.deps.store);
            let at = self.deps.clock.now().unix_millis();
            let line = format!(
                "重启补偿：重新入队 {replayed} 条未处理入站消息（at-least-once，可能重复回复）"
            );
            run_store("append_log", move || {
                store.append_log(NewLog {
                    bot_id,
                    level: "warn".to_owned(),
                    line,
                    at,
                })
            })
            .await?;
        }
        Ok(replayed)
    }

    async fn ensure_bot_row(&self) -> Result<(), HbotError> {
        let store = Arc::clone(&self.deps.store);
        let bot_id = self.spec.bot_id;
        let existing = run_store("get_bot", move || store.get_bot(&bot_id)).await?;
        if let Some(bot) = existing {
            if !bot.enabled {
                return Err(HbotError::denied(format!(
                    "bot {} is disabled in settings",
                    self.spec.bot_id
                )));
            }
            return Ok(());
        }
        let row = StoreBot {
            id: self.spec.bot_id,
            project_id: self.spec.project_id,
            name: self.spec.name.clone(),
            platform: self.spec.platform.clone(),
            config_json: self.spec.config_json.clone(),
            enabled: true,
            created_at: self.deps.clock.now().unix_millis(),
        };
        let store = Arc::clone(&self.deps.store);
        run_store("insert_bot", move || store.insert_bot(&row)).await?;
        Ok(())
    }

    fn record_step(&self, step: StartupStep) {
        let mut inner = self.lock_inner();
        inner.steps.push(step);
        tracing::debug!(
            bot = %self.spec.bot_id,
            step = step.as_str(),
            "supervisor startup step"
        );
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        // 临界区不含 await，poison 只会来自内部 panic；退化成内层数据，避免整个 bot 卡死。
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// handler 链路：先执行，再写 `processed_at`。
struct RecordingHandler {
    executor: Arc<dyn HandlerExecutor>,
    inbox: Arc<Inbox>,
}

#[async_trait]
impl HandlerExecutor for RecordingHandler {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        let message_id = ctx.message_id();
        self.executor.on_message(ctx).await?;
        match self.inbox.mark_processed(&message_id).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                // 行不见了（被人工清理/迁移）：这次执行确实发生过了，记录但不当作失败。
                tracing::warn!(
                    message = %message_id,
                    "processed_at not updated: inbound row missing"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// 读循环：订阅事件 → 去重落库 → 入队；队列满时暂停（背压），不丢消息。
///
/// 返回**退出原因**，并把它记进 `read_exit`：退出本身不是错误（正常停机也会退出），
/// 但「为什么退出」决定了上层要不要按 `crash` 收尾。意外结束时顺手把状态改成
/// `Degraded` —— 那一刻这个 bot 已经收不到任何新消息了，继续显示 `Running` 是撒谎。
async fn read_loop(
    bot_id: BotId,
    mut subscription: AdapterSubscription,
    inbox: Arc<Inbox>,
    queues: Arc<SessionQueues>,
    mut stop: watch::Receiver<bool>,
    inner: Arc<Mutex<Inner>>,
    read_exit: Arc<ReadExitSlot>,
) -> ReadLoopExit {
    let exit = loop {
        let event = tokio::select! {
            _ = stop.changed() => break ReadLoopExit::StopRequested,
            received = subscription.events.recv() => match received {
                Some(event) => event,
                None => {
                    tracing::warn!(bot = %bot_id, "adapter subscription closed; stopping read loop");
                    break ReadLoopExit::SubscriptionClosed;
                }
            },
        };
        let admission = match admit_with_retry(&bot_id, &inbox, &event, &mut stop).await {
            Ok(Some(admission)) => admission,
            Ok(None) => break ReadLoopExit::StopRequested,
            Err(error) => {
                tracing::error!(
                    bot = %bot_id,
                    %error,
                    "failed to persist inbound event; stopping read loop"
                );
                break ReadLoopExit::AdmitFailed {
                    error: error.to_string(),
                };
            }
        };
        match admission {
            Admission::Duplicate { external_id } => {
                tracing::debug!(bot = %bot_id, %external_id, "duplicate delivery skipped");
            }
            Admission::New(message) => {
                let message_id = message.id;
                let ctx = MessageCtx::new(bot_id, *message, event.conversation);
                match queues.enqueue(ctx, &mut stop).await {
                    Ok(()) => {}
                    Err(QueueError::Closed { .. }) => {
                        // 停止中：消息已落库，下次启动会重放。
                        tracing::debug!(bot = %bot_id, %message_id, "queue closed while stopping");
                        break ReadLoopExit::StopRequested;
                    }
                    Err(error) => {
                        tracing::error!(bot = %bot_id, %message_id, %error, "enqueue failed");
                        break ReadLoopExit::EnqueueFailed {
                            error: error.to_string(),
                        };
                    }
                }
            }
        }
    };

    tracing::debug!(bot = %bot_id, reason = %exit.describe(), "read loop stopped");
    if exit.is_unexpected() {
        // 只有「仍在跑」才降级：停机途中读循环退出是正常时序，别把 Stopping 盖成 Degraded。
        let mut guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(guard.status, BotStatus::Running) {
            guard.status = BotStatus::Degraded;
            tracing::error!(
                bot = %bot_id,
                reason = %exit.describe(),
                "read loop exited unexpectedly: this bot will not receive new events"
            );
        }
    }
    *read_exit
        .exit
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(exit.clone());
    read_exit.notify.notify_one();
    exit
}

/// 落库失败通常是暂时性的（SQLite busy/磁盘抖动）：退避重试直到成功或停止。
///
/// 不做“失败就丢”：事件已经离开适配器通道，丢掉就是真的丢了。
async fn admit_with_retry(
    bot_id: &BotId,
    inbox: &Inbox,
    event: &PlatformEvent,
    stop: &mut watch::Receiver<bool>,
) -> Result<Option<Admission>, HbotError> {
    let mut backoff = Duration::from_millis(50);
    let mut attempt: u32 = 0;
    loop {
        match inbox.admit(bot_id, event).await {
            Ok(admission) => return Ok(Some(admission)),
            Err(error) => {
                attempt += 1;
                tracing::warn!(
                    bot = %bot_id,
                    external_id = %event.external_id,
                    attempt,
                    %error,
                    "admit failed; retrying"
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.changed() => return Ok(None),
                }
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// 连续失败达阈值 → 状态 `Degraded` + 停止读取（不自动重启）。
async fn degraded_watch(
    bot_id: BotId,
    failures: Arc<FailureTracker>,
    inner: Arc<Mutex<Inner>>,
    stop_tx: watch::Sender<bool>,
    threshold: u32,
) {
    failures.wait_degraded().await;
    {
        let mut guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(guard.status, BotStatus::Running) {
            guard.status = BotStatus::Degraded;
            tracing::error!(
                bot = %bot_id,
                threshold,
                "bot degraded: consecutive handler failures; stopping event intake"
            );
        }
    }
    // 停止读取；已入队的仍会执行完，outbox 仍可发送。
    let _ = stop_tx.send(true);
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// 适配器事件的订阅容量（适配器写不进去就自己背压）。
pub const ADAPTER_EVENT_CAPACITY: usize = 256;

/// 启动补偿时每批从 `inbound` 取多少行。
///
/// 取 256 的理由：单条消息的 `payload_json` 通常几 KB，256 条的量级是 ~1MB 内存与一次
/// 批量的 SQL；同时它比常见的崩溃积压（几十条）大得多，正常情况一批就追平，
/// 只有真正的大积压才会走到第二轮。
pub const REPLAY_BATCH_SIZE: u32 = 256;

/// 把 `conversations.kind` 的存储字符串还原成领域枚举。
///
/// 取值与 `hbot-rpc` 的 `conversation_kind_from_str` 完全一致（`private` / `group` /
/// `channel`，其它一律 `Unknown`）。**刻意重复 6 行而不建共享函数**：`hbot-bot` 是比
/// `hbot-rpc` 更低的一层，不能反过来依赖 rpc；而 `hbot-adapters` 提供的映射会把
/// `AppState` 之外的 adapter 语义塞进 supervisor。存储侧的唯一真源是
/// `conversations.kind` 这一列本身（写入方是 `hbot-bot::ids` 与适配器）。
fn conversation_kind_of(raw: &str) -> ConversationKind {
    match raw {
        "private" => ConversationKind::Private,
        "group" => ConversationKind::Group,
        "channel" => ConversationKind::Channel,
        _ => ConversationKind::Unknown,
    }
}

/// 建一个订阅通道，返回（交给适配器的发送端, 交给 supervisor 的订阅句柄）。
///
/// 只是便捷函数：适配器实现可以直接用 [`AdapterSubscription::channel`]。
pub fn adapter_channel() -> (mpsc::Sender<PlatformEvent>, AdapterSubscription) {
    AdapterSubscription::channel(ADAPTER_EVENT_CAPACITY)
}
