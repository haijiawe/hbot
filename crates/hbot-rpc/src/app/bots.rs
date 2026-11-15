//! 正在运行的 bot 登记表。
//!
//! 一个 bot 从 `bot_start` 到 `bot_stop` 之间是"运行中"：它有一个后台任务（supervisor
//! 的读循环 + 会话队列）和一条停止信号。这里只做**登记**，不实现启动逻辑——启动需要
//! 适配器、执行器、store 三样东西，由 [`crate::app::commands::bots`] 装配。
//!
//! 为什么不让 `hbot-bot` 自己保存这张表：`hbot-bot` 不认识 `BotId` 之外的进程状态
//! （它被设计成可在测试里跑多个实例），"哪些 bot 在本进程里活着"是应用层的事实。
//!
//! ## 停止顺序
//!
//! `bot_stop` 先发停止信号让 supervisor 停止读取、排空队列、flush outbox，再等它
//! 退出；超过宽限期才 abort。反过来（先 abort）会丢队列里没处理完的消息
//! （`docs/02-architecture.md` §4.5）。
//!
//! ## 启动中的预留（reservation）
//!
//! `bot_start` 是异步的（读配置、连 WS、起 supervisor），两次 `bot_start` 并发进来时
//! 它们都还没 `insert`，只看 `is_running` 会双双通过、最后起两个读循环，同一会话的消息
//! 就会乱序。所以"启动中"也要占位：[`BotRegistry::reserve`] 失败即 -32003，启动结束
//! （成功或失败）都必须 [`BotRegistry::release`]。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hbot_bot::Supervisor;
use hbot_core::{BotId, Timestamp};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use std::sync::Arc;

/// 停止一个 bot 时等它自己收尾的宽限期。
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// 一个运行中的 bot。
pub struct RunningBot {
    /// 平台标识（`onebot11`），日志与状态面板用。
    pub platform: String,
    /// 启动时间。
    pub started_at: Timestamp,
    /// supervisor 本体：`bot_inject` 要借它走"落库 → 入队"的同一条路，
    /// 而不是直接调执行器（那样就绕过了去重与串行）。
    pub supervisor: Arc<Supervisor>,
    /// 停止信号：supervisor 收到后停止读取并排空。
    stop: oneshot::Sender<()>,
    /// 等待停止信号并调用 `supervisor.stop()` 的后台任务。
    task: JoinHandle<()>,
}

/// 登记表。
#[derive(Default)]
pub struct BotRegistry {
    inner: Mutex<HashMap<BotId, RunningBot>>,
    /// 正在启动（尚未 `insert`）的 bot。
    reserved: Mutex<HashSet<BotId>>,
    /// 已发生的启动次数（诊断用：反复启停不泄漏任务时这个数会长，表不会）。
    starts: AtomicU64,
}

impl BotRegistry {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 占位：从"开始启动"到"登记完成"之间挡住第二次启动。
    ///
    /// 返回 `false` 表示已经在运行或正在启动——调用方应返回 -32003（conflict）。
    pub fn reserve(&self, bot_id: BotId) -> bool {
        if self.is_running(&bot_id) {
            return false;
        }
        self.reserved_lock().insert(bot_id)
    }

    /// 释放占位（启动成功或失败都必须调用）。
    pub fn release(&self, bot_id: &BotId) {
        self.reserved_lock().remove(bot_id);
    }

    /// 是否正在启动。
    pub fn is_reserved(&self, bot_id: &BotId) -> bool {
        self.reserved_lock().contains(bot_id)
    }

    /// 登记一个刚启动的 bot（并释放启动占位）。
    ///
    /// 同一个 `BotId` 已在表里时返回旧的那一条，调用方必须先 [`stop_all`] 或
    /// 自己处理——重复启动同一个 bot 会产生两个读循环，同一会话的消息就会乱序。
    ///
    /// [`stop_all`]: Self::stop_all
    pub fn insert(&self, bot_id: BotId, bot: RunningBot) -> Option<RunningBot> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        self.release(&bot_id);
        self.lock().insert(bot_id, bot)
    }

    /// 是否在运行。
    pub fn is_running(&self, bot_id: &BotId) -> bool {
        self.lock().contains_key(bot_id)
    }

    /// 正在运行的 bot（按启动顺序无关的稳定顺序返回，便于断言）。
    pub fn running(&self) -> Vec<BotId> {
        let mut ids: Vec<BotId> = self.lock().keys().copied().collect();
        ids.sort_by_key(ToString::to_string);
        ids
    }

    /// 正在运行的平台标识。
    pub fn platform_of(&self, bot_id: &BotId) -> Option<String> {
        self.lock().get(bot_id).map(|bot| bot.platform.clone())
    }

    /// 正在运行的 supervisor（`bot_inject` 用）。
    pub fn supervisor_of(&self, bot_id: &BotId) -> Option<Arc<Supervisor>> {
        self.lock()
            .get(bot_id)
            .map(|bot| Arc::clone(&bot.supervisor))
    }

    /// 正在运行的那个 bot 的启动时间。
    pub fn started_at(&self, bot_id: &BotId) -> Option<Timestamp> {
        self.lock().get(bot_id).map(|bot| bot.started_at)
    }

    /// 启动次数（含已停止的）。
    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::Relaxed)
    }

    /// 停一个 bot：先发停止信号，宽限期内等它退出，超时才 abort。
    ///
    /// 返回 `false` 表示这个 bot 本来就没在跑（幂等：`bot_stop` 因此可以直接成功）。
    pub async fn stop(&self, bot_id: &BotId) -> bool {
        let Some(RunningBot { mut task, stop, .. }) = self.lock().remove(bot_id) else {
            return false;
        };
        let _ = stop.send(());
        if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
            tracing::warn!(bot = %bot_id, "supervisor 未在宽限期内退出，abort");
            task.abort();
        }
        true
    }

    /// 停掉全部（切项目、退出、重载配置时用）。
    pub async fn stop_all(&self) {
        let ids = self.running();
        for id in ids {
            self.stop(&id).await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BotId, RunningBot>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reserved_lock(&self) -> std::sync::MutexGuard<'_, HashSet<BotId>> {
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for BotRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotRegistry")
            .field("running", &self.running())
            .field("starts", &self.starts())
            .finish_non_exhaustive()
    }
}

/// 构造一个运行中的 bot（供 `bot_start` 使用）。
///
/// 它自己 spawn 收尾任务：等 `bot_stop` 的停止信号 → `Supervisor::stop()`（优雅停止：
/// 停止读取 → 排空队列 → flush outbox）。`bot_stop` 只需要发信号并等这个任务。
pub fn running_bot(
    platform: impl Into<String>,
    started_at: Timestamp,
    supervisor: Arc<Supervisor>,
) -> RunningBot {
    let (stop, stopped) = oneshot::channel();
    let bot_id = supervisor.bot_id();
    let stopping = Arc::clone(&supervisor);
    let task = tokio::spawn(async move {
        // `Err` 表示发送端已消失（例如 Registry 被丢弃）：此时按"进程要退了"处理，
        // 照样优雅停止，不要泄漏读循环。
        let _ = stopped.await;
        if let Err(error) = stopping.stop().await {
            tracing::warn!(bot = %bot_id, error = %error, "supervisor 停止时返回错误");
        }
    });
    RunningBot {
        platform: platform.into(),
        started_at,
        supervisor,
        stop,
        task,
    }
}
