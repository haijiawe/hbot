//! `platform = "local"` 的 [`BotAdapter`]：本地试跑，**不联网**。
//!
//! 平台原语（标识、试跑会话 id、`[reply]` 行格式）在
//! `hbot_adapters::platforms::local`——那是 `hbot-adapters` 的依赖约束决定的
//! （该 crate 只依赖 `hbot-core`，不认识 `BotAdapter`）。这里只做组合根该做的事：
//! 把 supervisor 的入站/出站接口接到「什么也不收、回复写日志」上。
//!
//! ## 三条行为
//!
//! 1. [`subscribe`](LocalAdapter::subscribe) 返回一个**正常构造**的订阅，但生产者端
//!    留在适配器自己身上：**不发送任何事件，也绝不 drop sender**。`hbot-bot` 的
//!    supervisor 在读循环里把 `recv() == None`（sender 全丢）当成"适配器连接断了"，
//!    会 `tracing::warn!("adapter subscription closed; stopping read loop")` 后跳出
//!    循环 —— 状态仍是 Running，但注入的消息再也不会被消费。本地试跑恰恰一条事件都
//!    不会推，所以"把 sender 存起来"不是省事，是这个平台的正确形状。
//! 2. [`send`](BotAdapter::send) 不连任何东西：把文本按 [`REPLY_PREFIX`] 逐行写进
//!    run log（日志表 + `Event::RunLog`），然后返回 `Ok`。返回成功是**诚实**的：
//!    "发出去了"在这个平台上的定义就是"写进日志了"，没有哪个接收方会失败。
//! 3. `declared_conversations()` 是默认的空实现（不预置会话）：试跑用的
//!    `local:default` 行由 `bot_start` 幂等建好（见
//!    `crate::app::commands::bots::ensure_local_conversation`）。
//!
//! ## 为什么日志走 store + `Event::RunLog` 而不是 `tracing`
//!
//! IDE 的日志面板读的是 `logs_tail`（日志表）与 `Event::RunLog`，`tracing` 只写进程
//! stderr。`BotAdapter` 没有日志端口，所以端口由组合根在装配时注入
//! （[`crate::app::runtime::AdapterPorts`]）——`send()` 拿到的信封里也没有 store 句柄。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use hbot_adapters::platforms::local as primitives;
use hbot_bot::{AdapterSubscription, BotAdapter, OutboundEnvelope, PlatformEvent};
use hbot_core::{BotId, Clock, EventSink, HbotError};
use hbot_store::{NewLog, Store};
use tokio::sync::mpsc;

use crate::app::EventBus;
use crate::dto::{Event, LogLevel};

/// `local` 平台标识（**唯一一份**在 `hbot_adapters::platforms::local`，这里只是让
/// `runtime::PLATFORMS` 能与 `onebot::PLATFORM` / `qq::PLATFORM` 并列书写）。
pub const PLATFORM: &str = primitives::PLATFORM;

/// 试跑会话的外部 id（`bot_start` 幂等建行时用它，文档与 UI 也按它找会话）。
pub const DEFAULT_CONVERSATION_EXTERNAL_ID: &str = primitives::DEFAULT_CONVERSATION_EXTERNAL_ID;

/// `local` 平台的 `BotAdapter`：不联网、不产生事件、回复只写 run log。
///
/// 端口（store / events / clock）由 [`crate::app::runtime::PlatformRouter`] 在
/// `bot_start` 时通过 [`crate::app::runtime::AdapterFactory::create_with`] 注入；
/// 适配器自己不认识 `AppState`（那会让 `hbot-rpc` 的依赖方向倒过来）。
pub struct LocalAdapter {
    /// 这个适配器只服务一个 bot。
    bot_id: BotId,
    /// 写 run log 用。
    store: Arc<Store>,
    /// 推 `Event::RunLog`（IDE 日志面板）。
    events: Arc<EventBus>,
    /// 日志行的时间戳。
    clock: Arc<dyn Clock>,
    /// 事件通道的生产者端。**永远不发送、也永远不 drop**：见模块文档第 1 条。
    ///
    /// 用 `Option` 是为了让"第二次 subscribe"能明确报错（而不是悄悄换掉一个通道，
    /// 让上一个订阅的读循环变成孤儿）。
    events_tx: Mutex<Option<mpsc::Sender<PlatformEvent>>>,
    /// 已经订阅过的标记（订阅只能发生一次）。
    subscribed: AtomicBool,
}

