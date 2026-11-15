//! 每会话有界队列 + 单 worker。
//!
//! ## 保证
//!
//! * **同会话严格 FIFO、串行执行**：每个 `(bot_id, conversation_id)` 一个
//!   [`tokio::sync::mpsc`] 通道 + 一个 worker 任务，worker 处理完上一条才取下一条，
//!   不存在“同会话两条消息并行”的可能（`AGENTS.md` §0.6）。
//! * **不同会话并行**：会话之间互不阻塞，全局同时执行的 handler 数量受
//!   [`QueueConfig::max_in_flight`]（默认 64）限制。
//! * **有界、不丢**：每会话最多 `session_capacity`（默认 1024）条**未完成**消息（含正在
//!   执行的那条）。超限返回 [`QueueError::Backpressure`]，调用方暂停读取后重试；消息
//!   此刻已经落库（见 [`crate::dedupe`]），所以等待期间不会丢。
//! * **可观测**：`depth()` 给 UI 显示积压。
//!
//! ## 计数口径
//!
//! `depth` 从成功入队开始计、到 handler 执行结束（无论成败）为止减一。因此它同时覆盖
//! “排队中”和“执行中”，UI 看到的就是“这个会话还有多少条没处理完”。背压判断也用同一
//! 个计数：这样即使 worker 已把消息取出、正在慢慢执行，队列仍然不会无限接收。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use hbot_core::{BotId, ConversationId, HbotError};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::handler::{HandlerExecutor, MessageCtx};

/// 会话键：一个 bot 下的一通平台会话。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// 所属 bot。
    pub bot_id: BotId,
    /// 本地会话 ID（由 `(bot_id, external_id)` 派生）。
    pub conversation_id: ConversationId,
}

impl SessionKey {
    /// 构造会话键。
    pub fn new(bot_id: BotId, conversation_id: ConversationId) -> Self {
        Self {
            bot_id,
            conversation_id,
        }
    }
}

/// 队列配置。
#[derive(Clone, Debug)]
pub struct QueueConfig {
    /// 每会话最多未完成消息数（含执行中的一条）；默认 1024。
    pub session_capacity: usize,
    /// 全局同时执行的 handler 上限；默认 64。
    pub max_in_flight: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            session_capacity: 1024,
            max_in_flight: 64,
        }
    }
}

impl QueueConfig {
    /// 把 0 修正成 1：容量为 0 的队列永远收不了消息，是配置错误而不是有界。
    fn sanitized(&self) -> Self {
        Self {
            session_capacity: self.session_capacity.max(1),
            max_in_flight: self.max_in_flight.max(1),
        }
    }
}

/// 入队失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// 会话队列已满（已落库，调用方应暂停读取并重试）。
    Backpressure {
        /// 哪个会话满了。
        session: SessionKey,
        /// 容量上限，便于日志与指标。
        capacity: usize,
    },
    /// 队列已关闭（bot 正在停止）：这条消息不再接受，但已落库。
    Closed {
        /// 哪个会话拒绝了。
        session: SessionKey,
    },
}

impl QueueError {
    /// 转成领域错误：背压用 [`HbotError::Timeout`]（调用方可以重试），关闭用 `Conflict`。
    pub fn into_hbot_error(self) -> HbotError {
        match self {
            Self::Backpressure { session, capacity } => HbotError::Timeout {
                operation: format!(
                    "enqueue session queue (bot {}, conversation {}) is full at {capacity}",
                    session.bot_id, session.conversation_id
                ),
                // 背压不是定时器超时，没有固定阈值；0 表示“由调用方退避重试”。
                timeout_ms: 0,
            },
            Self::Closed { session } => HbotError::Conflict {
                message: format!(
                    "session queue closed (bot {}, conversation {})",
                    session.bot_id, session.conversation_id
                ),
            },
        }
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backpressure { session, capacity } => write!(
                formatter,
                "session queue full at {capacity} (bot {}, conversation {})",
                session.bot_id, session.conversation_id
            ),
            Self::Closed { session } => write!(
                formatter,
                "session queue closed (bot {}, conversation {})",
                session.bot_id, session.conversation_id
            ),
        }
    }
}

