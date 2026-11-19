//! [`HandlerExecutor`] 的真实现：把一条消息编译 + 丢进 QuickJS isolate，再把
//! `ctx.reply()` 的结果送回 outbox。
//!
//! ## 数据流（与 `docs/02-architecture.md` §6.3 对齐）
//!
//! ```text
//! MessageCtx ─► 事件 JSON ─► worker[k]（每 bot ≤2 个 isolate）─► Sandbox::dispatch
//!                                                                   │
//!                      replies ─► mpsc<OutboundRequest> ─► Supervisor::send ─► outbox
//!                      logs    ─► 日志表 + Event::RunLog
//! ```
//!
//! ## 三条硬约束
//!
//! 1. **编译与 dispatch 都是阻塞的**（SWC 解析 + QuickJS 单线程运行时），必须走
//!    `tokio::task::spawn_blocking`，否则会把整个 tokio reactor 卡住。
//! 2. **会话亲和**：`conversation_id` 稳定哈希到固定 worker，同一会话永远落在同一个
//!    isolate 上（§6.3）；一个 worker 同时只跑一个 dispatch，所以同会话不可能并发。
//! 3. **不绕过沙箱**：沙箱起不来、handler 超时一律返回真错误，让 supervisor 不标记
//!    inbound 已处理（重启后可重放）。不要在这里"先回个 ok 让它过"。
//!
//! ## 内容指纹（D1：热路径零内容读）
//!
//! 判断"代码变了没有"用的是 [`FingerprintCache`]：元数据 TTL（默认 2s）内只 `stat`，
//! 内容地板（默认 5s）才重读文件内容。**稳态每条消息不读任何文件内容**——
//! 旧实现每条消息都 `blake3` 全部源码，1000 msg/min 就是每分钟几万次文件读。
//! 盲区与兜底写在 [`crate::app::runtime::fingerprint_cache`] 的模块文档里。
//!
//! ## 热重载语义（D2：§6.2 / §6.4）
//!
//! * **编译失败不换版本**：记 error 日志（日志表 + `Event::RunLog`），继续用上一次编译
//!   成功的项目服务；`onMessage` 不失败（否则一个错别字会让之后每条消息都失败）。
//! * **第一次编译就失败**（还没有任何可用版本）：这条消息确实处理不了，返回真错误让它
//!   保持未处理并在修好代码后重试，错误文案里明确写出这一点。
//! * **去抖 300ms**：连续保存只编译一次（[`HOT_RELOAD_DEBOUNCE`]）。
//! * **崩溃预算**：同一指纹连续 3 次 isolate 崩溃就停止自动重启，写 error 日志与事件，
//!   改代码（新指纹）自动重置（[`CRASH_BUDGET_LIMIT`]，风格对齐 `queue.rs` 的
//!   `FailureTracker`）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hbot_bot::{EventSource, HandlerExecutor, MessageCtx, OutboundRequest, event_json};
use hbot_code::{CodeError, CompiledProject, Limits, Sandbox};
use hbot_core::{BotId, Clock, EventSink, HbotError};
use hbot_store::{NewLog, Store};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::app::debug::sink::BotDebugSink;
use crate::app::debug::{BreakpointTarget, DebugRegistry};
use crate::app::runtime::fingerprint_cache::FingerprintCache;
use crate::app::runtime::jobs::declared_jobs;
use crate::app::runtime::net::http_config;
use crate::app::runtime::send::declared_egress;
use crate::app::runtime::waits::declared_session;
use crate::app::runtime::webhook::declared_webhook;
use crate::app::{EventBus, store_failed};
use crate::dto::{Event, LogLevel};

/// 每个 bot 最多几个 isolate（§6.3：默认 2 个 worker）。
///
/// 2 个 isolate = 2×32 MiB 堆上限；再多只会放大会话争抢 CPU 的窗口，不会提高吞吐
/// （一个 bot 的流量本质上是一个群/一个私聊的串行对话）。
pub const MAX_ISOLATES: usize = 2;

/// 回复通道容量。outbox 自己是串行单 worker，这里只负责解耦
/// "handler 产出一个回复" 与 "发送器把它写进 outbound 表"。
pub const REPLY_CAPACITY: usize = 64;

/// 热重载去抖窗口（§6.4）。
///
/// 编辑器的连番保存（或"保存全部"）在 300ms 内会连续改变指纹；每次变化都编译 + 预热新
/// isolate 是纯浪费。新指纹第一次被观测到后先等这么久，再取**当次**指纹编译一次。
pub const HOT_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// 同一指纹允许的连续 isolate 崩溃次数（§6.4：连续 3 次就停止自动重启）。
pub const CRASH_BUDGET_LIMIT: u32 = 3;

/// 定时器 tick 的宿主侧周期（1 秒）。
///
/// 与 `hbot-code` 的 [`hbot_code::timer::TICK`] 是**同一个值**：那边是计划表最小间隔
/// （`MIN_INTERVAL_MS`）的地板价，这边是宿主的轮询周期。轮询周期比最小间隔大，就等于
/// 让所有小间隔计划系统性迟到，所以两者必须相等；取值只在这里定义一次。
pub const TICK_INTERVAL: Duration = hbot_code::timer::TICK;

/// 一次作业 tick 最多认领几条。
///
/// 与定时器不同：定时器的"一批"由计划表长度决定（最多 16），作业的到期行可能有几百条。
/// 每认领一条就占住一个 isolate 跑一路（最长 10 分钟），所以一拍最多取
/// [`MAX_ISOLATES`] 条 —— 再多也只是排队等下一拍，而不是把 isolate 队列撑爆。
pub const JOB_CLAIM_LIMIT: u32 = MAX_ISOLATES as u32;

/// 一次等待 tick 最多结算几条超时。
///
/// 与 `JOB_CLAIM_LIMIT` 不同，这里**不**受 isolate 数限制：结算一条超时只是改一行并
/// 写一行日志，不占 isolate。取 32 是为了让「一秒内能记完的日志行」有个上界。
pub const WAIT_EXPIRE_LIMIT: u32 = 32;

/// 沙箱执行器的装配参数（字段全公开：`bot_start` 处一眼能看到都接了什么）。
pub struct SandboxHandlerConfig {
    /// 这个执行器属于哪个 bot（写日志行时要用）。
    pub bot_id: BotId,
    /// 生成代码的项目根（`src/main.ts` 所在目录）。
    pub project_root: std::path::PathBuf,
    /// 每 isolate 的堆/栈/超时限额（来自 `bots.json`，缺省 32 MiB / 2 s）。
    pub limits: Limits,
    /// 日志落库用（`logs_tail` 从这张表读）。
    pub store: Arc<Store>,
    /// `Event::RunLog` 推给 UI。
    pub events: Arc<EventBus>,
    /// 日志时间戳来源。
    pub clock: Arc<dyn Clock>,
    /// `ctx.reply()` 产出的回复发到这里；由 supervisor 的 drain 任务转成 outbox 行。
    pub replies: mpsc::Sender<OutboundRequest>,
    /// 状态能力（`ctx.kv` / `ctx.history`）的装配工厂。
    ///
    /// `None` = 这台宿主没有装配状态端口（测试、`hbot-bench`、以及任何没走
    /// `AppState::start_bot` 的路径），此时生成代码里 `ctx.kv.*` 每次调用都回
    /// `HBOT_STATE_UNAVAILABLE`，**不是**静默成功。
    ///
    /// 与 [`Self::device`] 同一套装配纪律：存的是工厂而不是现成实例，每次新建
    /// isolate 时现调一次，这样清单被 Agent 改过之后热重载能读到新声明
    /// （见 `crate::app::state` 的模块文档）。
    pub state: Option<Arc<dyn crate::app::state::StateSource>>,

    /// 智能能力（`ctx.ai.complete`）的装配工厂。
    ///
    /// `None` = 这台宿主没有装配模型客户端（测试、`hbot-bench`、以及任何没走
    /// `AppState::start_bot` 的路径），此时生成代码里 `ctx.ai.complete` 每次调用都回
    /// `HBOT_AI_UNAVAILABLE`，**不是**静默返回一段假文本。
    ///
    /// 存工厂而不是现成实例：清单的 `capabilities.ai` 会被 Agent 改，每次新建
    /// isolate 时现读一遍（与 [`Self::state`] 同一理由）。
    pub ai: Option<Arc<dyn crate::app::ai::AiSource>>,

    /// 作业入队端口（`ctx.jobs.enqueue` 落到 `bot_jobs` 表）。
    ///
    /// `None` = 这台宿主没有装配作业端口（测试、`hbot-bench`、以及任何没走
    /// `AppState::start_bot` 的路径），此时生成代码里 `ctx.jobs.enqueue` 每次调用都回
    /// `HBOT_JOBS_UNAVAILABLE`，**不是**静默把作业收进内存（那样重启后消失会像正常行为）。
    ///
    /// 装的是端口而不是"队列"：真正排在哪由 store 决定，沙箱不留第二份队列。
    pub jobs: Option<Arc<crate::app::runtime::jobs::StoreJobQueue>>,

    /// 会话等待端口（`ctx.session.prompt` 落到 `bot_waits` 表）。
    ///
    /// `None` = 这台宿主没有装配等待后端（测试、`hbot-bench`、以及任何没走
    /// `AppState::start_bot` 的路径），此时 `ctx.session.prompt` 报
    /// `HBOT_SESSION_UNAVAILABLE`，**不是**把等待收进内存（那样重启后消失会像正常行为）。
    pub waits: Option<Arc<crate::app::runtime::waits::StoreWaitQueue>>,
}

/// 一个 worker 槽位：指纹一致就复用，不一致说明代码变了，换新 isolate。
struct Worker {
    fingerprint: String,
    /// isolate 句柄。**故意不加锁**：串行化由本模块的槽位守卫负责（`run()` 在
    /// dispatch 期间一直持有 `workers[i]` 的锁 + 会话稳定哈希绑定，见本模块第 2 条
    /// 硬约束），不是靠这把锁。`Sandbox::dispatch` 本身是 `&self` + `Sync`，多个线程
    /// 同时调它**不会 UB**——worker 会串行执行并按到达顺序回复，但突发超过指令通道
    /// 深度会退化成一串 [`CodeError::SandboxDead`]（`hbot-code` 的 `dispatch` 文档
    /// 原话是"不要依赖这个类型做串行化"）。
    /// **所以别以为这里没有锁就可以并发 dispatch 同一个 isolate**；真要并行请加
    /// worker 槽位（`MAX_ISOLATES`），不要给这个句柄加锁。
    ///
    /// 历史上这里曾经是 `Arc<Mutex<Sandbox>>`，唯一原因是当时 `Sandbox` 持有一个裸
    /// `std::sync::mpsc::Receiver<()>` 而失去 `Sync`；`crates/hbot-code` 把那个字段改成
    /// `Mutex<Option<..>>` 之后 `Sandbox: Send + Sync`（该 crate 内有断言测试），
    /// 这把锁就成了纯粹的额外开销（还多一次跨线程取锁），已撤掉。
    sandbox: Arc<Sandbox>,
}

/// 上一次编译成功的项目。
struct CachedProject {
    fingerprint: String,
    project: Arc<CompiledProject>,
}

/// isolate 崩溃预算（风格对齐 `crates/hbot-bot/src/queue.rs` 的 `FailureTracker`）。
///
/// 只记"同一指纹的连续崩溃"：改代码产生新指纹就清零，因为新代码是新的诊断对象——
/// 把旧指纹的崩溃次数算到新版本头上会让用户改一次代码就永远起不来。
#[derive(Debug, Default)]
struct CrashBudget {
    fingerprint: Option<String>,
    consecutive: u32,
    blocked: bool,
}