impl LocalAdapter {
    /// 造一个还没订阅的 `local` 适配器。
    ///
    /// 不碰网络、不碰磁盘：构造是纯内存动作，`bot_start` 走到这里时唯一可能失败的是
    /// 平台路由（不是这个结构）。
    pub fn new(
        bot_id: BotId,
        store: Arc<Store>,
        events: Arc<EventBus>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bot_id,
            store,
            events,
            clock,
            events_tx: Mutex::new(None),
            subscribed: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl BotAdapter for LocalAdapter {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    // `declared_conversations()` 用 trait 的默认实现（空）：本地试跑没有"平台推来的
    // 会话"，预置一行只会让 supervisor 建出一个永远不会有消息的会话。试跑用的那一行
    // 由 `bot_start` 建（`local:default`），语义更准。

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        if self.subscribed.swap(true, Ordering::SeqCst) {
            return Err(HbotError::conflict(
                "local 适配器只能订阅一次：上一个订阅的事件通道还开着（重复 subscribe 会让读循环变成孤儿）",
            ));
        }

        // 有界通道，一条都不会进。容量刻意与真实适配器共用同一个常量
        // （`hbot_bot::supervisor::ADAPTER_EVENT_CAPACITY`）：即便这个平台永不发事件，
        // 也不该在"如果它发"的那天用另一个容量，supervisor 也不必为此分叉。
        let (sender, receiver) = mpsc::channel(hbot_bot::supervisor::ADAPTER_EVENT_CAPACITY);

        // 关键：发送端存进 `self`，让它在适配器的整个生命周期里活着。
        //
        // 反过来（`drop(sender)` 后只把 receiver 交出去）在本平台上是**稳定复现的
        // bug**：supervisor 的 `read_loop` 用 `subscription.events.recv().await` 等事件，
        // 拿到 `None` 就认为适配器断了、跳出循环 —— 而 `local` 永远不会有事件，于是
        // "bot 起来了但注入的消息没人消费"。这不是猜测，`hbot-bot` 的读循环对
        // `None` 的处理就是这么写的（`crates/hbot-bot/src/supervisor.rs` 的
        // `read_loop`：`None => { warn!("adapter subscription closed; stopping read loop"); break }`）。
        *lock(&self.events_tx) = Some(sender);

        tracing::info!(bot = %self.bot_id, "local 适配器已订阅：不产生事件，消息由 bot_inject 注入");
        Ok(AdapterSubscription::new(receiver))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        let mut lines = primitives::reply_lines(&envelope.text);
        // 附件：本地平台没有真的发送通道，但也要留痕 —— 试跑时用户要能确认
        // 「文件确实走到了适配器」。只记文件名与类型，**不记绝对路径**：
        // 这条日志会进 run log 面板与模型上下文（AGENTS.md 坑 47）。
        if let Some(attachment) = &envelope.attachment {
            let name = std::path::Path::new(&attachment.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment");
            lines.push(format!("[attachment] {} {name}", attachment.kind.as_str()));
        }
        if lines.is_empty() {
            // 空回复：没有可显示的行。不伪造一条 "[reply] "（那会让人以为 bot 回了空串），
            // 但也留一下痕，便于查"为什么点了回复日志里没有"。
            tracing::warn!(
                bot = %self.bot_id,
                outbound = %envelope.id,
                "local 适配器收到空回复：没有可写入的 [reply] 行"
            );
            return Ok(());
        }

        record_lines(
            &self.store,
            &self.events,
            &self.clock,
            self.bot_id,
            LogLevel::Info,
            lines,
        )
        .await;

        tracing::debug!(
            bot = %self.bot_id,
            outbound = %envelope.id,
            conversation = %envelope.conversation_id,
            "local 适配器把回复写进了 run log（没有真实发送）"
        );
        Ok(())
    }
}

/// 把若干行写进 run log：日志表（`logs_tail` 读它）+ `Event::RunLog`（面板实时读它）。
///
/// 与 `sandbox_handler` 的 `record_logs` 同一套做法：落库走 `spawn_blocking`（store 是
/// 同步单写线程），写失败只 `tracing::error!` 但**不吞**——回复已经产出了，为一条日志
/// 把整条出站链路判失败是错的；事件仍然照推，面板至少看得见。
pub(crate) async fn record_lines(
    store: &Arc<Store>,
    events: &Arc<EventBus>,
    clock: &Arc<dyn Clock>,
    bot_id: BotId,
    level: LogLevel,
    lines: Vec<String>,
) {
    let store = Arc::clone(store);
    let at = clock.now().unix_millis();
    let to_write = lines.clone();
    let level_row = level_column(level).to_owned();
    let written = tokio::task::spawn_blocking(move || {
        for line in &to_write {
            store.append_log(NewLog {
                bot_id,
                level: level_row.clone(),
                line: line.clone(),
                at,
            })?;
        }
        Ok::<(), hbot_store::StoreError>(())
    })
    .await;
    match written {
        Err(error) => tracing::error!(bot = %bot_id, error = %error, "日志写入任务失败"),
        Ok(Err(error)) => tracing::error!(bot = %bot_id, error = %error, "日志写入失败"),
        Ok(Ok(())) => {}
    }
    let at = clock.now();
    for line in lines {
        events.emit(Event::RunLog {
            bot_id,
            level,
            line,
            at,
        });
    }
}

/// `LogLevel` → `logs.level` 列里的字符串（与 `bots::log_level` 的读取方向同一张表）。
fn level_column(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// 拿锁；中毒时取回内部值（与本 crate 其它适配器同一写法：日志/事件端口不该因为
/// 一次 panic 就永久不可用）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
