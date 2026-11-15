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

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hbot_core::{BotId, Timestamp};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// 停止一个 bot 时等它自己收尾的宽限期。
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// 一个运行中的 bot。
pub struct RunningBot {
    /// 平台标识（`onebot11`），日志与状态面板用。
    pub platform: String,
    /// 启动时间。
    pub started_at: Timestamp,
    /// 停止信号：supervisor 收到后停止读取并排空。
    stop: oneshot::Sender<()>,
    /// supervisor 的后台任务。
    task: JoinHandle<()>,
}

/// 登记表。
#[derive(Default)]
pub struct BotRegistry {
    inner: Mutex<HashMap<BotId, RunningBot>>,
    /// 已发生的启动次数（诊断用：反复启停不泄漏任务时这个数会长，表不会）。
    starts: AtomicU64,
}

impl BotRegistry {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个刚启动的 bot。
    ///
    /// 同一个 `BotId` 已在表里时返回旧的那一条，调用方必须先 [`stop_all`] 或
    /// 自己处理——重复启动同一个 bot 会产生两个读循环，同一会话的消息就会乱序。
    ///
    /// [`stop_all`]: Self::stop_all
    pub fn insert(&self, bot_id: BotId, bot: RunningBot) -> Option<RunningBot> {
        self.starts.fetch_add(1, Ordering::Relaxed);
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

    /// 启动次数（含已停止的）。
    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::Relaxed)
    }

    /// 停一个 bot：先发停止信号，宽限期内等它退出，超时才 abort。
    ///
    /// 返回 `false` 表示这个 bot 本来就没在跑。
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
}

impl std::fmt::Debug for BotRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotRegistry")
            .field("running", &self.running())
            .field("starts", &self.starts())
            .finish()
    }
}

/// 构造一个运行中的 bot（供 `bot_start` 使用）。
pub fn running_bot(
    platform: impl Into<String>,
    started_at: Timestamp,
    task: JoinHandle<()>,
) -> (RunningBot, oneshot::Receiver<()>) {
    let (stop, stopped) = oneshot::channel();
    (
        RunningBot {
            platform: platform.into(),
            started_at,
            stop,
            task,
        },
        stopped,
    )
}