impl CrashBudget {
    /// 记一次崩溃；本次让预算转为"已封顶"时返回 `true`（只转换一次）。
    fn record_crash(&mut self, fingerprint: &str) -> bool {
        if self.fingerprint.as_deref() != Some(fingerprint) {
            // 新指纹：新的诊断对象，计数与封顶状态全部重置（§6.4）。
            self.fingerprint = Some(fingerprint.to_owned());
            self.consecutive = 0;
            self.blocked = false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= CRASH_BUDGET_LIMIT && !self.blocked {
            self.blocked = true;
            return true;
        }
        false
    }

    /// 记一次成功（isolate 正常跑完一条消息）：清连续计数，但**不**解除封顶。
    ///
    /// 与 `FailureTracker::record_success` 一致：封顶之后只有新指纹能解锁，否则
    /// "崩三次 → 成功一次 → 再崩三次"会变成无限重启。
    fn record_success(&mut self) {
        self.consecutive = 0;
    }

    /// 该指纹是否已经封顶（停止自动重启）。
    fn is_blocked(&self, fingerprint: &str) -> bool {
        self.blocked && self.fingerprint.as_deref() == Some(fingerprint)
    }

    /// 当前连续崩溃次数（日志用）。
    fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// 热重载状态机：上次成功版本 + 失败指纹 + 去抖窗口 + 崩溃预算。
#[derive(Default)]
struct HotReload {
    /// 上一次编译成功的项目（编译失败时继续用它服务）。
    compiled: Option<CachedProject>,
    /// 最近一次编译失败的指纹（同指纹不再反复重试编译）。
    failed_fingerprint: Option<String>,
    /// 去抖：某个待编译指纹第一次被观测到的时刻。
    pending: Option<(String, Instant)>,
    /// isolate 崩溃预算。
    crashes: CrashBudget,
}

impl HotReload {
    /// 指纹与当前编译产物一致 → 直接命中。
    fn hit(&self, fingerprint: &str) -> Option<(String, Arc<CompiledProject>)> {
        let cached = self.compiled.as_ref()?;
        if cached.fingerprint != fingerprint {
            return None;
        }
        Some((cached.fingerprint.clone(), Arc::clone(&cached.project)))
    }

    /// 这个指纹已经确认编译失败：有上一版就复用它（**不再重编译**）。
    fn fallback_on_failure(&self, fingerprint: &str) -> Option<(String, Arc<CompiledProject>)> {
        if self.failed_fingerprint.as_deref() != Some(fingerprint) {
            return None;
        }
        self.last_good()
    }

    /// 上一次编译成功的版本。
    fn last_good(&self) -> Option<(String, Arc<CompiledProject>)> {
        self.compiled
            .as_ref()
            .map(|cached| (cached.fingerprint.clone(), Arc::clone(&cached.project)))
    }

    /// 记录一次编译失败（同指纹后续请求直接走上一版）。
    fn mark_failed(&mut self, fingerprint: &str) {
        self.failed_fingerprint = Some(fingerprint.to_owned());
        self.pending = None;
    }

    /// 记录一次编译成功：缓存换新，失败指纹清空，去抖窗口关闭。
    fn mark_compiled(&mut self, fingerprint: &str, project: Arc<CompiledProject>) {
        self.compiled = Some(CachedProject {
            fingerprint: fingerprint.to_owned(),
            project,
        });
        self.failed_fingerprint = None;
        self.pending = None;
    }

    /// 去抖：返回"等到这个时刻再编译"的 deadline；`None` 表示不必等。
    ///
    /// 同一指纹只记第一次观测的时刻（连番保存时不断有更新的指纹覆盖它，所以窗口从
    /// **最后一次**变化起算）。已经等够窗口就返回 `None`。
    fn debounce(&mut self, fingerprint: &str, now: Instant, window: Duration) -> Option<Instant> {
        if window.is_zero() {
            return None;
        }
        let started = match &self.pending {
            Some((pending, at)) if pending == fingerprint => *at,
            Some((pending, at)) if pending != fingerprint => {
                self.pending = Some((fingerprint.to_owned(), now));
                return Some(now + window);
            }
            _ => {
                self.pending = Some((fingerprint.to_owned(), now));
                return Some(now + window);
            }
        };
        let deadline = started + window;
        if now >= deadline {
            None
        } else {
            Some(deadline)
        }
    }
}

/// QuickJS 沙箱执行器（每 bot 一个实例）。
pub struct SandboxHandler {
    bot_id: BotId,
    project_root: std::path::PathBuf,
    limits: Limits,
    store: Arc<Store>,
    events: Arc<EventBus>,
    clock: Arc<dyn Clock>,
    replies: mpsc::Sender<OutboundRequest>,
    /// 内容指纹缓存（D1：稳态零内容读）。
    fingerprints: Arc<FingerprintCache>,
    /// 热重载状态（D2：旧版本兜底 / 去抖 / 崩溃预算）。
    hot: Mutex<HotReload>,
    /// 编译串行化：并发消息同时发现缓存失效时，只让一个去编译，其余等结果。
    compile_lock: tokio::sync::Mutex<()>,
    /// 去抖窗口（生产恒为 [`HOT_RELOAD_DEBOUNCE`]；测试可缩短以免拖慢用例）。
    debounce: Duration,
    /// 真正执行过几次编译（测试断言"连番保存只编译一次"用）。
    compiles: AtomicU64,
    workers: Vec<tokio::sync::Mutex<Option<Worker>>>,
    /// 调试登记表：新 isolate 出生时从这里取断点，dispatch 时把
    /// [`BotDebugSink`] 挂上去（§6.5.1）。
    ///
    /// 存的是进程级单例（[`crate::app::debug::registry`]），不是配置注入的字段：
    /// [`SandboxHandlerConfig`] 由 `commands::bots` 用完整结构体字面量构造，那条装配缝
    /// 属于别的单元（见 `crate::app::debug` 的模块文档）。
    debug: Arc<DebugRegistry>,
    /// 设备能力的闸与后端（§11.1 的"装配方必须调两个方法"）。
    ///
    /// `None` = 这台宿主没有装配设备能力（测试、`hbot-bench`、以及任何没走
    /// `AppState::start_bot` 的路径）。此时生成代码里 `ctx.device.*` 每次调用都回
    /// `HBOT_DEVICE_UNAVAILABLE`，**不是**静默成功。
    ///
    /// 存的是 `Arc<dyn …>`，在**每次新建 isolate 时**才装进沙箱（`:509` 那段），
    /// 这样热重载换版本时会连白名单一起重读（清单可能刚被 Agent 改过）。
    device: Option<DeviceAssembly>,
    /// 状态能力（`ctx.kv` / `ctx.history`）的装配工厂（`None` = 未装配）。
    ///
    /// 在**每次新建 isolate 时**才调 `handles()` 并 `set_state`（下面 `worker` 那段），
    /// 所以清单里 `capabilities.state` 的改动会在热重载后生效。
    state: Option<Arc<dyn crate::app::state::StateSource>>,
    /// 智能能力（`ctx.ai.complete`）的装配工厂（`None` = 未装配）。
    ///
    /// 与 [`Self::state`] 同一条纪律：在**每次新建 isolate 时**才调 `client()` 并
    /// `set_ai`，所以清单里 `capabilities.ai` 的改动会在热重载后生效。
    ai: Option<Arc<dyn crate::app::ai::AiSource>>,
    /// 作业能力的入队端口（`None` = 未装配，`ctx.jobs.enqueue` 报 `HBOT_JOBS_UNAVAILABLE`）。
    ///
    /// 与 [`Self::state`] 同一条纪律：**每次新建 isolate 时**才 `set_jobs`，所以清单里
    /// `capabilities.jobs` 的改动会在热重载后生效。
    jobs: Option<Arc<crate::app::runtime::jobs::StoreJobQueue>>,
    /// 会话等待端口（`None` = 未装配，`ctx.session.*` 报 `HBOT_SESSION_UNAVAILABLE`）。
    ///
    /// 与其它能力同一条纪律：**每次新建 isolate 时**才 `set_session`，所以清单里
    /// `capabilities.state.session` 的改动会在热重载后生效。
    waits: Option<Arc<crate::app::runtime::waits::StoreWaitQueue>>,
    /// 出站端口（`None` = 未装配，`ctx.send` / `ctx.notify` 报 `HBOT_SEND_UNAVAILABLE`）。
    ///
    /// 与其它能力同一条纪律：**每次新建 isolate 时**才 `set_egress`，所以清单里
    /// `capabilities.send` / `capabilities.notify` 的改动会在热重载后生效。
    send: Option<Arc<dyn hbot_code::send::SendQueue>>,
}

/// 设备能力的两个端口 + 装配时刻。
///
/// 为什么白名单不在这里存：应用的清单是**项目文件**，改它不会产生新的 isolate 指纹以外的
/// 信号；每次起 isolate 时现读一遍最省事，也避免"清单改了但沙箱还拿着旧名单"。
struct DeviceAssembly {
    gate: Arc<dyn hbot_core::DeviceGate>,
    backend: Arc<dyn hbot_core::DeviceBackend>,
}

impl SandboxHandler {
    /// 用装配参数构造（不编译、不建 isolate：第一条消息到来时才做）。
    pub fn new(config: SandboxHandlerConfig) -> Self {
        let fingerprints = Arc::new(FingerprintCache::new(config.project_root.clone()));
        let workers = (0..MAX_ISOLATES)
            .map(|_| tokio::sync::Mutex::new(None))
            .collect();
        Self {
            bot_id: config.bot_id,
            project_root: config.project_root,
            limits: config.limits,
            store: config.store,
            events: config.events,
            clock: config.clock,
            replies: config.replies,
            fingerprints,
            hot: Mutex::new(HotReload::default()),
            compile_lock: tokio::sync::Mutex::new(()),
            debounce: HOT_RELOAD_DEBOUNCE,
            compiles: AtomicU64::new(0),
            workers,
            debug: crate::app::debug::registry(),
            device: None,
            state: config.state,
            ai: config.ai,
            jobs: config.jobs,
            waits: config.waits,
            send: None,
        }
    }

    /// 装配设备能力（闸 + 后端）。在 [`Self::new`] 之后立刻调，其它什么都不用改。
    ///
    /// 两件事必须一起做（`hbot-code` 的 `s3_device` 测试钉住）：
    ///
    /// 1. `Sandbox::set_device(gate, backend)` —— 不做就是每次调用回
    ///    `HBOT_DEVICE_UNAVAILABLE: 设备能力尚未装配`；
    /// 2. `Sandbox::set_device_apps(apps)`（在起 isolate 时按清单读）—— 不做则 **`input`
    ///    永远被拒且没有任何提示**，因为后端的空名单是 fail-closed 的。
    ///
    /// 授权判据不在这里：闸（[`hbot_core::DeviceGate`]）是唯一真源，见
    /// `crate::app::device`。这里只负责"把闸装进去"。
    pub fn with_device(
        mut self,
        gate: Arc<dyn hbot_core::DeviceGate>,
        backend: Arc<dyn hbot_core::DeviceBackend>,
    ) -> Self {
        self.device = Some(DeviceAssembly { gate, backend });
        self
    }

    /// 装配状态能力（`ctx.kv` / `ctx.history` 的工厂）。在 [`Self::new`] 之后立刻调。
    ///
    /// 传的是工厂而不是现成实例：清单里的 `capabilities.state` 是**项目文件**，改了它
    /// 不会产生别的信号，所以每次起 isolate 时现读一遍（见 `crate::app::state` 的模块
    /// 文档）。不调用本方法 = `ctx.kv.*` 每次回 `HBOT_STATE_UNAVAILABLE`，明确失败，
    /// 不是静默放行。
    ///
    /// 授权判据不在这里：声明闸在 [`crate::app::state::StoreBotState`] 的每个方法第一句
    /// （唯一一份），这里只负责"把工厂装进去"。
    pub fn with_state(mut self, source: Arc<dyn crate::app::state::StateSource>) -> Self {
        self.state = Some(source);
        self
    }

    /// 装配智能能力（`ctx.ai.complete` 的工厂）。在 [`Self::new`] 之后立刻调。
    ///
    /// 不调用本方法 = `ctx.ai.complete` 每次回 `HBOT_AI_UNAVAILABLE`，明确失败，
    /// 不是静默返回一段假文本。
    ///
    /// 声明闸不在这里：清单的 `capabilities.ai` 由工厂现读（每个新 isolate 一次），
    /// 这里只负责"把工厂装进去"。
    pub fn with_ai(mut self, source: Arc<dyn crate::app::ai::AiSource>) -> Self {
        self.ai = Some(source);
        self
    }

    /// 装配作业能力（`ctx.jobs.enqueue` 的入队端口）。在 [`Self::new`] 之后立刻调。
    ///
    /// 不调用本方法 = `ctx.jobs.enqueue` 每次回 `HBOT_JOBS_UNAVAILABLE`，明确失败，
    /// 不是静默把作业收进内存。声明闸不在这里：清单的 `capabilities.jobs` 在每次起
    /// isolate 时现读（见 [`Self::worker`]），这里只负责"把端口装进去"。
    pub fn with_jobs(mut self, queue: Arc<crate::app::runtime::jobs::StoreJobQueue>) -> Self {
        self.jobs = Some(queue);
        self
    }

    /// 装配会话等待能力（`ctx.session.*` + `onResume` 的端口）。在 [`Self::new`] 之后立刻调。
    ///
    /// 不调用本方法 = `ctx.session.prompt` 每次回 `HBOT_SESSION_UNAVAILABLE`，明确失败，
    /// 不是把等待收进内存（旧项目 `wait_input` 的失败形状，见 `docs/00` P0-3）。
    pub fn with_waits(mut self, queue: Arc<crate::app::runtime::waits::StoreWaitQueue>) -> Self {
        self.waits = Some(queue);
        self
    }

    /// 装配出站能力（`ctx.send` / `ctx.notify` 的端口）。在 [`Self::new`] 之后立刻调。
    ///
    /// 不调用本方法 = `ctx.send` 每次回 `HBOT_SEND_UNAVAILABLE`，明确失败，不是静默把消息丢掉。
    /// 声明闸不在这里：清单的 `capabilities.send` / `capabilities.notify` 在每次起 isolate 时现读
    /// （见 [`Self::worker`]），这里只负责「把端口装进去」。
    pub fn with_send(mut self, queue: Arc<dyn hbot_code::send::SendQueue>) -> Self {
        self.send = Some(queue);
        self
    }

    /// 覆盖去抖窗口（测试用；生产不要调，§6.4 的 300ms 是验过的值）。
    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    /// 覆盖指纹缓存的两个 TTL（测试用）。
    ///
    /// 生产恒为 [`DEFAULT_METADATA_TTL`]（2s）/ [`DEFAULT_CONTENT_FLOOR`]（5s）。用例里把
    /// 元数据 TTL 设成零（每轮都 stat）可以把"改文件后多久被看见"从秒级压到即时，否则每个
    /// 改文件的用例都要真等 2 秒。
    #[cfg(test)]
    pub fn with_fingerprint_ttls(
        mut self,
        metadata_ttl: Duration,
        content_floor: Duration,
    ) -> Self {
        self.fingerprints = Arc::new(FingerprintCache::with_ttls(
            self.project_root.clone(),
            metadata_ttl,
            content_floor,
        ));
        self
    }

    /// 测试用：累计读过的文件内容次数（D1「稳态零内容读」的观测点）。
    #[cfg(test)]
    pub fn content_reads(&self) -> u64 {
        self.fingerprints.content_reads()
    }

    /// 项目根（诊断用）。
    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    /// 已执行编译次数（测试观测点：[`FingerprintCache::content_reads`] 的编译侧对应物）。
    pub fn compile_count(&self) -> u64 {
        self.compiles.load(Ordering::Relaxed)
    }

    /// 当前项目内容指纹（走 [`FingerprintCache`]，稳态不读内容）。
    async fn fingerprint(&self) -> Result<String, HbotError> {
        let cache = Arc::clone(&self.fingerprints);
        tokio::task::spawn_blocking(move || cache.current())
            .await
            .map_err(|error| {
                HbotError::internal(format!(
                    "计算项目指纹任务失败（bot {}）：{error}",
                    self.bot_id
                ))
            })?
    }

    /// 取（必要时编译）当前代码的编译产物。
    ///
    /// 返回的指纹**一定是实际服务的那一版的指纹**：编译失败时它是上一版成功版本的指纹，
    /// 所以 `worker()` 的"指纹变了就换 isolate"逻辑不会被一次失败带偏。
    async fn compiled(&self) -> Result<(String, Arc<CompiledProject>), HbotError> {
        let mut fingerprint = self.fingerprint().await?;

        if let Some(hit) = lock(&self.hot).hit(&fingerprint) {
            return Ok(hit);
        }
        if let Some(fallback) = lock(&self.hot).fallback_on_failure(&fingerprint) {
            return Ok(fallback);
        }

        // 去抖：新指纹先等窗口，等连番保存落定后再取一次指纹（可能又变了）。
        let deadline = {
            let mut hot = lock(&self.hot);
            hot.debounce(&fingerprint, Instant::now(), self.debounce)
        };
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            fingerprint = self.fingerprint().await?;
            if let Some(hit) = lock(&self.hot).hit(&fingerprint) {
                return Ok(hit);
            }
            if let Some(fallback) = lock(&self.hot).fallback_on_failure(&fingerprint) {
                return Ok(fallback);
            }
        }

        // 串行编译：并发消息在这里排队，前一个编译完后面的直接命中缓存。
        let _guard = self.compile_lock.lock().await;
        if let Some(hit) = lock(&self.hot).hit(&fingerprint) {
            return Ok(hit);
        }
        if let Some(fallback) = lock(&self.hot).fallback_on_failure(&fingerprint) {
            return Ok(fallback);
        }

        self.compiles.fetch_add(1, Ordering::Relaxed);
        let root = self.project_root.clone();
        // **恒用 Debug 构建**：只有 Debug 构建里才插了 `__hbot_trace` 探针，断点才可能命中。
        // 代价靠"没断点就不 arm"抵消 —— 探针快路径是一句 `if (__hbot_dbg && …)`，
        // 未 arm 时每条语句多一次全局读，实测 10 文件 24µs vs release 27µs（噪声内）；
        // arm 后 probe-dense 最坏 4.64×（`crates/hbot-code/tests/s1_debug_instrument.rs`）。
        // 若这里退回 `compile_project`（release），`debug_set_breakpoints` 会**静默永不命中**。
        let compiled = tokio::task::spawn_blocking(move || hbot_code::compile_project_debug(&root))
            .await
            .map_err(|error| {
                HbotError::internal(format!("编译任务失败（bot {}）：{error}", self.bot_id))
            })?;

        match compiled {
            Ok(project) => {
                let project = Arc::new(project);
                tracing::info!(
                    bot = %self.bot_id,
                    root = %self.project_root.display(),
                    modules = project.modules().len(),
                    fingerprint = %fingerprint,
                    "生成代码已编译（缓存已更新）"
                );
                lock(&self.hot).mark_compiled(&fingerprint, Arc::clone(&project));
                Ok((fingerprint, project))
            }
            Err(error) => {
                // 编译错误是"用户代码有问题"：写进日志表 + RunLog（UI 的诊断面板从这两处读），
                // 然后决定是继续服务上一版还是如实报错。
                self.record_error_line(format!("生成代码编译失败：{error}"))
                    .await;
                tracing::warn!(
                    bot = %self.bot_id,
                    root = %self.project_root.display(),
                    error = %error,
                    fingerprint = %fingerprint,
                    "生成代码编译失败"
                );

                // 先把"上一版"取出来放语句里（守卫随之释放）：`match lock(&self.hot)...`
                // 这种写法会让守卫活到整个 `match` 结尾，分支里再 `lock(&self.hot)` 就是
                // 同一个 std Mutex 的二次加锁——同线程直接死锁（曾经真的挂住过用例）。
                let last_good = lock(&self.hot).last_good();
                match last_good {
                    Some((last_good_fingerprint, project)) => {
                        // §6.2 / §6.4：编译失败**不替换**正在运行的版本，消息继续被处理。
                        lock(&self.hot).mark_failed(&fingerprint);
                        tracing::warn!(
                            bot = %self.bot_id,
                            serving = %last_good_fingerprint,
                            "编译失败：继续使用上一版已编译代码服务"
                        );
                        Ok((last_good_fingerprint, project))
                    }
                    None => Err(HbotError::invalid(format!(
                        "生成代码编译失败，且当前没有可用的已编译版本：{error}。\
                         这条消息保持未处理状态（inbound 未标记 processed），修好代码后会自动重试"
                    ))),
                }
            }
        }
    }

    /// 取（必要时创建/替换）某个 worker 的 isolate，并返回持有该槽位锁的守卫。
    ///
    /// 守卫生存期 = 一次 `dispatch`：同一个 isolate 永远不会被两条消息同时使用
    /// （QuickJS 运行时是单线程的，并发投递只会让超时判定失真）。
    async fn worker(
        &self,
        index: usize,
        fingerprint: &str,
        project: &Arc<CompiledProject>,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Worker>>, HbotError> {
        // 崩溃预算封顶：不再自动重启（§6.4），明确报错而不是静默重试。
        if lock(&self.hot).crashes.is_blocked(fingerprint) {
            return Err(HbotError::internal(format!(
                "isolate 连续崩溃 {CRASH_BUDGET_LIMIT} 次（指纹 {fingerprint}），已停止自动重启；\
                 改代码会产生新指纹并自动重置预算，或查看日志定位代码问题"
            )));
        }

        let slot = self.workers.get(index).ok_or_else(|| {
            HbotError::internal(format!("worker 槽位 {index} 越界（bot {}）", self.bot_id))
        })?;
        let mut guard = slot.lock().await;

        let stale = guard
            .as_ref()
            .map(|worker| worker.fingerprint != fingerprint)
            .unwrap_or(true);
        if stale {
            // 旧 isolate 的 Drop 会 `recv_timeout(handler_timeout + 1s)` 等它的 OS 线程
            // 退出（默认最多 3 秒），必须在阻塞线程里丢。
            //
            // 必须把**整个 `Worker`** move 进去，不能只 clone 一个 `Arc` 进去丢：join
            // 发生在最后一个强引用被 drop 的那一刻，而 `guard.take()` 之后最后那个引用
            // 就在 `previous` 里——只丢 clone 的话，真正触发 join 的是 `previous` 在
            // async 线程上的析构，等于没搬，reactor 照样卡满整个预算。
            if let Some(previous) = guard.take() {
                tokio::task::spawn_blocking(move || drop(previous))
                    .await
                    .map_err(|error| {
                        HbotError::internal(format!(
                            "回收旧 isolate 失败（bot {}）：{error}",
                            self.bot_id
                        ))
                    })?;
            }

            let limits = self.limits;
            let project = Arc::clone(project);
            let root = self.project_root.clone();
            let spawned = tokio::task::spawn_blocking(move || {
                // `http.get` 的真传输：清单的 `permissions.http`（空表 = 全禁）由
                // `hbot-code` 判，这里只负责把"清单 + 真 transport"装进沙箱。
                // 清单缺失/非法时退回全禁，绝不让沙箱因此起不来（见 `net::http_config`）。
                let http = http_config(&root);
                Sandbox::spawn_with_http((*project).clone(), limits, http)
            })
            .await
            .map_err(|error| {
                HbotError::internal(format!(
                    "isolate 启动任务失败（bot {}）：{error}",
                    self.bot_id
                ))
            })?;
            let sandbox = match spawned {
                Ok(sandbox) => Arc::new(sandbox),
                Err(error) => {
                    tracing::error!(bot = %self.bot_id, error = %error, "isolate 启动失败");
                    return Err(sandbox_start_error(error));
                }
            };

            // 设备能力：闸 + 后端 + 清单里的应用白名单，都在这里装（每个新 isolate 装一次）。
            // 漏掉这一步只会让 `ctx.device.*` 全部报"尚未装配"，是明确的失败，不是静默放行。
            if let Some(assembly) = &self.device {
                sandbox.set_device(Arc::clone(&assembly.gate), Arc::clone(&assembly.backend));
                sandbox.set_device_apps(device_apps(&self.project_root));
            }

            // 状态能力：工厂现读一次清单，把两个端口装进沙箱（每个新 isolate 装一次）。
            // 漏掉这一步只会让 `ctx.kv.*` 报"尚未装配"，是明确的失败，不是静默放行。
            if let Some(source) = &self.state {
                let (state, history) = source.handles(&self.project_root);
                sandbox.set_state(state, history);
            }

            // 智能能力：工厂现读一次清单，把模型客户端装进沙箱（每个新 isolate 装一次）。
            // 漏掉这一步只会让 `ctx.ai.complete` 报"尚未装配"，是明确的失败，不是静默放行。
            if let Some(source) = &self.ai {
                if let Some(client) = source.client(&self.project_root) {
                    sandbox.set_ai(client);
                }
            }

            // 定时计划：清单里 `capabilities.timer` 现读一遍装进槽位（每个新 isolate 装一次）。
            //
            // 用 `clock.now()` 而不是"进程启动时刻"：热重载换版本时计划重新对齐到当前
            // 时刻，不会因为 isolate 换了就把"下一次触发"倒退回去。
            let schedules = declared_schedules(&self.project_root);
            if !schedules.is_empty() {
                sandbox.set_schedules(&schedules, self.clock.now());
            }

            // 作业能力：清单里 `capabilities.jobs` 现读一遍，连同入队端口装进槽位
            // （每个新 isolate 装一次）。声明表与端口一起装：只装端口会让"声明了才准入队"
            // 变成"装配了就能入队"。
            if let Some(queue) = &self.jobs {
                sandbox.set_jobs(
                    &declared_jobs(&self.project_root),
                    Arc::clone(queue) as Arc<dyn hbot_code::jobs::JobQueue>,
                );
            }
            // 会话等待：声明开关与端口一起装（每个新 isolate 装一次）。只装端口会让
            // "声明了才准开等待"变成"装配了就能开"。
            if let Some(queue) = &self.waits {
                sandbox.set_session(
                    declared_session(&self.project_root),
                    Arc::clone(queue) as Arc<dyn hbot_code::session::WaitQueue>,
                );
            }
            // 出站能力：清单里 `capabilities.send` / `capabilities.notify` 现读一遍，连同端口一起装
            // （每个新 isolate 装一次）。三样一起装的理由同 `set_session`：只装端口会让「声明了才准发」
            // 变成「装配了就能发」，只装声明会让 `send` 永远拿不到队列。
            if let Some(queue) = &self.send {
                let (declared, target) = declared_egress(&self.project_root);
                sandbox.set_egress(declared, target, Arc::clone(queue));
            }
            tracing::debug!(
                bot = %self.bot_id,
                worker = index,
                fingerprint = %fingerprint,
                "isolate 已就绪"
            );
            // 新 isolate 出生时把当前断点补上去（"先设断点、后启动 bot"这条路径）：
            // `debug_set_breakpoints` 只能推给**已经存在**的沙箱，而 sandbox 是第一条消息
            // 到来时才建的，漏了这一步用户会看到"断点设了但没停住"。
            //
            // 必须先于把 `sandbox` move 进 `Worker`，所以要 clone 一个句柄给登记表。
            self.debug.attach(
                self.bot_id,
                Arc::clone(&sandbox) as Arc<dyn BreakpointTarget>,
            );
            *guard = Some(Worker {
                fingerprint: fingerprint.to_owned(),
                sandbox,
            });
        }

        Ok(guard)
    }

    /// 把 `EventOutcome.logs` 落到日志表并推 `RunLog` 事件。
    ///
    /// 日志写失败不改变 handler 结果（回复已经产出），但必须留痕——不许静默吞。
    async fn record_logs(&self, logs: Vec<String>) {
        if logs.is_empty() {
            return;
        }
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let at = self.clock.now().unix_millis();
        // 落库用副本：`lines` 之后还要拿去推 `RunLog`，不能 move 进阻塞任务。
        let to_write = logs.clone();
        let written = tokio::task::spawn_blocking(move || {
            for line in &to_write {
                store.append_log(NewLog {
                    bot_id,
                    level: "info".to_owned(),
                    line: line.clone(),
                    at,
                })?;
            }
            Ok::<(), hbot_store::StoreError>(())
        })
        .await;
        match written {
            Err(error) => tracing::error!(bot = %bot_id, error = %error, "日志写入任务失败"),
            Ok(Err(error)) => {
                tracing::error!(bot = %bot_id, error = %store_failed(error), "日志写入失败");
            }
            Ok(Ok(())) => {}
        }
        let at = self.clock.now();
        for line in logs {
            self.events.emit(Event::RunLog {
                bot_id,
                level: LogLevel::Info,
                line,
                at,
            });
        }
    }

    /// 写一条 `error` 级日志（日志表 + `Event::RunLog`）。
    ///
    /// 平台自身的问题（编译失败、崩溃预算封顶）走这里：UI 的日志面板与
    /// `bot_logs_tail` 读的是同一份数据。写失败只 warn，但 warn 本身不会被吞。
    async fn record_error_line(&self, line: String) {
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let at = self.clock.now().unix_millis();
        let to_write = line.clone();
        let written = tokio::task::spawn_blocking(move || {
            store.append_log(NewLog {
                bot_id,
                level: "error".to_owned(),
                line: to_write,
                at,
            })
        })
        .await;
        match written {
            Err(error) => tracing::error!(bot = %bot_id, error = %error, "错误日志写入任务失败"),
            Ok(Err(error)) => {
                tracing::error!(bot = %bot_id, error = %store_failed(error), "错误日志写入失败");
            }
            Ok(Ok(())) => {}
        }
        self.events.emit(Event::RunLog {
            bot_id,
            level: LogLevel::Error,
            line,
            at: self.clock.now(),
        });
    }

    /// 记一次 isolate 崩溃；连续崩溃达到预算时写 error 日志与事件（停止自动重启）。
    async fn record_crash(&self, fingerprint: &str) {
        let (exhausted, consecutive) = {
            let mut hot = lock(&self.hot);
            let exhausted = hot.crashes.record_crash(fingerprint);
            (exhausted, hot.crashes.consecutive())
        };
        if exhausted {
            tracing::error!(
                bot = %self.bot_id,
                consecutive,
                limit = CRASH_BUDGET_LIMIT,
                "isolate crash budget exhausted; auto-restart disabled"
            );
            self.record_error_line(format!(
                "isolate 连续崩溃 {consecutive} 次（指纹 {fingerprint}）：已停止自动重启，\
                 请查看日志定位代码问题；改动代码（新指纹）会自动重置预算"
            ))
            .await;
        } else {
            tracing::warn!(
                bot = %self.bot_id,
                consecutive,
                limit = CRASH_BUDGET_LIMIT,
                "isolate 崩溃，丢弃后重建"
            );
        }
    }

    /// 把 handler 产出的回复交给 supervisor 的 outbox 通道。
    ///
    /// 每条回复都带上触发它的**平台消息 ID**（`AGENTS.md` §4.1 被动回复语义）：
    /// 缺了它，OneBot 侧无法组 `reply` 段，也就无法计入被动回复配额。
    async fn deliver_replies(
        &self,
        ctx: &MessageCtx,
        replies: Vec<String>,
    ) -> Result<(), HbotError> {
        let reply_to = passive_reply_id(ctx);
        for text in replies {
            let request = OutboundRequest {
                conversation: ctx.conversation.clone(),
                text,
                reply_to: reply_to.clone(),
            };
            self.replies.send(request).await.map_err(|_| {
                HbotError::internal(format!(
                    "回复通道已关闭（bot {}）：supervisor 已停止，这条回复没有进入 outbox",
                    self.bot_id
                ))
            })?;
        }
        Ok(())
    }

    /// 跑一轮定时器 tick：把到点的计划逐个派发，并把日志/回复接到同一条通道。
    ///
    /// `now` 由调用方（tick 任务）给，**不在这里读时钟**：这样"现在几点"只有一个来源，
    /// 测试也能用固定值复现。
    ///
    /// ## 回复发往哪里
    ///
    /// 定时器没有入站消息，`ReplyTarget` 只能来自计划自己声明的 `conversation`。所以：
    ///
    /// * 计划写了 `conversation`：按 `(bot_id, external_id)` 在库里找会话行，找到就按
    ///   它的 `kind` 组 `ConversationRef`，`reply_to` 恒为 `None`（主动消息，没有可引用
    ///   的历史事件 —— 冒用旧 ID 会被平台判参数非法）；
    /// * 会话行还不存在（这个会话从没收到过消息）：**不猜类型**，记一行 error 日志说明
    ///   原因，这条回复丢掉。猜成 `private` 发群号、或猜成 `group` 发私聊，都是发错人。
    /// * 计划没写 `conversation`：`ctx.reply` 在 sandbox 里已经抛 `HBOT_TIMER_NO_TARGET`
    ///   （见 `hbot-code` 的 `build_timer_event`），所以这里拿不到回复，只有日志。
    ///
    /// 单条计划失败不影响其它计划：一次 tick 里每条独立结算，失败的那条写 error 日志。
    pub async fn tick(&self, now: hbot_core::Timestamp) -> Result<(), HbotError> {
        // 什么都没声明就先走：定时器是"声明了才有"的能力，让它在没声明时去编译项目、
        // 起 isolate，等于给每个 bot 加一份**每秒一次**的常态开销。
        if declared_schedules(&self.project_root).is_empty() {
            return Ok(());
        }
        // 没装表就不刷指纹、不编译、不建 isolate。
        let (fingerprint, project) = match self.compiled().await {
            Ok(pair) => pair,
            Err(error) => {
                // 编译失败时 tick 不重试：下一次 tick 会再走一遍，用户改代码即恢复。
                tracing::warn!(bot = %self.bot_id, error = %error, "定时器 tick 无法取到项目");
                return Ok(());
            }
        };
        let mut schedules: Vec<(usize, hbot_core::DueSchedule)> = Vec::new();
        for index in 0..self.workers.len() {
            let mut guard = self.worker(index, &fingerprint, &project).await?;
            let Some(worker) = guard.as_mut() else {
                continue;
            };
            for due in worker.sandbox.due(now) {
                schedules.push((index, due));
            }
        }
        if schedules.is_empty() {
            return Ok(());
        }

        for (index, due) in schedules {
            let name = due.name.clone();
            let outcome = self
                .dispatch_timer_on(index, &fingerprint, &project, due.clone())
                .await;
            match outcome {
                Ok(outcome) => {
                    self.record_logs(outcome.logs).await;
                    if let Err(error) = self.deliver_timer_replies(&due, outcome.replies).await {
                        // 回复没进 outbox 等于丢了：留痕，不静默吞。
                        tracing::warn!(
                            bot = %self.bot_id,
                            timer = %name,
                            error = %error,
                            "定时器回复未能进入 outbox"
                        );
                        self.record_error_line(format!(
                            "定时器 `{name}` 的回复没有发出去：{error}"
                        ))
                        .await;
                    }
                }
                Err(failure) => {
                    // 失败路径也要落日志：`ctx.log` 在抛出前写的那几行是排障的唯一线索。
                    let mapped = remap_error(&failure.error, &project);
                    self.record_logs(failure.logs).await;
                    self.record_error_line(format!("定时器 `{name}` 执行失败：{mapped}"))
                        .await;
                }
            }
        }
        Ok(())
    }

    /// 在**指定的** worker 上派发一条到点计划。
    ///
    /// `index` 来自 [`SandboxHandler::tick`] 里 `due()` 的调用位置：计划表是每个 isolate
    /// 各一份，在 A 上取到的到点条目必须在 A 上跑，否则 B 的计划表根本没推进过。
    async fn dispatch_timer_on(
        &self,
        index: usize,
        fingerprint: &str,
        project: &Arc<CompiledProject>,
        due: hbot_core::DueSchedule,
    ) -> Result<hbot_code::EventOutcome, hbot_code::EventFailure> {
        let mut guard = self
            .worker(index, fingerprint, project)
            .await
            .map_err(|error| {
                hbot_code::EventFailure::without_output(hbot_code::CodeError::SandboxDead {
                    message: error.to_string(),
                })
            })?;
        let Some(worker) = guard.as_mut() else {
            return Err(hbot_code::EventFailure::without_output(
                hbot_code::CodeError::SandboxDead {
                    message: format!(
                        "定时器 `{}` 的 isolate 槽位 {index} 在派发前被清空（bot {}）",
                        due.name, self.bot_id
                    ),
                },
            ));
        };
        // 已由 `tick` 的 `due()` 取过，这里不再判断"到没到点"：到点条目是**一次性**的，
        // 同一个 `DueSchedule` 派发两次就是两次执行。
        let sandbox = Arc::clone(&worker.sandbox);
        tokio::task::spawn_blocking(move || sandbox.dispatch_timer_collecting(&due))
            .await
            .map_err(|error| {
                hbot_code::EventFailure::without_output(hbot_code::CodeError::SandboxDead {
                    message: format!("定时器派发任务失败：{error}"),
                })
            })?
    }

    /// 把定时器产出的回复交给 supervisor 的 outbox 通道。
    ///
    /// 目标会话从库里按 `(bot_id, external_id)` 查：**会话类型不猜**，`kind` 取那一行的
    /// 值（见 [`SandboxHandler::tick`] 的说明）。查不到就报错，由调用方写成用户能读的
    /// 一行日志 —— 静默丢弃会让用户以为"定时器发了但没人收到"。
    async fn deliver_timer_replies(
        &self,
        due: &hbot_core::DueSchedule,
        replies: Vec<String>,
    ) -> Result<(), HbotError> {
        if replies.is_empty() {
            return Ok(());
        }
        let Some(external_id) = due.conversation.clone() else {
            return Err(HbotError::invalid(format!(
                "定时器 `{}` 产出了 {} 条回复，但计划没有声明 `conversation`：\
                 没有目标会话就不发（猜一个群发出去收不回来）",
                due.name,
                replies.len()
            )));
        };
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let lookup = external_id.clone();
        let conversation = tokio::task::spawn_blocking(move || {
            store.get_conversation_by_external(&bot_id, &lookup)
        })
        .await
        .map_err(|error| HbotError::internal(format!("查定时器目标会话的任务失败：{error}")))?
        .map_err(store_failed)?;
        let Some(row) = conversation else {
            return Err(HbotError::invalid(format!(
                "定时器 `{}` 的目标会话 `{external_id}` 还不存在（这个会话从没收到过消息）：\
                 先让它收到一条消息，或在对话列表里确认 externalId 写对了",
                due.name
            )));
        };
        let target = hbot_bot::ConversationRef::new(
            row.external_id.clone(),
            crate::app::runtime::conversation_kind_from_str(&row.kind),
        );
        for text in replies {
            let request = OutboundRequest {
                conversation: target.clone(),
                text,
                // 主动消息：没有可引用的历史事件，**不得**冒用旧 ID。
                reply_to: None,
            };
            self.replies.send(request).await.map_err(|_| {
                HbotError::internal(format!(
                    "回复通道已关闭（bot {}）：supervisor 已停止，这条定时器回复没有进入 outbox",
                    self.bot_id
                ))
            })?;
        }
        Ok(())
    }
    /// 跑一轮作业 tick：认领本 bot 的到期作业，逐条派发、落日志、发回复、结算终态。
    ///
    /// 与 [`Self::tick] 是**两条独立的循环**，不能合并成一条：定时器那条第一句就是
    /// "没声明计划就 return"，把它复用给作业会让"只声明了 jobs、没声明 timer"的项目
    /// 永远派发不到作业（而且看起来像"作业不跑"，不像配置问题）。
    ///
    /// ## 顺序：先编译，再认领
    ///
    /// 反过来的话，编译失败的那一刻作业行**已经被改成 `running 且 `attempts 已加一**，
    /// 下一拍又因为 `state != 'pending' 拿不到它 —— 这条作业就卡在 `running 里，
    /// 只能等下一次进程启动的 `resume_interrupted_jobs 救回来。编译在前，失败时行还
    /// 是 `pending，下一拍自然重试。
    ///
    /// 单条作业失败不影响同批其它作业：每条独立结算（重试 / 落 `failed）。
    /// 跑一轮等待 tick：结算到期的等待，把超时写进日志。
    ///
    /// 与 `tick` / `tick_jobs` 是**第三条**独立循环，理由同坑 69：每个「声明了才有」
    /// 的能力都要自己第一句读清单，不能指望共享的循环替它判。
    ///
    /// 超时**不派发 handler**：等待过期没有「该跑什么代码」的答案（用户没回话）。
    /// 所以这里只落一行 error 日志 —— 一行日志是超时唯一的痕迹，等待没有 handler。
    pub async fn tick_waits(&self, now: hbot_core::Timestamp) -> Result<(), HbotError> {
        if !declared_session(&self.project_root) {
            return Ok(());
        }
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let at = now.unix_millis();
        let expired = tokio::task::spawn_blocking(move || {
            store.expire_due_waits(&bot_id, at, WAIT_EXPIRE_LIMIT)
        })
        .await
        .map_err(|error| {
            HbotError::internal(format!(
                "结算到期等待的任务失败（bot {}）：{error}",
                self.bot_id
            ))
        })?;
        let expired = expired.map_err(store_failed)?;
        for row in expired {
            self.record_error_line(format!(
                "等待 `{}` 超时了：{} 秒内没有{}回答（提示是「{}」，目标会话 `{}`）。\
                 超时不会重发提示；要继续就再问用户一次 ctx.session.prompt",
                row.key,
                row.deadline / 1000,
                if row.scope == "conversation" {
                    "任何人"
                } else {
                    "匹配的人"
                },
                row.prompt,
                row.conversation
            ))
            .await;
        }
        Ok(())
    }

    /// 跑一轮作业 tick：认领本 bot 的到期作业，逐条派发、落日志、发回复、结算终态。
    ///
    /// 与 `tick` 是**两条独立的循环**，不能合并成一条：定时器那条第一句就是「没声明
    /// 计划就 return」，把它复用给作业会让「只声明了 jobs、没声明 timer」的项目永远
    /// 派发不到作业（而且看起来像「作业不跑」，不像配置问题）。
    ///
    /// **顺序是先编译、后认领**：反过来的话，编译失败那一刻行已经被改成 `running` 且
    /// `attempts` 已加一，下一拍又因为 `state != 'pending'` 拿不到它 —— 这条作业就卡在
    /// `running` 里，只能等下一次进程启动的 `resume_interrupted_jobs` 救回来。
    pub async fn tick_jobs(&self, now: hbot_core::Timestamp) -> Result<(), HbotError> {
        // 没声明作业类型就直接走：与定时器同一条理由（别给每个 bot 加一份每秒一次的
        // 编译 + isolate 开销）。
        if declared_jobs(&self.project_root).is_empty() {
            return Ok(());
        }
        let (fingerprint, project) = match self.compiled().await {
            Ok(pair) => pair,
            Err(error) => {
                // 编译失败不在这里重试：下一拍照样会走一遍，用户改好代码即恢复。
                tracing::warn!(bot = %self.bot_id, error = %error, "作业 tick 无法取到项目");
                return Ok(());
            }
        };
        let claimed = self.claim_jobs(now).await?;
        for row in claimed {
            self.run_claimed_job(&fingerprint, &project, row, now).await;
        }
        Ok(())
    }

    /// 认领本 bot 的到期作业（一步 SQL：挑 + 改 `running + `attempts + 1）。
    ///
    /// **只能走 `claim_due_jobs_for_bot**：全局 `claim_due_jobs 会让 A 把 B 的作业抢走
    /// 并在 A 的 isolate 里跑 —— 跨 bot 隔离当场破，而且日志里看不出任何异常（名字对、
    /// 载荷对，只有上下文错）。
    async fn claim_jobs(
        &self,
        now: hbot_core::Timestamp,
    ) -> Result<Vec<hbot_store::JobRow>, HbotError> {
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let at = now.unix_millis();
        tokio::task::spawn_blocking(move || {
            store.claim_due_jobs_for_bot(&bot_id, at, JOB_CLAIM_LIMIT)
        })
        .await
        .map_err(|error| {
            HbotError::internal(format!(
                "认领到期作业的任务失败（bot {}）：{error}",
                self.bot_id
            ))
        })?
        .map_err(store_failed)
    }

    /// 跑一条已经认领的作业，并结算它的终态（`done / 重试 / `failed）。
    async fn run_claimed_job(
        &self,
        fingerprint: &str,
        project: &Arc<CompiledProject>,
        row: hbot_store::JobRow,
        now: hbot_core::Timestamp,
    ) {
        let name = row.name.clone();
        // 两个 crate 各有一个 `JobId 新类型（同底层 Uuid，不同类型）：走 `from_uuid
        // 而不是字符串往返 —— 主键是库里读出来的，绕一圈 `to_string 只会多一处可能
        // 悄悄变形的环节。
        let job_id = hbot_core::JobId::from_uuid(*row.id.as_uuid());
        let payload = match serde_json::from_str::<Value>(&row.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                // 载荷坏了重试多少次都是坏的：直接落 failed，别让它每秒重跑一遍。
                let message = format!(
                    "作业 `{name}`（{job_id}）的载荷不是合法 JSON（payload_json 已损坏）：{error}"
                );
                self.fail_job(&row, &message, now, false).await;
                return;
            }
        };
        // 作业没有"会话"这个稳定键，用作业主键映射到 worker：同一条作业的重试落在
        // 同一个 isolate 上，跨重启也成立（主键落库了）。
        let index = stable_index(&job_id.to_string(), self.workers.len());
        let outcome = self
            .dispatch_job_on(index, fingerprint, project, &row, job_id, payload)
            .await;
        match outcome {
            Ok(outcome) => {
                self.record_logs(outcome.logs).await;
                if let Err(error) = self.deliver_job_replies(&row, outcome.replies).await {
                    // 回复没进 outbox 等于丢了：留痕，但不改终态 —— 作业体本身跑成功了，
                    // 把它标成 failed 会让它重跑一遍（副作用翻倍）。
                    tracing::warn!(
                        bot = %self.bot_id,
                        job = %name,
                        error = %error,
                        "作业回复未能进入 outbox"
                    );
                    self.record_error_line(format!("作业 `{name}` 的回复没有发出去：{error}"))
                        .await;
                }
                self.finish_job(&row, now).await;
            }
            Err(failure) => {
                // 失败路径也要落日志：`ctx.log` 在抛出前写的那几行是排障的唯一线索。
                let mapped = remap_error(&failure.error, project);
                self.record_logs(failure.logs).await;
                self.fail_job(&row, &mapped.to_string(), now, true).await;
            }
        }
    }

    /// 在**指定的** worker 上派发一条作业。
    ///
    /// 与 [`Self::dispatch_timer_on] 同形：预算、日志收集、失败形状都由 `hbot-code`
    /// 的 [`Sandbox::dispatch_job_collecting] 负责，这里只负责"在哪个 isolate 上跑"。
    async fn dispatch_job_on(
        &self,
        index: usize,
        fingerprint: &str,
        project: &Arc<CompiledProject>,
        row: &hbot_store::JobRow,
        job_id: hbot_core::JobId,
        payload: Value,
    ) -> Result<hbot_code::EventOutcome, hbot_code::EventFailure> {
        let mut guard = self
            .worker(index, fingerprint, project)
            .await
            .map_err(|error| {
                hbot_code::EventFailure::without_output(hbot_code::CodeError::SandboxDead {
                    message: error.to_string(),
                })
            })?;
        let Some(worker) = guard.as_mut() else {
            return Err(hbot_code::EventFailure::without_output(
                hbot_code::CodeError::SandboxDead {
                    message: format!(
                        "作业 `{}` 的 isolate 槽位 {index} 在派发前被清空（bot {}）",
                        row.name, self.bot_id
                    ),
                },
            ));
        };
        let sandbox = Arc::clone(&worker.sandbox);
        // budget_ms 取**行里冻结的那一份**，不重读清单：改清单不该让已经排队的作业
        // 换预算（换语义是排障时最费时间的一类问题）。
        let budget_ms = row.budget_ms;
        let name = row.name.clone();
        let key = row.idempotency_key.clone();
        let conversation = row.conversation.clone();
        tokio::task::spawn_blocking(move || {
            sandbox.dispatch_job_collecting(job_id, budget_ms, &name, payload, &key, conversation)
        })
        .await
        .map_err(|error| {
            hbot_code::EventFailure::without_output(hbot_code::CodeError::SandboxDead {
                message: format!("作业派发任务失败：{error}"),
            })
        })?
    }

    /// 标成功：落 `done 终态。
    async fn finish_job(&self, row: &hbot_store::JobRow, now: hbot_core::Timestamp) {
        let store = Arc::clone(&self.store);
        let id = row.id;
        let at = now.unix_millis();
        let written = tokio::task::spawn_blocking(move || store.mark_job_done(&id, at)).await;
        match written {
            Err(error) => {
                tracing::error!(bot = %self.bot_id, error = %error, "标记作业成功的任务失败");
            }
            Ok(Err(error)) => {
                tracing::error!(bot = %self.bot_id, error = %store_failed(error), "标记作业成功失败");
            }
            Ok(Ok(false)) => {
                // 行不在了（被并发改成终态）：留痕，不静默吞。
                tracing::warn!(
                    bot = %self.bot_id,
                    job = %row.name,
                    "作业跑完了但库里已经没有这一行（可能被并发取消）"
                );
            }
            Ok(Ok(true)) => {}
        }
    }

    /// 结算一次失败：按声明表决定"重试一次"还是"落 `failed"。
    ///
    /// `retryable = false 用于载荷损坏这类"再跑一次也是同样的坏"的失败。
    async fn fail_job(
        &self,
        row: &hbot_store::JobRow,
        message: &str,
        now: hbot_core::Timestamp,
        retryable: bool,
    ) {
        let name = row.name.clone();
        // 重试次数取**清单里现读的那一份**：retries 是"这个作业类型失败几次后放弃"，
        // 属于策略而不是这一次执行的快照（预算反过来，预算按行冻结）。
        let retries = declared_jobs(&self.project_root)
            .iter()
            .find(|spec| spec.name == name)
            .map_or(0, |spec| spec.retries);
        // attempts 在认领时已经加过了，所以它现在的值就是"这一次是第几次"。
        if retryable && row.attempts <= retries {
            let delay = hbot_core::retry_delay_ms(row.attempts);
            let next_at = now
                .unix_millis()
                .saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
            let store = Arc::clone(&self.store);
            let id = row.id;
            let at = now.unix_millis();
            let error = message.to_owned();
            let written =
                tokio::task::spawn_blocking(move || store.reschedule_job(&id, next_at, &error, at))
                    .await;
            match written {
                Err(error) => {
                    tracing::error!(bot = %self.bot_id, error = %error, "排下一次作业重试的任务失败");
                }
                Ok(Err(error)) => {
                    tracing::error!(bot = %self.bot_id, error = %store_failed(error), "排作业重试失败");
                }
                Ok(Ok(_)) => {}
            }
            self.record_error_line(format!(
                "作业 `{name}` 第 {} 次执行失败，{delay} 毫秒后重试（本类型最多重试 {retries} 次）：{message}",
                row.attempts
            ))
            .await;
            return;
        }

        let store = Arc::clone(&self.store);
        let id = row.id;
        let at = now.unix_millis();
        let error = message.to_owned();
        let written =
            tokio::task::spawn_blocking(move || store.mark_job_failed(&id, &error, at)).await;
        match written {
            Err(error) => {
                tracing::error!(bot = %self.bot_id, error = %error, "标记作业失败的任务失败");
            }
            Ok(Err(error)) => {
                tracing::error!(bot = %self.bot_id, error = %store_failed(error), "标记作业失败失败");
            }
            Ok(Ok(_)) => {}
        }
        self.record_error_line(format!(
            "作业 `{name}` 执行失败（第 {} 次，不再重试）：{message}",
            row.attempts
        ))
        .await;
    }

    /// 把作业产出的回复交给 supervisor 的 outbox 通道。
    ///
    /// 目标会话从库里按 `(bot_id, external_id) 查，与 [`Self::deliver_timer_replies]
    /// 同一口径：**会话类型不猜**，查不到就报错（由调用方写成一行 error 日志），不静默丢弃。
    async fn deliver_job_replies(
        &self,
        row: &hbot_store::JobRow,
        replies: Vec<String>,
    ) -> Result<(), HbotError> {
        if replies.is_empty() {
            return Ok(());
        }
        let Some(external_id) = row.conversation.clone() else {
            return Err(HbotError::invalid(format!(
                "作业 `{}` 产出了 {} 条回复，但入队时没有写 options.conversation：\
                 没有目标会话就不发（猜一个群发出去收不回来）",
                row.name,
                replies.len()
            )));
        };
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let lookup = external_id.clone();
        let conversation = tokio::task::spawn_blocking(move || {
            store.get_conversation_by_external(&bot_id, &lookup)
        })
        .await
        .map_err(|error| HbotError::internal(format!("查作业目标会话的任务失败：{error}")))?
        .map_err(store_failed)?;
        let Some(found) = conversation else {
            return Err(HbotError::invalid(format!(
                "作业 `{}` 的目标会话 `{external_id}` 还不存在（这个会话从没收到过消息）：\
                 先让它收到一条消息，或在入队时写对 externalId",
                row.name
            )));
        };
        let target = hbot_bot::ConversationRef::new(
            found.external_id.clone(),
            crate::app::runtime::conversation_kind_from_str(&found.kind),
        );
        for text in replies {
            let request = OutboundRequest {
                conversation: target.clone(),
                text,
                // 主动消息：作业背后没有可引用的历史事件，**不得**冒用旧 ID。
                reply_to: None,
            };
            self.replies.send(request).await.map_err(|_| {
                HbotError::internal(format!(
                    "回复通道已关闭（bot {}）：supervisor 已停止，这条作业回复没有进入 outbox",
                    self.bot_id
                ))
            })?;
        }
        Ok(())
    }

    /// 派发一次 webhook 请求，返回 HTTP 应答。
    ///
    /// 与 {BT}dispatch_job_on{BT} 同形（编译 → 取 worker 槽 → {BT}spawn_blocking{BT}），但**没有重试、
    /// 没有落库**：调用方是一个在等 HTTP 响应的外部系统，一次请求就是一次事件。
    ///
    /// 三条错误各有各的归宿（见 {BT}hbot_core::bot_webhook::WebhookError{BT}）：
    ///
    /// * 路由不在清单里 → {BT}NoRoute{BT}（404）；
    /// * 没有 isolate（bot 没起来、槽位被清空）→ {BT}Unavailable{BT}（503）；
    /// * handler 抛错 / 超时 → {BT}Failed{BT} / {BT}Timeout{BT}（500 / 504）。
    ///
    /// 日志无论成败都落库：外部系统看不到 {BT}ctx.log{BT}，它是唯一排障线索。
    pub async fn dispatch_webhook(
        &self,
        route: &str,
        method: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> Result<hbot_core::WebhookResponse, hbot_core::WebhookError> {
        if !declared_webhook(&self.project_root)
            .iter()
            .any(|declared| declared.path == route)
        {
            return Err(hbot_core::WebhookError::NoRoute {
                route: route.to_owned(),
            });
        }
        let (fingerprint, project) =
            self.compiled()
                .await
                .map_err(|error| hbot_core::WebhookError::Unavailable {
                    reason: error.to_string(),
                })?;
        // 外部请求没有会话可做稳定哈希，用路由名映射到 worker：同一条路由总落在同一个
        // isolate 上，与 {BT}run_claimed_job{BT} 用作业主键是同一手法。
        let index = stable_index(route, self.workers.len());
        let mut guard = self
            .worker(index, &fingerprint, &project)
            .await
            .map_err(|error| hbot_core::WebhookError::Unavailable {
                reason: error.to_string(),
            })?;
        let Some(worker) = guard.as_mut() else {
            return Err(hbot_core::WebhookError::Unavailable {
                reason: format!("isolate 槽位 {index} 在派发前被清空（bot {}）", self.bot_id),
            });
        };
        let sandbox = Arc::clone(&worker.sandbox);
        let route_owned = route.to_owned();
        let method_owned = method.to_owned();
        let content_type_owned = content_type.map(str::to_owned);
        let body_owned = body.to_owned();
        let dispatched = tokio::task::spawn_blocking(move || {
            sandbox.dispatch_webhook_collecting(
                &route_owned,
                &method_owned,
                content_type_owned.as_deref(),
                &body_owned,
            )
        })
        .await
        .map_err(|error| hbot_core::WebhookError::Failed {
            message: format!("webhook 派发任务失败：{error}"),
        })?;

        match dispatched {
            Ok((outcome, response)) => {
                lock(&self.hot).crashes.record_success();
                self.record_logs(outcome.logs).await;
                // 返回值缺失 = handler 没回答（undefined/null）→ 204。契约里这是合法形状。
                match response {
                    Some(value) => hbot_core::response_from_handler(&value),
                    None => Ok(hbot_core::WebhookResponse::no_content()),
                }
            }
            Err(failure) => {
                let mapped = remap_error(&failure.error, &project);
                let crash = is_crash_class(&failure.error);
                let dead = if crash { guard.take() } else { None };
                drop(guard);
                self.record_logs(failure.logs).await;
                if let Some(dead) = dead {
                    release_isolate(dead).await;
                }
                if crash {
                    self.record_crash(&fingerprint).await;
                }
                self.record_error_line(format!("路由 `{route}` 处理失败：{mapped}"))
                    .await;
                Err(
                    if matches!(*failure.error, hbot_code::CodeError::Timeout { .. }) {
                        hbot_core::WebhookError::Timeout
                    } else {
                        hbot_core::WebhookError::Failed {
                            message: mapped.to_string(),
                        }
                    },
                )
            }
        }
    }
}

/// 这条消息的回复应该挂在哪个平台消息 ID 上。
///
/// 有平台消息 ID 时才返回 `Some`，否则返回 `None`（降级为**不带引用**的普通回复）。
/// 两条"`external_id` 不是平台消息 ID"的路：
///
/// 1. **调试注入**（`bot_inject`）：不是平台事件，`external_id` 是合成的去重键
///    （`inject-…`），拿去当 `reply_to` 会被平台拒发。判定依据是注入方写进 payload 的
///    `injected` 标记（见 `crate::app::commands::bots::inject`），另有 `inject-` 前缀兜底。
/// 2. **平台没给 `message_id`**：适配器会用兜底幂等键
///    `"{user_id}-{time}-{fnv1a64:016x}"` 填 `external_id`
///    （`hbot_adapters::platforms::onebot::normalize`）。那个键**只是本地的去重键**，
///    不是 OneBot `message_id`；拿它组 `{"type":"reply","data":{"id":"111-…-…"}}`
///    发给真网关，整条消息会被判参数非法而**发不出去**（"能回"变"回不出去"），
///    比"少一个引用"糟得多。所以按形状识别出来就降级为主动回复。
///
/// 判别采用**兜底键形状**而不是"必须是纯数字"，因为 `external_id` 不只有 OneBot：
/// QQ 官方适配器的 `external_id` 是 `ROBOT1.0_…`（非纯数字，见
/// `hbot_adapters::platforms::qq::normalize`），要求纯数字会把 QQ 的被动回复全打死；
/// 而本仓库既有断言（`replies_carry_the_platform_message_id`）用的也是 `msg-777`
/// 这种非数字 id。形状识别同时保住三边：OneBot 真 message_id（i64，纯数字）、
/// QQ 官方 id（`ROBOT1.0_…`）、兜底键（`数字-数字-16 位十六进制`）。
fn passive_reply_id(ctx: &MessageCtx) -> Option<String> {
    let injected = ctx
        .payload()
        .get("injected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if injected {
        return None;
    }
    let external_id = ctx.external_id();
    if is_synthetic_external_id(external_id) {
        return None;
    }
    Some(external_id.to_owned())
}

/// `external_id` 是不是"本地合成的去重键"（而不是平台侧的消息 id）。
///
/// **形状判据只有一份，在适配器里**：`hbot_adapters::platforms::onebot::
/// is_synthetic_external_id` 与 `fallback_external_id` 的格式串相邻，改一处必须改另一处
/// （那边有往返测试 `synthetic_id_judgement_round_trips_the_fallback_format` 钉住）。
/// 这里只补两件**本层**才知道的合成键，不复制形状判断：
///
/// 1. `bot_inject` 的 `inject-…`（正常情况下 payload 里的 `injected` 标记已经拦住，
///    这里按前缀再兜一次，避免"标记丢了但键明显是合成的"时把合成键发回平台）；
/// 2. 空串——没有引用目标可言。
fn is_synthetic_external_id(external_id: &str) -> bool {
    external_id.trim().is_empty()
        || external_id.starts_with("inject-")
        || hbot_adapters::platforms::onebot::is_synthetic_external_id(external_id)
}

/// 把一个（崩溃的）worker 在阻塞线程里释放。
///
/// 必须整包 move 进 `spawn_blocking`：`Worker` 里那个 `Arc<Sandbox>` 的最后一次 drop
/// 会 join 隔离线程，在 reactor 线程上做会卡住整个运行时。
async fn release_isolate(worker: Worker) {
    if let Err(error) = tokio::task::spawn_blocking(move || drop(worker)).await {
        tracing::error!(error = %error, "崩溃 isolate 回收任务失败");
    }
}

#[async_trait]
impl HandlerExecutor for SandboxHandler {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        match self.run(&ctx).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // 失败必须留痕：supervisor 只会把 inbound 留在"未处理"并累计连续失败，
                // 它不写日志表。这里补一条 error 日志 + RunLog，`logs_tail` 才能回答
                // "为什么这条消息没被处理"。
                self.record_failure(&ctx, &error).await;
                Err(error)
            }
        }
    }
}