impl std::error::Error for QueueError {}

/// 连续失败计数：连续 [`FailureTracker::threshold`] 次 handler 崩溃 → Degraded。
///
/// 成功一次即清零（“连续”才降级，偶发失败不降级）。降级后不自动恢复：需要显式
/// `stop` + `start`，避免崩溃循环（`docs/02-architecture.md` §12）。
pub struct FailureTracker {
    threshold: u32,
    consecutive: AtomicU32,
    degraded: AtomicBool,
    notify: Notify,
}

impl FailureTracker {
    /// 新建计数器；`threshold` 为 0 时按 1 处理。
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            consecutive: AtomicU32::new(0),
            degraded: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// 记录一次 handler 失败；返回 `true` 表示**本次**触发了降级。
    pub fn record_failure(&self) -> bool {
        let previous = self.consecutive.fetch_add(1, Ordering::AcqRel);
        if previous + 1 >= self.threshold && !self.degraded.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
            return true;
        }
        false
    }

    /// 记录一次成功：连续计数清零（已经降级的状态不因此恢复）。
    pub fn record_success(&self) {
        self.consecutive.store(0, Ordering::Release);
    }

    /// 复位：显式 `start` 时调用（“不自动重启”指的是崩溃后不自动拉起，人工重启应解除降级）。
    pub fn reset(&self) {
        self.consecutive.store(0, Ordering::Release);
        self.degraded.store(false, Ordering::Release);
    }

    /// 当前连续失败次数。
    pub fn consecutive(&self) -> u32 {
        self.consecutive.load(Ordering::Acquire)
    }

    /// 降级阈值。
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// 是否已降级。
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    /// 等待降级发生（没有丢唤醒：先注册监听再复查标志）。
    pub async fn wait_degraded(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // `enable()` 之后再检查标志，避免“检查完、notify 已发、自己才开始等”的丢失唤醒。
            notified.as_mut().enable();
            if self.is_degraded() {
                return;
            }
            notified.await;
        }
    }
}

/// 队列共享状态：worker 与 [`SessionQueues`] 共用。
struct QueueShared {
    handler: Arc<dyn HandlerExecutor>,
    failures: Arc<FailureTracker>,
    session_capacity: usize,
    /// 全 bot 未完成消息数（排队 + 执行中）。
    total: AtomicUsize,
    /// 每完成一条就唤醒等待容量/排空的调用方。
    progress: Notify,
    /// 全局并发闸门。
    in_flight: Semaphore,
}

struct Session {
    tx: mpsc::Sender<MessageCtx>,
    depth: Arc<AtomicUsize>,
    worker: JoinHandle<()>,
}

