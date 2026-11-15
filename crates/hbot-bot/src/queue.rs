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
//! * **空闲会话可回收（D7）**：会话 worker 在首次入队时创建，`depth == 0` 且超过
//!   [`DEFAULT_SESSION_IDLE_TTL`] 未活动的会话会在下一次入队的扫描里被移除（连同通道，
//!   worker 随之自然退出），下次同会话入队时重建。**只能在 `depth == 0` 时回收**，且
//!   “扫描 + 移除”与 `try_enqueue` 的“插入 + 发送”共用同一把 `sessions` 锁，所以不会出现
//!   “worker 已退出、消息塞进旧通道”的静默丢消息。
//!
//! ## 计数口径
//!
//! `depth` 从成功入队开始计、到 handler 执行结束（无论成败）为止减一。因此它同时覆盖
//! “排队中”和“执行中”，UI 看到的就是“这个会话还有多少条没处理完”。背压判断也用同一
//! 个计数：这样即使 worker 已把消息取出、正在慢慢执行，队列仍然不会无限接收。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hbot_core::{BotId, ConversationId, HbotError};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::handler::{HandlerExecutor, MessageCtx};

/// 空闲会话 worker 的默认回收 TTL（D7）。
///
/// 10 分钟没有消息的会话，其 worker 任务与通道在一次入队检查时被回收；下一次同会话
/// 入队会重建 worker。取值要远大于一次 handler 的耗时（默认 2s 超时），避免"刚排空就拆"。
pub const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_secs(600);

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
    /// 测试用：当前存活的 worker 任务数（D7 要观察"回收后 worker 真的退出了"）。
    #[cfg(test)]
    live_workers: AtomicUsize,
}

struct Session {
    tx: mpsc::Sender<MessageCtx>,
    depth: Arc<AtomicUsize>,
    /// 最后一次有消息进出这个会话的时刻（`now_millis()`，D7 回收判据）。
    last_active: Arc<AtomicU64>,
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

    /// 记录一次活动（入队成功、或一条消息执行完毕）。
    fn touch(&self) {
        self.last_active.store(now_millis(), Ordering::Release);
    }
}