impl SandboxHandler {
    /// 这条消息是否唤醒了一个等待；命中就返回它（`claim` 是一条 SQL，改状态与返回同一句）。
    ///
    /// 没装配等待端口、或清单没声明 `state.session` 时直接返回 `None`：那是「这个项目
    /// 不用等待」，不是错误。
    async fn claim_wait(&self, ctx: &MessageCtx) -> Result<Option<hbot_core::WaitHit>, HbotError> {
        if self.waits.is_none() && !declared_session(&self.project_root) {
            return Ok(None);
        }
        if !declared_session(&self.project_root) {
            return Ok(None);
        }
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id;
        let conversation_id = ctx.conversation_id();
        let sender_id = ctx.message.sender_id.clone();
        let message_id = ctx.message_id().to_string();
        let at = self.clock.now().unix_millis();
        let hit = tokio::task::spawn_blocking(move || {
            store.claim_wait(&bot_id, &conversation_id, &sender_id, &message_id, at)
        })
        .await
        .map_err(|error| {
            HbotError::internal(format!(
                "唤醒等待的任务失败（bot {}）：{error}",
                self.bot_id
            ))
        })?;
        let hit = hit.map_err(store_failed)?;
        Ok(hit.map(|row| hbot_core::WaitHit {
            id: row.id,
            key: row.key,
            prompt: row.prompt,
            owner: row.owner,
        }))
    }