impl Session {
    /// 预留一个未完成名额（CAS 循环，不超容量）。
    fn reserve(&self, capacity: usize) -> bool {
        let mut current = self.depth.load(Ordering::Acquire);
        loop {
            if current >= capacity {
                return false;
            }
            match self.depth.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release(&self) {
        self.depth.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 每会话有界队列集合。
pub struct SessionQueues {
    shared: Arc<QueueShared>,
    sessions: Mutex<HashMap<SessionKey, Session>>,
    closing: AtomicBool,
}

impl SessionQueues {
    /// 新建队列集合；worker 在首次入队时按需创建（没有消息的会话不占任务）。
    pub fn new(
        config: QueueConfig,
        handler: Arc<dyn HandlerExecutor>,
        failures: Arc<FailureTracker>,
    ) -> Self {
        let config = config.sanitized();
        Self {
            shared: Arc::new(QueueShared {
                handler,
                failures,
                session_capacity: config.session_capacity,
                total: AtomicUsize::new(0),
                progress: Notify::new(),
                in_flight: Semaphore::new(config.max_in_flight),
            }),
            sessions: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
        }
    }

    /// 每会话容量。
    pub fn session_capacity(&self) -> usize {
        self.shared.session_capacity
    }

    /// 某个会话的未完成消息数（UI 显示用）。
    pub fn depth(&self, key: &SessionKey) -> usize {
        self.lock()
            .get(key)
            .map(|session| session.depth.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// 所有会话的未完成消息数。
    pub fn total_depth(&self) -> usize {
        self.shared.total.load(Ordering::Acquire)
    }

    /// 尝试入队，不等待（供 `inject` / RPC 用；背压直接上报给调用方）。
    pub fn try_enqueue(&self, ctx: MessageCtx) -> Result<(), QueueError> {
        let key = SessionKey::new(ctx.bot_id, ctx.conversation_id());
        let mut sessions = self.lock();
        if self.closing.load(Ordering::Acquire) {
            return Err(QueueError::Closed { session: key });
        }
        if let Entry::Vacant(slot) = sessions.entry(key) {
            slot.insert(self.spawn_session(key));
        }
        let Some(session) = sessions.get_mut(&key) else {
            // 上面刚插入过，走到这里只可能是内部 bug；按关闭上报，不 panic。
            return Err(QueueError::Closed { session: key });
        };
        if !session.reserve(self.shared.session_capacity) {
            return Err(QueueError::Backpressure {
                session: key,
                capacity: self.shared.session_capacity,
            });
        }
        self.shared.total.fetch_add(1, Ordering::AcqRel);
        match session.tx.try_send(ctx) {
            Ok(()) => Ok(()),
            // 容量已经由 `reserve` 保证，走到这里说明计数与通道不一致：按背压处理并回滚计数，
            // 不丢消息（消息已落库，调用方会重试）。
            Err(TrySendError::Full(_)) => {
                session.release();
                self.shared.total.fetch_sub(1, Ordering::AcqRel);
                self.shared.progress.notify_waiters();
                Err(QueueError::Backpressure {
                    session: key,
                    capacity: self.shared.session_capacity,
                })
            }
            Err(TrySendError::Closed(_)) => {
                session.release();
                self.shared.total.fetch_sub(1, Ordering::AcqRel);
                self.shared.progress.notify_waiters();
                Err(QueueError::Closed { session: key })
            }
        }
    }

    /// 入队，队列满时等待（**调用方暂停读取**的背压语义）。
    ///
    /// `stop` 触发时立即返回 [`QueueError::Closed`]：消息已经落库，下次启动会重放，
    /// 不会因为关停而丢失。
    pub async fn enqueue(
        &self,
        ctx: MessageCtx,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), QueueError> {
        loop {
            let key = SessionKey::new(ctx.bot_id, ctx.conversation_id());
            let notified = self.shared.progress.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_enqueue(ctx.clone()) {
                Ok(()) => return Ok(()),
                Err(QueueError::Backpressure { .. }) => {
                    tokio::select! {
                        _ = &mut notified => continue,
                        changed = stop.changed() => {
                            // 发送端被丢弃（supervisor 已退出）也当作停止。
                            let _ = changed;
                            return Err(QueueError::Closed { session: key });
                        }
                    }
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// 等待所有会话排空（不关闭；关停顺序是“排空 → 取消 worker”）。
    pub async fn wait_drained(&self, timeout: Duration) -> Result<(), HbotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.shared.progress.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.total_depth() == 0 {
                return Ok(());
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(HbotError::timeout(
                        format!("drain session queues ({} still outstanding)", self.total_depth()),
                        millis_u64(timeout),
                    ));
                }
            }
        }
    }

    /// 关闭所有会话 worker：不再接受新消息，等 worker 退出；超时则 abort 掉。
    ///
    /// 调用前应先 [`Self::wait_drained`]；这里仍然处理“到点还没停”的情况，避免 stop 挂死。
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), HbotError> {
        self.closing.store(true, Ordering::Release);
        let sessions: Vec<Session> = {
            let mut guard = self.lock();
            guard.drain().map(|(_key, session)| session).collect()
        };
        let (senders, mut workers): (Vec<_>, Vec<_>) =
            sessions.into_iter().map(|s| (s.tx, s.worker)).unzip();
        drop(senders);
        let outcome = tokio::time::timeout(timeout, async {
            for worker in workers.iter_mut() {
                let _ = worker.await;
            }
        })
        .await;
        if outcome.is_err() {
            for worker in &workers {
                worker.abort();
            }
            return Err(HbotError::timeout(
                "cancel session workers",
                millis_u64(timeout),
            ));
        }
        Ok(())
    }

    fn spawn_session(&self, key: SessionKey) -> Session {
        let capacity = self.shared.session_capacity;
        let (tx, rx) = mpsc::channel::<MessageCtx>(capacity);
        let depth = Arc::new(AtomicUsize::new(0));
        let shared = Arc::clone(&self.shared);
        let worker_depth = Arc::clone(&depth);
        let worker = tokio::spawn(async move {
            run_worker(shared, key, rx, worker_depth).await;
        });
        Session { tx, depth, worker }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Session>> {
        // 持锁期间不做任何 await / 阻塞 IO，poison 只可能来自这里的 panic；退化成
        // 内层数据继续用，不因为一个 panic 让整个 bot 不可用。
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 单会话 worker：串行执行到通道关闭。
async fn run_worker(
    shared: Arc<QueueShared>,
    key: SessionKey,
    mut rx: mpsc::Receiver<MessageCtx>,
    depth: Arc<AtomicUsize>,
) {
    while let Some(ctx) = rx.recv().await {
        let permit = match shared.in_flight.acquire().await {
            Ok(permit) => permit,
            // 信号量只会在关闭时出错；本 crate 不关闭它，走到这里直接退出并告警。
            Err(error) => {
                tracing::warn!(
                    bot = %key.bot_id,
                    conversation = %key.conversation_id,
                    %error,
                    "global in-flight semaphore closed; stopping session worker"
                );
                break;
            }
        };
        let outcome = shared.handler.on_message(ctx).await;
        match &outcome {
            Ok(()) => shared.failures.record_success(),
            Err(error) => {
                let degraded = shared.failures.record_failure();
                tracing::warn!(
                    bot = %key.bot_id,
                    conversation = %key.conversation_id,
                    consecutive_failures = shared.failures.consecutive(),
                    %error,
                    "handler failed"
                );
                if degraded {
                    tracing::error!(
                        bot = %key.bot_id,
                        threshold = shared.failures.threshold(),
                        "bot degraded: consecutive handler failures"
                    );
                }
            }
        }
        drop(permit);
        depth.fetch_sub(1, Ordering::AcqRel);
        if shared.total.fetch_sub(1, Ordering::AcqRel) == 1 {
            shared.progress.notify_waiters();
        }
    }
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use hbot_core::{
        BotId, ConversationId, ConversationKind, HbotError, InboundMessage, MessageId, Timestamp,
    };
    use tokio::sync::{Mutex, Semaphore, watch};
    use uuid::Uuid;

    use super::{FailureTracker, QueueConfig, QueueError, SessionKey, SessionQueues};
    use crate::event::ConversationRef;
    use crate::handler::{HandlerExecutor, MessageCtx};

    fn ctx(bot: u128, conversation: u128, seq: u64) -> MessageCtx {
        let bot_id = BotId::from_uuid(Uuid::from_u128(bot));
        MessageCtx::new(
            bot_id,
            InboundMessage {
                id: MessageId::from_uuid(Uuid::from_u128(u128::from(seq) + 1)),
                bot_id,
                conversation_id: ConversationId::from_uuid(Uuid::from_u128(conversation)),
                external_id: format!("evt-{seq}"),
                sender_id: "user".to_owned(),
                payload: serde_json::json!({ "text": seq.to_string() }),
                received_at: Timestamp::from_unix_millis(0),
                processed_at: None,
            },
            ConversationRef::new("c", ConversationKind::Group),
        )
    }

    /// 记录执行顺序；可选“闸门”让 handler 停住以制造积压。
    ///
    /// 闸门用零许可的 [`Semaphore`] 而不是 `Notify`：`Notify::notify_waiters` 只唤醒**当时**
    /// 已在等待的任务，同会话串行时后来的 handler 会永久卡住。信号量是状态而不是事件，
    /// 放行的许可对之后才到达的 handler 同样有效。
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
        gate: Mutex<Option<Arc<Semaphore>>>,
        running: Mutex<usize>,
        max_running: Mutex<usize>,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl HandlerExecutor for Recorder {
        async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
            {
                let mut running = self.running.lock().await;
                *running += 1;
                let mut max_running = self.max_running.lock().await;
                *max_running = (*max_running).max(*running);
            }
            if let Some(gate) = self.gate.lock().await.clone() {
                // 许可即放行：拿到就立即丢弃，不做限流。
                let _permit = gate.acquire().await;
            }
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                return Err(HbotError::internal("boom"));
            }
            self.seen.lock().await.push(ctx.external_id().to_owned());
            let mut running = self.running.lock().await;
            *running -= 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn same_session_is_fifo() {
        let recorder = Arc::new(Recorder::default());
        let queues = SessionQueues::new(
            QueueConfig::default(),
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        );
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        let key = SessionKey::new(
            BotId::from_uuid(Uuid::from_u128(2)),
            ConversationId::from_uuid(Uuid::from_u128(3)),
        );
        for seq in 0..50_u64 {
            queues
                .enqueue(ctx(2, 3, seq), &mut stop_rx)
                .await
                .expect("enqueued");
        }
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        let seen = recorder.seen.lock().await.clone();
        let expected: Vec<String> = (0..50_u64).map(|seq| format!("evt-{seq}")).collect();
        assert_eq!(seen, expected);
        assert_eq!(queues.depth(&key), 0);
    }

    #[tokio::test]
    async fn different_sessions_run_in_parallel() {
        let recorder = Arc::new(Recorder::default());
        let gate = Arc::new(Semaphore::new(0));
        *recorder.gate.lock().await = Some(Arc::clone(&gate));
        let queues = SessionQueues::new(
            QueueConfig::default(),
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        );
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        queues
            .enqueue(ctx(2, 3, 1), &mut stop_rx)
            .await
            .expect("enqueued a");
        queues
            .enqueue(ctx(2, 4, 2), &mut stop_rx)
            .await
            .expect("enqueued b");
        // 深度只说明“入了队”，必须等两个 worker 真的同时卡在 handler 里。
        for _ in 0..2_000 {
            if *recorder.running.lock().await == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let running = *recorder.running.lock().await;
        assert_eq!(running, 2, "two sessions must execute concurrently");
        gate.add_permits(2);
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        let max_running = *recorder.max_running.lock().await;
        assert_eq!(max_running, 2, "both sessions must overlap");
    }

    #[tokio::test]
    async fn full_queue_reports_backpressure() {
        let recorder = Arc::new(Recorder::default());
        let gate = Arc::new(Semaphore::new(0));
        *recorder.gate.lock().await = Some(Arc::clone(&gate));
        let queues = SessionQueues::new(
            QueueConfig {
                session_capacity: 2,
                max_in_flight: 4,
            },
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        );
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        queues
            .enqueue(ctx(2, 3, 1), &mut stop_rx)
            .await
            .expect("first");
        queues
            .enqueue(ctx(2, 3, 2), &mut stop_rx)
            .await
            .expect("second");
        let error = queues.try_enqueue(ctx(2, 3, 3)).expect_err("full");
        assert!(matches!(
            error,
            QueueError::Backpressure { capacity: 2, .. }
        ));
        assert_eq!(
            queues.total_depth(),
            2,
            "rejected message must not be counted"
        );
        gate.add_permits(2);
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        assert_eq!(recorder.seen.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn consecutive_failures_degrade_once() {
        let tracker = FailureTracker::new(3);
        assert!(!tracker.record_failure());
        assert!(!tracker.record_failure());
        assert!(
            tracker.record_failure(),
            "third consecutive failure degrades"
        );
        assert!(
            !tracker.record_failure(),
            "already degraded, not a new transition"
        );
        assert!(tracker.is_degraded());
        tracker.record_success();
        assert!(tracker.is_degraded(), "degraded is sticky");
    }

    #[tokio::test]
    async fn shutdown_rejects_new_messages() {
        let recorder = Arc::new(Recorder::default());
        let queues = SessionQueues::new(
            QueueConfig::default(),
            recorder,
            Arc::new(FailureTracker::new(3)),
        );
        queues
            .shutdown(Duration::from_secs(1))
            .await
            .expect("shutdown");
        let error = queues.try_enqueue(ctx(2, 3, 1)).expect_err("closed");
        assert!(matches!(error, QueueError::Closed { .. }));
    }
}