/// 每会话有界队列集合。
pub struct SessionQueues {
    shared: Arc<QueueShared>,
    sessions: Mutex<HashMap<SessionKey, Session>>,
    closing: AtomicBool,
    /// 空闲会话回收 TTL；`Duration::ZERO` = 关闭回收（测试里也用它固定行为）。
    idle_ttl: Duration,
    /// 下一次允许做回收扫描的时刻（`now_millis()`）。把扫描摊平到入队路径上，
    /// 不额外起定时任务——回收是"顺便做的清理"，不值得一个常驻任务。
    next_sweep: AtomicU64,
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
                #[cfg(test)]
                live_workers: AtomicUsize::new(0),
            }),
            sessions: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            idle_ttl: DEFAULT_SESSION_IDLE_TTL,
            next_sweep: AtomicU64::new(0),
        }
    }

    /// 覆盖空闲会话回收 TTL（D7）；`Duration::ZERO` 关闭回收。
    ///
    /// 主要给测试用：生产默认值见 [`DEFAULT_SESSION_IDLE_TTL`]。
    pub fn with_idle_ttl(mut self, idle_ttl: Duration) -> Self {
        self.idle_ttl = idle_ttl;
        self
    }

    /// 当前存活会话数（测试与指标用；会话被回收后这里会变小）。
    pub fn live_sessions(&self) -> usize {
        self.lock().len()
    }

    /// 测试用：当前存活的 worker 任务数。
    #[cfg(test)]
    pub fn live_workers(&self) -> usize {
        self.shared.live_workers.load(Ordering::Acquire)
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
        // D7：顺手回收空闲会话。**必须**在这把锁里做——回收与「取 tx 塞消息」在同一把锁下，
        // 才不会出现"worker 已退出但新消息被塞进旧通道"的静默丢消息。
        self.reclaim_idle(&mut sessions);
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
        session.touch();
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
        let last_active = Arc::new(AtomicU64::new(now_millis()));
        let shared = Arc::clone(&self.shared);
        let worker_depth = Arc::clone(&depth);
        let worker_last_active = Arc::clone(&last_active);
        let worker = tokio::spawn(async move {
            run_worker(shared, key, rx, worker_depth, worker_last_active).await;
        });
        Session {
            tx,
            depth,
            last_active,
            worker,
        }
    }

    /// 回收空闲会话（D7）：`depth == 0` 且超过 TTL 的会话从表里移除。
    ///
    /// 不变量：
    /// * 只在 `depth == 0` 时回收——`depth` 在 `reserve` 时 +1、handler 返回后 -1，
    ///   所以 `depth == 0` 蕴含「没有排队、也没有正在跑的」。
    /// * 移除动作与 `try_enqueue` 的插入/发送都在 `sessions` 这把锁里，因此不会出现
    ///   "worker 已退出、新消息塞进旧通道"的静默丢消息。
    ///
    /// 移除 `Session` 会丢弃它的 `tx`，worker 侧 `rx.recv()` 随即返回 `None` 并退出；
    /// `JoinHandle` 被丢弃只意味着 detached，不会 abort（后台自然退出即可）。
    fn reclaim_idle(&self, sessions: &mut HashMap<SessionKey, Session>) {
        if self.idle_ttl.is_zero() || sessions.is_empty() {
            return;
        }
        let now = now_millis();
        if now < self.next_sweep.load(Ordering::Acquire) {
            return;
        }
        let ttl = millis_u64(self.idle_ttl);
        self.next_sweep
            .store(now.saturating_add(ttl), Ordering::Release);
        sessions.retain(|_key, session| {
            if session.depth.load(Ordering::Acquire) != 0 {
                return true;
            }
            now.saturating_sub(session.last_active.load(Ordering::Acquire)) < ttl
        });
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
    last_active: Arc<AtomicU64>,
) {
    #[cfg(test)]
    shared.live_workers.fetch_add(1, Ordering::AcqRel);
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
        shared.total.fetch_sub(1, Ordering::AcqRel);
        // D7：一条消息跑完也算活动，别让"刚跑完就超 TTL"把会话拆掉。
        last_active.store(now_millis(), Ordering::Release);
        // D6：**每一次**释放都必须唤醒等待者，不能只在 `total` 归零时唤醒。
        //
        // `enqueue` 等的是"**本会话**腾出容量"，而 `total` 是全部会话的和：会话 A 满、
        // 会话 B 有在跑的消息时，A 的 worker 排空一条后 `total` 仍然 > 0，只在归零时
        // 唤醒会让 A 的等待者一直睡到整个 bot 空闲为止——读循环卡在 A 的 `enqueue` 上，
        // 表现就是"bot 不回消息，日志里什么都没有"。
        //
        // `notify_waiters` 只唤醒**当时**已注册的等待者，所以 `enqueue` 里
        // `notified.as_mut().enable()` 必须先于 `try_enqueue`（现有写法如此）：先注册、
        // 再试、失败才 await，中间没有丢唤醒的窗口。
        shared.progress.notify_waiters();
    }
    #[cfg(test)]
    shared.live_workers.fetch_sub(1, Ordering::AcqRel);
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// 当前时刻的毫秒（D7 用；只做"比先后"，不要求墙钟准确）。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, millis_u64)
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

    impl Recorder {
        /// 有 handler 正在执行（D7 用例要确认 `depth > 0` 的状态已经成立）。
        async fn saw_running(&self) -> bool {
            *self.running.lock().await > 0
        }
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

    /// 按会话卡住的 handler：只卡指定会话，其余会话照常跑完。
    ///
    /// D6 的现场是"**别的**会话还在跑"，所以不能用 `Recorder` 的全局闸门——那会把
    /// A 也一起卡住，测不出"释放容量后是否唤醒等待者"。
    #[derive(Default)]
    struct GateByConversation {
        gates: std::sync::Mutex<Vec<(ConversationId, Arc<Semaphore>)>>,
        started: Mutex<Vec<String>>,
        finished: Mutex<Vec<String>>,
    }

    impl GateByConversation {
        fn gate_for(&self, conversation: ConversationId) -> Option<Arc<Semaphore>> {
            let gates = self
                .gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gates
                .iter()
                .find(|(key, _)| *key == conversation)
                .map(|(_, gate)| Arc::clone(gate))
        }

        fn block(&self, conversation: ConversationId, gate: Arc<Semaphore>) {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gates.push((conversation, gate));
        }
    }

    #[async_trait]
    impl HandlerExecutor for GateByConversation {
        async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
            self.started.lock().await.push(ctx.external_id().to_owned());
            if let Some(gate) = self.gate_for(ctx.conversation_id()) {
                let _permit = gate.acquire().await;
            }
            self.finished
                .lock()
                .await
                .push(ctx.external_id().to_owned());
            Ok(())
        }
    }

    // D6：会话 A 等容量时，只要 **A 自己** 排空了一条就必须被唤醒——哪怕别的会话
    // 还有消息在跑（`total > 0`）。修复前只在 `total` 归零时唤醒，这里会一直阻塞
    // 到整个 bot 空闲，读循环就卡在 A 的 `enqueue` 上（表现为 bot 假死、日志无错）。
    #[tokio::test]
    async fn capacity_release_wakes_the_waiter_while_other_sessions_stay_busy() {
        let recorder = Arc::new(GateByConversation::default());
        let gate_a = Arc::new(Semaphore::new(0));
        let gate_b = Arc::new(Semaphore::new(0));
        let conversation_a = ConversationId::from_uuid(Uuid::from_u128(3));
        let conversation_b = ConversationId::from_uuid(Uuid::from_u128(4));
        recorder.block(conversation_a, Arc::clone(&gate_a));
        recorder.block(conversation_b, Arc::clone(&gate_b));

        let queues = Arc::new(SessionQueues::new(
            QueueConfig {
                session_capacity: 1,
                max_in_flight: 8,
            },
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        ));

        let (_stop_tx, mut stop_rx) = watch::channel(false);
        queues
            .enqueue(ctx(2, 3, 1), &mut stop_rx)
            .await
            .expect("a1");
        queues
            .enqueue(ctx(2, 4, 2), &mut stop_rx)
            .await
            .expect("b1");
        // 等到两个会话都真的进了 handler：A 占满自己的容量（1），B 让 total 保持 > 0。
        for _ in 0..2_000 {
            if recorder.started.lock().await.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            recorder.started.lock().await.len(),
            2,
            "前提：两个会话都得在 handler 里"
        );
        assert_eq!(queues.total_depth(), 2);

        // A 容量是 1：第二条只能在后台等容量。
        let (_stop_tx2, mut stop_rx2) = watch::channel(false);
        let mut waiter = {
            let queues = Arc::clone(&queues);
            tokio::spawn(async move { queues.enqueue(ctx(2, 3, 3), &mut stop_rx2).await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut waiter)
                .await
                .is_err(),
            "A 已满时等待者必须真的在等，否则这条用例没有区分力"
        );

        // 只放行 A 的第一条：A 腾出容量，而 B 仍然卡着（total 还是 > 0）。
        gate_a.add_permits(1);
        let joined = tokio::time::timeout(Duration::from_secs(2), &mut waiter)
            .await
            .expect("D6：A 腾出容量后必须唤醒等待者，不能等整个 bot 空闲")
            .expect("等待任务不能 panic");
        joined.expect("第二条消息应当入队成功");
        assert!(
            !recorder
                .finished
                .lock()
                .await
                .iter()
                .any(|id| id == "evt-b1"),
            "断言前提：此刻 B 仍未完成，total > 0"
        );

        // 收尾：放行所有闸门，等队列排空。
        gate_a.add_permits(8);
        gate_b.add_permits(8);
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
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

    // -----------------------------------------------------------------------
    // D7：会话 worker 只增不减（长期内存/任务泄漏）
    // -----------------------------------------------------------------------

    /// 空闲会话必须被回收——连同它的 worker 任务与通道——并在下次入队时重建。
    #[tokio::test]
    async fn idle_sessions_are_reclaimed_and_rebuilt_on_demand() {
        let recorder = Arc::new(Recorder::default());
        let queues = SessionQueues::new(
            QueueConfig::default(),
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        )
        .with_idle_ttl(Duration::from_millis(40));
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        queues
            .enqueue(ctx(2, 3, 1), &mut stop_rx)
            .await
            .expect("enqueued");
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        assert_eq!(queues.live_sessions(), 1);
        assert_eq!(queues.live_workers(), 1);

        // 超过 TTL 后，下一次入队顺手回收会话 3 的 worker。
        tokio::time::sleep(Duration::from_millis(120)).await;
        queues
            .enqueue(ctx(2, 4, 2), &mut stop_rx)
            .await
            .expect("enqueued");
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        assert_eq!(
            queues.live_sessions(),
            1,
            "空闲会话必须被回收，只剩刚入队的那一个"
        );
        assert_eq!(queues.live_workers(), 1, "被回收会话的 worker 必须真的退出");
        assert_eq!(queues.depth(&key_of(2, 3)), 0);

        // 回收后再入队同一会话：必须重建 worker 并照旧执行（不丢、不乱序）。
        queues
            .enqueue(ctx(2, 3, 5), &mut stop_rx)
            .await
            .expect("enqueued");
        queues
            .enqueue(ctx(2, 3, 6), &mut stop_rx)
            .await
            .expect("enqueued");
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        let seen = recorder.seen.lock().await.clone();
        assert_eq!(
            seen,
            vec!["evt-1", "evt-2", "evt-5", "evt-6"],
            "回收不能丢消息，重建后的会话仍然 FIFO"
        );
    }

    /// `depth > 0` 的会话**绝不**能被回收（回收正在跑/排队的会话就是丢消息）。
    #[tokio::test]
    async fn busy_session_is_never_reclaimed() {
        let recorder = Arc::new(Recorder::default());
        let gate = Arc::new(Semaphore::new(0));
        *recorder.gate.lock().await = Some(Arc::clone(&gate));
        let queues = SessionQueues::new(
            QueueConfig::default(),
            recorder.clone(),
            Arc::new(FailureTracker::new(3)),
        )
        .with_idle_ttl(Duration::from_millis(20));
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        queues
            .enqueue(ctx(2, 3, 1), &mut stop_rx)
            .await
            .expect("enqueued");
        // 等到 handler 真的卡在闸门上（depth 已经是 1）。
        let mut waited = 0_u64;
        while !recorder.saw_running().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
            waited += 5;
            assert!(waited < 2_000, "handler 没有开始执行");
        }

        // 远超 TTL 之后再用另一个会话触发回收扫描。
        tokio::time::sleep(Duration::from_millis(80)).await;
        queues
            .enqueue(ctx(2, 4, 2), &mut stop_rx)
            .await
            .expect("enqueued");
        assert_eq!(
            queues.live_sessions(),
            2,
            "仍在执行（depth > 0）的会话绝不能被回收"
        );

        gate.add_permits(4);
        queues
            .wait_drained(Duration::from_secs(5))
            .await
            .expect("drained");
        let seen = recorder.seen.lock().await.clone();
        assert_eq!(seen, vec!["evt-1", "evt-2"], "两个会话的消息都不能丢");
    }

    fn key_of(bot: u128, conversation: u128) -> SessionKey {
        SessionKey::new(
            BotId::from_uuid(Uuid::from_u128(bot)),
            ConversationId::from_uuid(Uuid::from_u128(conversation)),
        )
    }
}