    /// 跑一次 `onResume(ctx, message)`：等待被这条消息唤醒之后的路。
    ///
    /// 与 `on_message` 的差别只有钩子和 ctx：预算、日志落库、回复交付、崩溃预算都走
    /// 同一套（回复同样经 `replies` 通道进 outbox，`reply_to` 恒为 `None` —— 主动
    /// 消息不得冒用旧事件）。
    async fn resume(
        &self,
        ctx: &MessageCtx,
        event: &serde_json::Value,
        hit: hbot_core::WaitHit,
    ) -> Result<(), HbotError> {
        let payload = event.to_string();
        let conversation = ctx.conversation.external_id.clone();
        let (fingerprint, project) = self.compiled().await?;
        let index = stable_index(&ctx.conversation_id().to_string(), self.workers.len());
        let mut guard = self.worker(index, &fingerprint, &project).await?;
        let sandbox = guard
            .as_ref()
            .map(|worker| Arc::clone(&worker.sandbox))
            .ok_or_else(|| HbotError::internal("worker 槽位在持有期间被清空"))?;
        let wait_key = hit.key.clone();
        let dispatched = tokio::task::spawn_blocking(move || {
            sandbox.dispatch_resume_collecting(&payload, &hit.key, &hit.prompt, Some(conversation))
        })
        .await
        .map_err(|error| {
            HbotError::internal(format!("恢复派发任务失败（bot {}）：{error}", self.bot_id))
        })?;

        match dispatched {
            Ok(outcome) => {
                lock(&self.hot).crashes.record_success();
                self.record_logs(outcome.logs).await;
                // 恢复事件的 `reply_to` 恒为 `None`：这条回复回答的是**之前**那次提问，
                // 冒用当前消息的 ID 会把它绑到一条错误的引用上。
                self.deliver_resume_replies(ctx, outcome.replies).await?;
                Ok(())
            }
            Err(failure) => {
                let mapped = remap_error(&failure.error, &project);
                let crash = is_crash_class(&failure.error);
                // 崩溃类的 isolate 已经不可用：清空槽位后再去阻塞线程里 drop。
                let dead = if crash { guard.take() } else { None };
                drop(guard);
                self.record_logs(failure.logs).await;
                if let Some(dead) = dead {
                    release_isolate(dead).await;
                }
                if crash {
                    self.record_crash(&fingerprint).await;
                }
                tracing::warn!(
                    bot = %self.bot_id,
                    wait = %wait_key,
                    error = %mapped,
                    "恢复钩子执行失败"
                );
                Err(mapped)
            }
        }
    }

    /// 把恢复产出的回复交给 outbox（`reply_to = None`）。
    async fn deliver_resume_replies(
        &self,
        ctx: &MessageCtx,
        replies: Vec<String>,
    ) -> Result<(), HbotError> {
        for text in replies {
            let request = OutboundRequest {
                conversation: ctx.conversation.clone(),
                text,
                reply_to: None,
            };
            self.replies.send(request).await.map_err(|_| {
                HbotError::internal(format!(
                    "回复通道已关闭（bot {}）：supervisor 已停止，这条恢复回复没有进入 outbox",
                    self.bot_id
                ))
            })?;
        }
        Ok(())
    }
    /// 一条消息的完整处理：编译 → 取 worker → dispatch → 日志 → 回复。
    async fn run(&self, ctx: &MessageCtx) -> Result<(), HbotError> {
        // 事件对象的唯一拼装在 `hbot-bot::event_json`（生产与回放共用）。
        let event = event_json(&EventSource {
            bot_id: ctx.bot_id,
            conversation_id: ctx.conversation_id(),
            message_id: ctx.message_id(),
            external_id: ctx.external_id(),
            sender_id: &ctx.message.sender_id,
            text: ctx.text(),
            kind: ctx.conversation_kind(),
            conversation_external_id: &ctx.conversation.external_id,
            received_at: ctx.received_at(),
            payload: ctx.payload(),
        });
        // **唤醒优先于 onMessage**：这条消息可能正是某个等待在等的那一句。
        //
        // 顺序不能反：先跑 `onMessage` 会让「用户在回答一个提问」与「用户发起新话题」
        // 走同一条路，而这两件事该做的事通常相反。命中之后就**不再**叫 `onMessage` ——
        // 一次唤醒只该唤醒一次（`claim_wait` 是一条 SQL，所以重投的消息也不会双唤醒）。
        if let Some(hit) = self.claim_wait(ctx).await? {
            return self.resume(ctx, &event, hit).await;
        }
        let conversation_id = ctx.conversation_id();
        let worker_index = stable_index(&conversation_id.to_string(), self.workers.len());

        let (fingerprint, project) = self.compiled().await?;
        let mut guard = self.worker(worker_index, &fingerprint, &project).await?;
        let sandbox = guard
            .as_ref()
            .map(|worker| Arc::clone(&worker.sandbox))
            .ok_or_else(|| HbotError::internal("worker 槽位在持有期间被清空"))?;

        let started = Instant::now();
        let bot_id = self.bot_id;
        let debug = Arc::clone(&self.debug);
        let events = Arc::clone(&self.events);
        // **没有断点就不 arm**：`dispatch_with_debug(Some(sink))` 会让隔离区里的
        // `__hbot_dbg` 为真，每条语句都走一次探针（probe-dense handler 实测 4.64×）。
        // 平时不给 sink，探针快路径退化成一次全局读（≈release），断点是"要用才付钱"。
        let has_breakpoints = !self.debug.breakpoints_of(&bot_id).is_empty();
        let dispatched = tokio::task::spawn_blocking(move || {
            // 守卫活到 dispatch 结束：调用方在 await 期间一直持有槽位锁，所以这里不会
            // 有第二个 dispatch 抢同一个 isolate（见本模块第 2 条硬约束）。
            //
            // **sink 必须在 blocking 线程里构造并使用**：`DebugSink::paused` 会同步阻塞
            // 到用户在 UI 点"继续"或 5 分钟超时。放到 reactor 线程上就绪会被钉死 5 分钟
            // （症状是整个应用没反应，而不是这个 bot 不回话）——见 `app::debug::sink`
            // 的模块文档「线程约束」。
            if !has_breakpoints {
                // `*_collecting`：失败路径也要拿回 handler 已经写出的 `ctx.log` 行，
                // 否则用户看到的是"我的 bot 出错了，但出错前那三行 log 一条都没有"。
                return sandbox.dispatch_collecting(&event);
            }
            let sink = BotDebugSink::new(bot_id, debug, events);
            let sink: Option<&dyn hbot_core::debug::DebugSink> = Some(&sink);
            sandbox.dispatch_with_debug_collecting(&event, sink)
        })
        .await
        .map_err(|error| {
            HbotError::internal(format!("dispatch 任务失败（bot {}）：{error}", self.bot_id))
        })?;
        let outcome = match dispatched {
            Ok(outcome) => {
                drop(guard);
                lock(&self.hot).crashes.record_success();
                outcome
            }
            Err(failure) => {
                let mapped = remap_error(&failure.error, &project);
                let crash = is_crash_class(&failure.error);
                // 崩溃类的 isolate 已经不可用：必须丢掉，否则下一条消息继续撞同一个死运行时。
                // 先把槽位清空、放开槽位锁，再去阻塞线程里 drop（isolate 的 Drop 会 join
                // 它的 OS 线程，不能在 reactor 线程上做，也不能持锁做）。
                let dead = if crash { guard.take() } else { None };
                drop(guard);
                // **失败路径也要落日志**，而且必须排在错误行之前：`on_message` 在
                // `run` 返回 Err 之后才写 "消息 … 处理失败" 那行，这里先 await 完
                // `record_logs`，日志表与 `RunLog` 流里"出错前打的那几行"就天然在前，
                // 用户读到的因果顺序才是对的。
                self.record_logs(failure.logs).await;
                if let Some(dead) = dead {
                    release_isolate(dead).await;
                }
                if crash {
                    self.record_crash(&fingerprint).await;
                }
                tracing::warn!(
                    bot = %self.bot_id,
                    conversation = %conversation_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    error = %mapped,
                    "handler 执行失败"
                );
                return Err(mapped);
            }
        };

        let reply_count = outcome.replies.len();
        // 先落日志（诊据），再交付回复：回复进 outbox 后 RecordingHandler 才会标记
        // inbound 已处理，所以这两步的顺序是 "证据先于结果"。
        self.record_logs(outcome.logs).await;
        self.deliver_replies(ctx, outcome.replies).await?;

        tracing::debug!(
            bot = %self.bot_id,
            conversation = %conversation_id,
            replies = reply_count,
            elapsed_ms = started.elapsed().as_millis(),
            "handler 执行完成"
        );
        Ok(())
    }

    /// 把一次失败写成一行 `error` 日志，并推 `RunLog`（供 `logs_tail` 与 UI 查看）。
    async fn record_failure(&self, ctx: &MessageCtx, error: &HbotError) {
        self.record_error_line(format!("消息 {} 处理失败：{error}", ctx.message_id()))
            .await;
    }
}

impl std::fmt::Debug for SandboxHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxHandler")
            .field("bot_id", &self.bot_id)
            .field("project_root", &self.project_root)
            .field("isolates", &self.workers.len())
            .field("limits", &self.limits)
            .field("compiles", &self.compile_count())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 清单里的设备应用白名单（`permissions.device.apps`），已归一化（trim + 小写 + 去重）。
///
/// 与 `crate::app::device::DeviceRegistry::declared_apps` **同一份口径**，但这里不能复用：
/// 那个方法在闸（`AppState` 侧）上，而装沙箱发生在 `spawn_blocking` 里、手上只有项目根。
///
/// 读不到清单 / 没写 `device` 段 → 空表。空表在设备后端就是"任何 `input` 目标都不放"
/// （fail-closed），与 `ctx.device.screen()` 无关（截屏不需要应用白名单）。
fn device_apps(root: &std::path::Path) -> Vec<String> {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return Vec::new();
    };
    let Some(device) = manifest.permissions.device else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::with_capacity(device.apps.len());
    for app in device.apps {
        let app = app.trim().to_ascii_lowercase();
        if app.is_empty() || out.contains(&app) {
            continue;
        }
        out.push(app);
    }
    out
}

/// 清单里声明的定时计划（`capabilities.timer`），读不到就是空表。
///
/// 与 [`device_apps`] 同一口径：清单是项目文件，改它除了指纹没有别的信号，所以每次
/// 起 isolate 时现读一遍。空表 = 没有定时器，tick 循环据此**不编译、不建 isolate**。
fn declared_schedules(root: &std::path::Path) -> Vec<hbot_core::ScheduleSpec> {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return Vec::new();
    };
    manifest.capabilities.timer().to_vec()
}

/// 会话 → worker 的稳定映射（FNV-1a，非加密；只要求同会话恒定）。
fn stable_index(key: &str, modulo: usize) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if modulo == 0 {
        0
    } else {
        usize::try_from(hash % modulo as u64).unwrap_or(0)
    }
}

/// isolate 起不来：没有隔离就不执行代码（§6.3），错误码按原因区分。
fn sandbox_start_error(error: CodeError) -> HbotError {
    match error {
        CodeError::MemoryLimit { .. } | CodeError::Timeout { .. } => {
            HbotError::invalid(error.to_string())
        }
        other => HbotError::internal(other.to_string()),
    }
}

/// 这次失败算不算"isolate 崩了"（要计崩溃预算、要丢弃 isolate）。
///
/// 只有运行时级的死亡算：超时被中断、内存超限、进程级死亡。用户代码抛异常（`JsException`）
/// 是**消息级**失败，同一个 isolate 还能继续服务，不该被计进崩溃预算。
fn is_crash_class(error: &CodeError) -> bool {
    matches!(
        error,
        CodeError::SandboxDead { .. } | CodeError::Timeout { .. } | CodeError::MemoryLimit { .. }
    )
}

/// dispatch 期错误 → wire 错误码。
///
/// 只做三件事：**超时 → -32004**（可重试的运行时问题）、**进程级死亡 → -32006**、
/// 其余（语法/JS 异常/内存超限）→ -32002。不要把它们都塞进 internal，否则 UI 分不出
/// "用户代码写错了"和"平台坏了"。
fn remap_error(error: &CodeError, project: &CompiledProject) -> HbotError {
    // 按引用匹配：`JsException` 里的 `stack` 要借用去重映射栈，move 出来会让
    // `error` 变成部分移动，后面 `error.to_string()` 就用不了了。
    match error {
        CodeError::Timeout { .. } => timeout_error(error),
        CodeError::SandboxDead { .. } => HbotError::internal(error.to_string()),
        CodeError::JsException {
            stack: Some(stack), ..
        } => {
            let remapped = project.remap_stack(stack);
            HbotError::invalid(format!("{error}\n{remapped}"))
        }
        _ => HbotError::invalid(error.to_string()),
    }
}

/// 超时 → `-32004`，**保留 `hbot-code` 的 `HBOT_TIMEOUT` 机器标识**。
///
/// 为什么不能只用 `HbotError::timeout("sandbox.dispatch", ms)`：那句的 Display 是
/// `timeout after 4000ms: sandbox.dispatch`，运维按 `HBOT_TIMEOUT` grep 超时会一条都
/// 搜不到（真正的原因文案在 `crates/hbot-code/src/error.rs` 的
/// `CodeError::Timeout`，是 `HBOT_TIMEOUT: handler exceeded the …`）。
///
/// 所以把原始的 `CodeError` 文案整段括在操作名后面：
/// `timeout after 4000ms: sandbox.dispatch (HBOT_TIMEOUT: handler exceeded the 4000 ms
/// budget and was interrupted)`。这样四处都还能定位：
/// * `HBOT_TIMEOUT` —— 运维/告警的机器标识（本函数存在的唯一理由）；
/// * `timeout after` + 毫秒数 —— 老的 grep 与 `crates/hbot-parity` 的用例；
/// * `sandbox.dispatch` —— 还看得出是 dispatch 阶段的超时，不是编译期。
///
/// 文案不在这里重抄：`{error}` 走 `CodeError` 的 Display，那段文字的唯一真源在
/// `hbot-code`；在这边手写第二份会在对方改措辞时悄悄漂移。
fn timeout_error(error: &CodeError) -> HbotError {
    match error {
        CodeError::Timeout { budget_ms } => HbotError::Timeout {
            operation: format!("sandbox.dispatch ({error})"),
            timeout_ms: *budget_ms,
        },
        // 只有 `Timeout` 会进来（上面的匹配臂决定）。保守回退成 internal 而不是 panic：
        // 这条路径在"处理消息失败"的日志链上，辅助函数不该因为将来多一个变体就把
        // bot 打死。
        other => HbotError::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hbot_bot::ConversationRef;
    use hbot_core::{ConversationId, MessageId, Timestamp};
    use hbot_store::{Bot as StoreBot, Project, ProjectId};
    use serde_json::json;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::app::test_support::TempDir;

    /// 一版能跑的代码（回复 `v1`）。
    const GOOD_V1: &str = "import { defineBot } from 'hbot';\n\
                            export default defineBot({\n\
                              onMessage(ctx) {\n\
                                ctx.reply('v1');\n\
                              },\n\
                            });\n";

    /// 第二版能跑的代码（回复 `v2`）：用来验证"修好之后真的换到新版本"。
    const GOOD_V2: &str = "import { defineBot } from 'hbot';\n\
                            export default defineBot({\n\
                              onMessage(ctx) {\n\
                                ctx.reply('v2');\n\
                              },\n\
                            });\n";

    /// 编译期就失败：裸导入被 §6.1 的模块规则拒绝（`HBOT_BARE_IMPORT`）。
    const BROKEN: &str = "import lodash from 'lodash';\nexport default lodash;\n";

    /// 运行时死循环：只有超时中断能让它停下来，用来触发 isolate 崩溃预算。
    const SPIN: &str = "import { defineBot } from 'hbot';\n\
                        export default defineBot({\n\
                          onMessage() {\n\
                            for (;;) {}\n\
                          },\n\
                        });\n";

    /// 先写一行日志、再抛错：P2 缺陷的最小复现（旧实现里那行日志随 error 分支被丢掉）。
    const LOG_THEN_THROW: &str = "import { defineBot } from 'hbot';\n\
                                  export default defineBot({\n\
                                    onMessage(ctx) {\n\
                                      ctx.log('before the throw');\n\
                                      throw new Error('boom');\n\
                                    },\n\
                                  });\n";

    /// 每条消息的测试硬超时：handler 挂住时让用例 10 秒内红掉，而不是让 CI 无限期卡住。
    const MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

    /// 一个装配好的 handler 及其依赖（项目目录 + 真 SQLite + 回复通道）。
    struct Env {
        _dir: TempDir,
        root: PathBuf,
        store: Arc<Store>,
        events: Arc<EventBus>,
        bot_id: BotId,
        conversation_id: ConversationId,
        handler: Arc<SandboxHandler>,
        replies: mpsc::Receiver<OutboundRequest>,
    }

    impl Env {
        /// 默认夹具：默认 limits、元数据 TTL 归零（改文件立刻可见）、关闭去抖。
        fn new(label: &str, main_ts: &str) -> Self {
            Self::build(
                label,
                main_ts,
                Limits::new(),
                Duration::ZERO,
                Duration::from_secs(60),
                Duration::ZERO,
            )
        }

        fn build(
            label: &str,
            main_ts: &str,
            limits: Limits,
            metadata_ttl: Duration,
            content_floor: Duration,
            debounce: Duration,
        ) -> Self {
            let dir = TempDir::new(label);
            let root = dir.path().join("project");
            std::fs::create_dir_all(root.join("src")).expect("建 src 目录");
            std::fs::write(root.join("src").join("main.ts"), main_ts).expect("写 main.ts");

            let store = Arc::new(Store::open(dir.path().join("hbot.db")).expect("打开测试库"));
            let project_id = ProjectId::from_uuid(Uuid::from_u128(0x1000_0000_0000_0001));
            let bot_id = BotId::from_uuid(Uuid::from_u128(0x2000_0000_0000_0002));
            // 日志表有指向 `bots` 的外键：这两行必须先落库，否则 error 日志写不进去。
            store
                .insert_project(&Project {
                    id: project_id,
                    name: "test-project".to_owned(),
                    path: root.display().to_string(),
                    created_at: 1,
                })
                .expect("插入项目");
            store
                .insert_bot(&StoreBot {
                    id: bot_id,
                    project_id,
                    name: "test-bot".to_owned(),
                    platform: "onebot".to_owned(),
                    config_json: "{}".to_owned(),
                    enabled: true,
                    created_at: 1,
                })
                .expect("插入 bot");

            let events = Arc::new(EventBus::new());
            let (reply_tx, reply_rx) = mpsc::channel(REPLY_CAPACITY);
            let handler = Arc::new(
                SandboxHandler::new(SandboxHandlerConfig {
                    bot_id,
                    project_root: root.clone(),
                    limits,
                    store: Arc::clone(&store),
                    events: Arc::clone(&events),
                    clock: Arc::new(hbot_adapters::SystemClock),
                    replies: reply_tx,
                    // 测试夹具不装状态能力：`ctx.kv.*` 一律回 `HBOT_STATE_UNAVAILABLE`。
                    state: None,
                    // 同上：`ctx.ai.complete` 一律回 `HBOT_AI_UNAVAILABLE`。
                    ai: None,
                    // 本文件的用例都不依赖 `ctx.jobs`；不装端口时入队报
                    // `HBOT_JOBS_UNAVAILABLE`（明确的失败，不是静默收下）。
                    jobs: None,
                    waits: None,
                })
                .with_debounce(debounce)
                .with_fingerprint_ttls(metadata_ttl, content_floor),
            );

            Self {
                _dir: dir,
                root,
                store,
                events,
                bot_id,
                conversation_id: ConversationId::from_uuid(Uuid::from_u128(0xc0_0000_0000_0003)),
                handler,
                replies: reply_rx,
            }
        }

        /// 改写 `src/main.ts`（模拟用户在 IDE 里保存）。
        fn write_main(&self, contents: &str) {
            std::fs::write(self.root.join("src").join("main.ts"), contents).expect("改写 main.ts");
        }

        /// 造一条入站消息（`external_id` 就是平台消息 ID）。
        fn ctx(&self, external_id: &str, text: &str) -> MessageCtx {
            self.ctx_with_payload(external_id, json!({ "text": text }))
        }

        /// 造一条调试注入消息（`bot_inject` 的载荷：带 `injected` 标记）。
        fn injected_ctx(&self, external_id: &str, text: &str) -> MessageCtx {
            self.ctx_with_payload(external_id, json!({ "text": text, "injected": true }))
        }

        fn ctx_with_payload(&self, external_id: &str, payload: Value) -> MessageCtx {
            let message = hbot_core::InboundMessage {
                id: MessageId::from_uuid(Uuid::from_u128(0x3000_0000_0000_0004)),
                bot_id: self.bot_id,
                conversation_id: self.conversation_id,
                external_id: external_id.to_owned(),
                sender_id: "sender-1".to_owned(),
                payload,
                received_at: Timestamp::from_unix_millis(1_000),
                processed_at: None,
            };
            MessageCtx::new(
                self.bot_id,
                message,
                ConversationRef::new("chat-1", hbot_core::ConversationKind::Group),
            )
        }

        /// 处理一条消息，带**硬超时**：挂住必须变成带位置的失败，不能把 CI 拖成永远。
        ///
        /// 回归价值来自一次真实事故：旧版 `compiled()` 写成
        /// `match lock(&self.hot).last_good() { Some => … lock(&self.hot).mark_failed(…) }`，
        /// `match` 的临时守卫活到整个 match 结尾，于是同一线程对同一个 std Mutex 二次加锁
        /// → 编译失败并且已有上一版时**静默死锁**（用例 60 秒无输出、CPU 0%）。当时没有任何
        /// 超时，所以表现为 CI 无限期卡住；这层 10 秒超时让同类问题在下一次就红。
        async fn handle(&self, ctx: MessageCtx) -> Result<(), HbotError> {
            match tokio::time::timeout(MESSAGE_TIMEOUT, self.handler.on_message(ctx)).await {
                Ok(result) => result,
                Err(_) => panic!("on_message 超过 {MESSAGE_TIMEOUT:?} 没有返回：handler 挂住了"),
            }
        }

        /// 取走目前为止产出的全部回复。
        fn take_replies(&mut self) -> Vec<OutboundRequest> {
            let mut collected = Vec::new();
            while let Ok(request) = self.replies.try_recv() {
                collected.push(request);
            }
            collected
        }

        /// 日志表里以 `needle` 开头的 error 行数。
        ///
        /// 用"开头"而不是"包含"：消息失败那行也会带上编译错误原文
        /// （`消息 … 处理失败：生成代码编译失败…`），包含匹配会把它数进来。
        fn error_lines(&self, needle: &str) -> Vec<String> {
            self.store
                .list_logs(Some(&self.bot_id), 200)
                .expect("读日志")
                .into_iter()
                .filter(|row| row.level == "error" && row.line.starts_with(needle))
                .map(|row| row.line)
                .collect()
        }

        /// 已产出的回复文本。
        fn reply_texts(&mut self) -> Vec<String> {
            self.take_replies()
                .into_iter()
                .map(|request| request.text)
                .collect()
        }
    }

    /// D2：第一次编译就失败（没有任何可用版本）——消息确实处理不了，必须返回真错误，
    /// 让 inbound 保持未处理并在修好代码后重试。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_compile_failure_without_last_good_returns_error() {
        let mut env = Env::new("first-compile-fails", BROKEN);

        let error = env
            .handle(env.ctx("evt-1", "hi"))
            .await
            .expect_err("没有可用版本时必须返回真错误");

        assert_eq!(error.code(), -32002, "编译错误映射为 invalid：{error}");
        let text = error.to_string();
        assert!(
            text.contains("没有可用的已编译版本"),
            "错误文案必须说清「没有可用版本」：{text}"
        );
        assert!(
            text.contains("修好代码后会自动重试"),
            "错误文案必须说清「消息会重试」：{text}"
        );
        assert_eq!(
            env.error_lines("生成代码编译失败").len(),
            1,
            "编译失败必须写一条 error 日志给 logs_tail / UI"
        );
        assert!(
            env.reply_texts().is_empty(),
            "没有可用版本时不该产出任何回复"
        );
    }

    /// D2 核心：改坏代码后**继续用上一版服务**，消息不失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compile_failure_keeps_serving_last_good_version() {
        let mut env = Env::new("keep-last-good", GOOD_V1);

        env.handle(env.ctx("evt-1", "one"))
            .await
            .expect("第一版能跑");
        assert_eq!(env.reply_texts(), vec!["v1".to_owned()]);

        env.write_main(BROKEN);
        let before = env.handler.compile_count();
        env.handle(env.ctx("evt-2", "two"))
            .await
            .expect("一个错别字不许让每条消息都失败");
        assert_eq!(
            env.handler.compile_count(),
            before + 1,
            "新指纹要真的尝试编译一次"
        );
        assert_eq!(
            env.reply_texts(),
            vec!["v1".to_owned()],
            "编译失败时继续用上一版代码回复"
        );

        // 同一条坏代码再来一条消息：不许再编译（否则每条消息都在烧 CPU）。
        let after_typo = env.handler.compile_count();
        env.handle(env.ctx("evt-3", "three"))
            .await
            .expect("继续用上一版服务");
        assert_eq!(
            env.handler.compile_count(),
            after_typo,
            "同一份坏代码只编译一次，之后直接复用上一版"
        );
        assert_eq!(env.reply_texts(), vec!["v1".to_owned()]);
        assert_eq!(
            env.error_lines("生成代码编译失败").len(),
            1,
            "同一指纹的失败只记一次，不许每条消息都刷一行 error 日志"
        );
    }

    /// D2：改好代码后真的换到新版本（不是一直卡在上一版）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fixed_code_is_compiled_and_swapped_in() {
        let mut env = Env::new("swap-in", GOOD_V1);
        env.handle(env.ctx("evt-1", "one")).await.expect("v1");
        assert_eq!(env.reply_texts(), vec!["v1".to_owned()]);

        env.write_main(BROKEN);
        env.handle(env.ctx("evt-2", "two"))
            .await
            .expect("坏代码期间继续服务 v1");
        assert_eq!(env.reply_texts(), vec!["v1".to_owned()]);

        env.write_main(GOOD_V2);
        env.handle(env.ctx("evt-3", "three"))
            .await
            .expect("修好后应该编译成功");
        assert_eq!(
            env.reply_texts(),
            vec!["v2".to_owned()],
            "新指纹编译成功后必须换到新版本"
        );
    }

    /// D1：代码没变时，连续 N 条消息**一次文件内容读都不做**。
    ///
    /// 旧实现每条消息都 blake3 全项目源码；这里用 `content_reads` 计数器钉住回归
    /// （计数器是按"读了几个文件"累加的）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unchanged_code_is_not_reread_for_consecutive_messages() {
        // 元数据 TTL 取生产值（2s）+ 内容地板拉长到 60s：整轮用例落在一个"信任期"里，
        // 所以任何一次内容读都只可能来自热路径的旧实现。
        let env = Env::build(
            "no-content-reads",
            GOOD_V1,
            Limits::new(),
            super::super::fingerprint_cache::DEFAULT_METADATA_TTL,
            Duration::from_secs(60),
            Duration::ZERO,
        );

        env.handle(env.ctx("evt-0", "warmup"))
            .await
            .expect("首条消息需要编译一次");
        let baseline = env.handler.content_reads();
        assert_eq!(baseline, 1, "项目只有一个源文件，首轮应读一次内容");

        for index in 0..20 {
            env.handle(env.ctx(&format!("evt-{index}"), "hi"))
                .await
                .expect("代码没变，消息必须照常处理");
        }
        assert_eq!(
            env.handler.content_reads(),
            baseline,
            "20 条消息、代码没变：热路径不许再读任何文件内容"
        );
        assert_eq!(env.handler.compile_count(), 1, "没变就不该重编译");
    }

    /// D2：崩溃预算 —— 同一指纹连崩 3 次封顶，新指纹重置（纯状态机）。
    #[test]
    fn crash_budget_blocks_after_three_and_resets_on_new_fingerprint() {
        let mut budget = CrashBudget::default();
        assert!(!budget.record_crash("fp-a"), "第 1 次不该封顶");
        assert_eq!(budget.consecutive(), 1);
        assert!(!budget.record_crash("fp-a"), "第 2 次不该封顶");
        assert!(budget.record_crash("fp-a"), "第 3 次必须报告封顶");
        assert_eq!(budget.consecutive(), 3);
        assert!(budget.is_blocked("fp-a"), "封顶后该指纹不再自动重启");
        assert!(!budget.is_blocked("fp-b"), "封顶只对被封的那个指纹生效");

        assert!(
            !budget.record_crash("fp-b"),
            "新指纹是新的诊断对象：计数与封顶都清零"
        );
        assert_eq!(budget.consecutive(), 1);
        assert!(!budget.is_blocked("fp-b"));
        assert!(!budget.is_blocked("fp-a"), "换指纹后旧指纹的封顶也解除了");
    }

    /// D2：真跑 3 次 isolate 超时（`for(;;)` + 120ms 预算）→ 停止自动重启，第 4 条被拒绝。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn three_isolate_crashes_stop_auto_restart() {
        let limits = Limits::new().with_handler_timeout(Duration::from_millis(120));
        let env = Env::build(
            "crash-budget",
            SPIN,
            limits,
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::ZERO,
        );

        for attempt in 1..=CRASH_BUDGET_LIMIT {
            let error = match env.handle(env.ctx(&format!("evt-{attempt}"), "spin")).await {
                Ok(()) => panic!("第 {attempt} 次死循环必须超时"),
                Err(error) => error,
            };
            assert_eq!(
                error.code(),
                -32004,
                "第 {attempt} 次超时必须映射为 -32004：{error}"
            );
        }

        let error = env
            .handle(env.ctx("evt-after", "spin"))
            .await
            .expect_err("封顶后必须明确报错，不许静默重启");
        assert!(
            error.to_string().contains("已停止自动重启"),
            "第 4 条消息必须说明自动重启已停止：{error}"
        );
        assert_eq!(error.code(), -32006, "这是平台侧状态，不是用户代码错误");
        assert!(
            !env.error_lines("isolate 连续崩溃").is_empty(),
            "停止自动重启必须写 error 日志，不能只在内存里沉默"
        );
    }

    /// D2：封顶之后改代码（新指纹）自动恢复服务。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fresh_fingerprint_resets_the_crash_budget() {
        let limits = Limits::new().with_handler_timeout(Duration::from_millis(120));
        let mut env = Env::build(
            "crash-reset",
            SPIN,
            limits,
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::ZERO,
        );

        for attempt in 1..=CRASH_BUDGET_LIMIT {
            let error = env
                .handle(env.ctx(&format!("evt-{attempt}"), "spin"))
                .await
                .expect_err("死循环必须超时");
            assert_eq!(error.code(), -32004, "超时映射为 -32004：{error}");
        }
        assert!(
            env.handle(env.ctx("evt-blocked", "spin")).await.is_err(),
            "前提：此刻应该已经被封顶"
        );

        // 用户改代码 = 新指纹 = 新的诊断对象，预算必须重置。
        env.write_main(GOOD_V2);
        env.handle(env.ctx("evt-fixed", "hi"))
            .await
            .expect("新指纹必须重置崩溃预算并重新部署 isolate");
        assert_eq!(env.reply_texts(), vec!["v2".to_owned()]);
    }

    /// D2：连番保存（5 条消息同时发现缓存失效）只编译一次。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn debounce_coalesces_concurrent_compiles() {
        let env = Arc::new(Env::build(
            "debounce",
            GOOD_V1,
            Limits::new(),
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::from_millis(80),
        ));

        let mut tasks = Vec::new();
        for index in 0..5 {
            let handler = Arc::clone(&env.handler);
            let ctx = env.ctx(&format!("evt-{index}"), "hi");
            tasks.push(tokio::spawn(async move {
                match tokio::time::timeout(MESSAGE_TIMEOUT, handler.on_message(ctx)).await {
                    Ok(result) => result,
                    Err(_) => {
                        panic!("on_message 超过 {MESSAGE_TIMEOUT:?} 没有返回：handler 挂住了")
                    }
                }
            }));
        }
        for task in tasks {
            task.await
                .expect("任务不该 panic")
                .expect("每条消息都要被处理");
        }
        assert_eq!(
            env.handler.compile_count(),
            1,
            "并发/连番保存期间只许编译一次（300ms 去抖窗口）"
        );
    }

    /// D3：被动回复必须带上触发它的平台消息 ID。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replies_carry_the_platform_message_id() {
        let mut env = Env::new("reply-to", GOOD_V1);
        env.handle(env.ctx("msg-777", "hi"))
            .await
            .expect("处理成功");

        let replies = env.take_replies();
        assert_eq!(replies.len(), 1, "一条消息一个回复");
        assert_eq!(replies[0].text, "v1");
        assert_eq!(
            replies[0].reply_to.as_deref(),
            Some("msg-777"),
            "QQ/OneBot 被动回复靠这个 ID 组 reply 段并计入配额"
        );
    }

    /// D3：调试注入的 `external_id` 是合成去重键，不能当 `reply_to` 用（会被平台拒发）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn injected_messages_do_not_reuse_a_synthetic_id_as_reply_to() {
        let mut env = Env::new("injected", GOOD_V1);
        env.handle(env.injected_ctx("inject-abc", "hi"))
            .await
            .expect("注入消息照常处理");

        let replies = env.take_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].reply_to, None,
            "合成的 inject-… 不是平台消息 ID，只能按主动消息发"
        );
    }

    /// 平台没给 `message_id` 时，`external_id` 是适配器的兜底去重键，**不能**当引用目标。
    ///
    /// 这条是 parity 抓到的真缺陷：出站载荷长成
    /// `{"type":"reply","data":{"id":"111-1794790031-06032ba7feff3e72"}}`，
    /// 而 OneBot 的 `reply.data.id` 必须是 `message_id`（数字）。真网关收到这种段会判
    /// 参数非法、整条消息发不出去——比"少一个引用"严重。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fallback_dedupe_key_is_never_used_as_the_reply_target() {
        let mut env = Env::new("fallback-reply-to", GOOD_V1);
        // 形状与 `onebot::normalize::fallback_external_id` 一致：
        // `{user_id}-{time}-{fnv1a64:016x}`。
        env.handle(env.ctx("111-1794790031-06032ba7feff3e72", "hi"))
            .await
            .expect("无 message_id 的消息照样处理");

        let replies = env.take_replies();
        assert_eq!(replies.len(), 1, "一条消息一个回复");
        assert_eq!(
            replies[0].reply_to, None,
            "兜底去重键不是平台消息 ID：必须降级成不带引用的回复，否则真网关整条发不出去"
        );
        assert_eq!(replies[0].text, "v1", "降级只影响引用段，正文照发");
    }

    /// 非数字但**确实是**平台消息 ID 的形态（QQ 官方 `ROBOT1.0_…`）仍要走被动回复。
    ///
    /// 防止把上一条修成"仅纯数字才算平台 id"——那会打断 QQ 官方适配器的被动回复
    /// （它的 `external_id` 就是 `d.id`，`ROBOT1.0_…` 形态）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_platform_id_that_is_not_numeric_still_gets_a_passive_reply() {
        let mut env = Env::new("qq-form-reply-to", GOOD_V1);
        env.handle(env.ctx("ROBOT1.0_AbCdEf0123456789", "hi"))
            .await
            .expect("处理成功");

        let replies = env.take_replies();
        assert_eq!(
            replies[0].reply_to.as_deref(),
            Some("ROBOT1.0_AbCdEf0123456789"),
            "QQ 官方的消息 id 不是纯数字，但它就是平台消息 ID，必须带引用"
        );
    }

    /// 本层的合成键判据：**形状**交给适配器单源判据，`inject-`/空串由本层负责。
    ///
    /// 形状本身的往返测试在 `hbot-adapters`
    /// （`synthetic_id_judgement_round_trips_the_fallback_format`）；这条钉的是
    /// "本层确实把三者拼在了一起，且没有把 QQ 形态误判成合成键"。
    #[test]
    fn synthetic_external_ids_are_recognised_by_shape() {
        // 兜底键：三段、前两段数字、末段 16 位十六进制 → 合成。
        assert!(is_synthetic_external_id("111-1794790031-06032ba7feff3e72"));
        // OneBot 真 message_id（i64）→ 平台 id。
        assert!(!is_synthetic_external_id("77001"));
        // QQ 官方消息 id → 平台 id。
        assert!(!is_synthetic_external_id("ROBOT1.0_AbCdEf0123456789"));
        // 既有用例用的形态（非数字、两段）→ 平台 id。
        assert!(!is_synthetic_external_id("msg-777"));
        // 注入键 → 合成（payload 标记之外的兜底）。
        assert!(is_synthetic_external_id("inject-abc"));
        // 空串没有引用目标可言。
        assert!(is_synthetic_external_id(""));
        // 形状相近但段数/字符集不符的，不误判成合成。
        assert!(!is_synthetic_external_id("111-1794790031-06032ba7feff3e7"));
        assert!(!is_synthetic_external_id("111-abc-06032ba7feff3e72"));
        assert!(!is_synthetic_external_id(
            "111-1794790031-06032ba7feff3e722"
        ));
    }

    /// 超时重映射必须保留 `hbot-code` 的 `HBOT_TIMEOUT` 机器标识。
    ///
    /// 起因（parity 的 P3）：`HbotError::timeout("sandbox.dispatch", ms)` 的 Display 只有
    /// `timeout after 4000ms: sandbox.dispatch`，运维按 `HBOT_TIMEOUT` grep 一条都搜不到。
    /// 这条用例把四处可定位性一起钉住：机器标识、毫秒数、dispatch 阶段、既有 `timeout after`
    /// 文案（`crates/hbot-parity/tests/parity_05_*.rs` 按它断言，别改掉）。
    #[test]
    fn timeout_keeps_the_hbot_code_marker() {
        let error = CodeError::Timeout { budget_ms: 4000 };
        let mapped = timeout_error(&error);
        assert_eq!(mapped.code(), -32004, "超时必须是可重试的 -32004");
        let text = mapped.to_string();
        assert!(
            text.contains("HBOT_TIMEOUT"),
            "重映射后丢了 hbot-code 的机器标识（运维按它 grep）：{text}"
        );
        assert!(text.contains("4000"), "文案里必须带毫秒数：{text}");
        assert!(
            text.contains("sandbox.dispatch"),
            "文案里要能看出是 dispatch 阶段超时：{text}"
        );
        assert!(
            text.contains("timeout after"),
            "既有 grep 与 parity 用例按 `timeout after` 断言，不能丢：{text}"
        );
        assert!(
            text.contains("budget and was interrupted"),
            "原 CodeError 的措辞应当整段保留（单一真源在 hbot-code）：{text}"
        );
    }

    /// 防呆：上面用例里的 `env.store` / `env.events` 不是死字段（供后续用例扩展）。
    #[test]
    fn fixture_keeps_store_and_event_bus_alive() {
        let env = Env::new("fixture", GOOD_V1);
        assert!(env.store.schema_version().is_ok());
        assert!(Path::new(&env.root).exists());
        assert_eq!(env.events.receiver_count(), 0);
    }

    /// P2 回归：handler **抛错之前**用 `ctx.log` 写出的行必须留在日志表与
    /// `RunLog` 流里，并且排在"处理失败"那行**之前**。
    ///
    /// 旧实现里 `finish` 的错误分支只返回 `CodeError`，`RawEvent.logs` 在
    /// `hbot-code` 内部就被丢掉，rpc 侧 `record_logs` 只有成功路径会走 ——
    /// 用户看到的是"我的 bot 出错了，但出错前那三行 log 一条都没有"。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn logs_written_before_a_throw_are_persisted_before_the_error_line() {
        let mut env = Env::new("log-before-throw", LOG_THEN_THROW);
        // 先订阅：`RunLog` 是广播事件，处理完再订阅就收不到已经发出去的那几条。
        let mut run_log = env.events.subscribe();

        let error = env
            .handle(env.ctx("evt-1", "hi"))
            .await
            .expect_err("handler 抛错必须让消息失败");
        assert_eq!(error.code(), -32002, "JS 异常映射为 invalid：{error}");
        assert!(error.to_string().contains("boom"), "{error}");
        assert!(
            env.take_replies().is_empty(),
            "失败路径不交付回复（回复只在成功路径 deliver）"
        );

        // 日志表：`list_logs` 最新在前，所以用户那行的下标必须更大（写得更早）。
        let rows: Vec<(String, String)> = env
            .store
            .list_logs(Some(&env.bot_id), 200)
            .expect("读日志")
            .into_iter()
            .map(|row| (row.level, row.line))
            .collect();
        let user_at = rows
            .iter()
            .position(|(level, line)| level == "info" && line == "before the throw")
            .unwrap_or_else(|| panic!("用户日志行没有落进日志表：{rows:?}"));
        let error_at = rows
            .iter()
            .position(|(level, line)| level == "error" && line.starts_with("消息 "))
            .unwrap_or_else(|| panic!("失败的错误行没有落进日志表：{rows:?}"));
        assert!(
            user_at > error_at,
            "用户那行必须先于错误行落库（最新在前：user_at={user_at}, error_at={error_at}）：{rows:?}"
        );

        // 事件流：`RunLog` 的到达顺序同样必须是"用户日志 → 错误行"。
        let mut emitted: Vec<(bool, String)> = Vec::new();
        while let Ok(event) = run_log.try_recv() {
            if let Event::RunLog { level, line, .. } = event {
                emitted.push((level == LogLevel::Error, line));
            }
        }
        let user_event = emitted
            .iter()
            .position(|(is_error, line)| !is_error && line == "before the throw")
            .unwrap_or_else(|| panic!("RunLog 流里没有用户日志：{emitted:?}"));
        let error_event = emitted
            .iter()
            .position(|(is_error, line)| *is_error && line.starts_with("消息 "))
            .unwrap_or_else(|| panic!("RunLog 流里没有失败错误行：{emitted:?}"));
        assert!(
            user_event < error_event,
            "RunLog 流里用户日志必须先于错误行（user_event={user_event}, error_event={error_event}）：{emitted:?}"
        );
    }

    /// `hbot.d.ts` 声明 `text: string`，而 `MessageCtx::text()` 是 `Option<&str>`：
    /// 纯图片、纯表情这类 payload 里没有 `text` 字段。修复前 `event_json` 把
    /// `Option<&str>` 原样丢给 `json!`，落到 JS 就是 `null`，生成代码写
    /// `ctx.text.length` 会当场抛 TypeError —— 而 TypeScript 类型检查一个字都不报，
    /// 因为声明撒谎。
    ///
    /// 契约：缺文本时 `ctx.text` 是**空串**（不是 `null`、不是 `undefined`），
    /// `.length === 0`。
    const TEXT_LENGTH: &str = "import { defineBot } from 'hbot';\n\
                               export default defineBot({\n\
                                 onMessage(ctx) {\n\
                                   ctx.reply('len=' + ctx.text.length + ' type=' + typeof ctx.text);\n\
                                 },\n\
                               });\n";

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_message_without_text_still_has_a_string_ctx_text() {
        let mut env = Env::new("no-text", TEXT_LENGTH);
        // 纯图片入站：payload 里没有 `text` 字段（OneBot 的图片消息就是这样）。
        let ctx = env.ctx_with_payload(
            "evt-image",
            json!({ "image": "https://example.invalid/a.png", "postType": "message" }),
        );

        env.handle(ctx)
            .await
            .expect("没有 text 字段的消息不该让 handler 失败");

        assert_eq!(
            env.reply_texts(),
            vec!["len=0 type=string".to_owned()],
            "缺文本时 ctx.text 必须是空串：读 .length 不许抛"
        );
    }
}
