//! One QuickJS isolate on its own OS thread.
//!
//! Threading model (per `docs/02-architecture.md` §6.3): `rquickjs::Runtime` is
//! `!Send` unless the `parallel` feature is enabled, and we deliberately do not
//! enable it. So the runtime never leaves its thread; the rest of the process
//! talks to it through a bounded channel of commands and receives results on
//! one-shot channels.
//!
//! Inside the thread we run a `current_thread` tokio runtime plus a [`LocalSet`].
//! The `LocalSet` is what lets `sleep` await a tokio timer while the isolate's
//! `AsyncContext` future is being polled (`WithFuture` drives spawned futures).
//!
//! Module loading uses QuickJS' native ES module loader: [`ProjectResolver`]
//! maps a specifier to a project-relative module name (rejecting bare packages
//! with `HBOT_BARE_IMPORT`), and [`ProjectLoader`] hands QuickJS the stripped
//! JavaScript that `compile` already produced. There is no bundling step and no
//! `node_modules` lookup, matching `docs/02-architecture.md` §6.2.
//!
//! [`LocalSet`]: tokio::task::LocalSet

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::MaybePromise;
use rquickjs::{
    AsyncContext, AsyncRuntime, Ctx, Exception, Function, Module, Object, Persistent, Value,
};

use crate::ai::AiSlot;
use crate::compile::CompiledProject;
use crate::device::DeviceSlot;
use crate::dispatch::EventFailure;
use crate::error::CodeError;
use crate::http::HttpConfig;
use crate::instrument;
use crate::jobs::JobSlot;
use crate::limits::{Deadline, Limits};
use crate::modules;
use crate::send::SendSlot;
use crate::state::StateSlot;
use crate::timer::TimerSlot;
use hbot_core::debug::{Breakpoints, DebugSink};
use hbot_core::{AiClient, BotState, DeviceBackend, DeviceGate, HistoryReader};

use super::debug_host::{DebugControl, DebugRuntime};
use super::host_api::{self, HostOutput};
use super::{EventOutcome, MemoryReport, js_error};

/// How many commands may queue up before `dispatch` blocks.
///
/// Per-conversation serialization lives one layer up (`hbot-bot`); this is only
/// the isolate's inbox, and it stays small on purpose so a burst cannot grow
/// host memory without bound.
const COMMAND_BUFFER: usize = 8;

/// How long [`Sandbox::send`] waits between `try_send` retries once the inbox
/// is full (`std`'s `SyncSender::send_timeout` is still unstable).
const SEND_RETRY: Duration = Duration::from_millis(2);

/// How often [`Sandbox::dispatch_with_debug`] re-checks whether the delay it is
/// waiting on is a user looking at a paused frame or a genuinely wedged worker.
///
/// A normal dispatch answers immediately: `recv_timeout` returns as soon as the
/// reply arrives, so this interval is only the granularity of that one check and
/// never a per-event cost.
const DEBUG_POLL: Duration = Duration::from_millis(50);

/// Prefix that turns a project-relative module name into a relative specifier.
///
/// QuickJS resolves the entry against an empty base when the import comes from
/// Rust, so it must read `./src/main.ts` to take the relative branch of
/// [`modules::resolve_module`].
const ENTRY_SPECIFIER_PREFIX: &str = "./";

/// A handle to a running isolate.
///
/// Dropping the handle asks the isolate to stop and waits for it, but only for
/// [`Limits::handler_timeout`] plus a grace second: a worker wedged inside
/// QuickJS is detached and logged instead of hanging the host.
pub struct Sandbox {
    commands: SyncSender<Command>,
    /// Fires once the worker thread body is about to return.
    ///
    /// [`Drop`] uses it to bound its wait: the thread may be wedged inside
    /// QuickJS (or awaiting a promise that never settles), and joining such a
    /// thread hangs the host forever — the S1 bug this channel exists to kill.
    ///
    /// The receiver sits behind a [`Mutex`] only for auto-trait reasons:
    /// [`Receiver`] is `!Sync`, which would make `Sandbox: !Sync`, which would
    /// make `Arc<Sandbox>: !Send` and leave no `Send + Sync` executor able to
    /// hold one (`crates/hbot-rpc/src/app/runtime/sandbox_handler.rs` needs
    /// exactly that). The mutex is never a serialization point: the receiver is
    /// taken out exactly once, by [`Drop`], which already has `&mut self`. If it
    /// did serialize anything, that would defeat the per-conversation
    /// parallelism the bot layer relies on.
    finished: Mutex<Option<Receiver<()>>>,
    worker: Option<JoinHandle<()>>,
    limits: Limits,
    /// Breakpoints for the instrumented (`debug`) build, and whether the worker
    /// is parked at one. Shared with the worker thread through an `Arc` so
    /// `set_breakpoints` needs no round-trip.
    ///
    /// Only consulted by `__hbot_trace`/`__hbot_pause` in a debug build
    /// (`crate::instrument`, `docs/02-architecture.md` §6.5); a release build has
    /// no probes, so this is inert there.
    debug: Arc<Mutex<DebugState>>,
    /// The debugger's atomics, sink slot and step session.
    ///
    /// Separate from [`Sandbox::debug`] because the probe fast path reads
    /// atomics and must not take the `DebugState` mutex, and because the sink is
    /// only meaningful for the duration of one [`Sandbox::dispatch_with_debug`].
    control: Arc<DebugControl>,
    /// The device slot: gate + backend, replaceable at any time.
    ///
    /// Deliberately **not** copied into the isolate at spawn: every
    /// `ctx.device.*` call re-reads it, so revoking a device session takes effect
    /// on the next call rather than on the next isolate (`docs/09` §4.2). Kept as
    /// an `Arc` so [`Sandbox::set_device`] needs no command round-trip and cannot
    /// be blocked by a wedged worker.
    device: Arc<DeviceSlot>,
    /// The state slot: `kv` + `history` backends and this event's identity,
    /// replaceable at any time.
    ///
    /// Same reason as [`Sandbox::device`]: the spawn signatures are frozen, so
    /// the only way in is a slot the worker re-reads per call — never a
    /// spawn-time snapshot (`docs/09` §4.2). The identity (`botId` /
    /// `conversationId`) lives here too, because the isolate is reused across
    /// events and `ctx.kv`/`ctx.history` are installed once at bootstrap.
    state: Arc<StateSlot>,
    /// The intelligence slot: the model client and this event's call budget,
    /// replaceable at any time.
    ///
    /// Same reason as [`Sandbox::device`] and [`Sandbox::state`]: spawn
    /// signatures are frozen, so the worker re-reads the slot per call instead of
    /// capturing a client at spawn. Replacing it therefore points the very next
    /// `ctx.ai.complete` at the new model without restarting the bot.
    ai: Arc<AiSlot>,
    /// The timer slot: the declared schedule table, replaceable at any time.
    ///
    /// Same reason as the other slots, plus one of its own: the host asks this
    /// slot "what is due" once per tick, so the clock lives in the owner thread
    /// and the isolate itself never needs a JS `setTimeout` (which the sandbox
    /// does not expose anyway).
    timer: Arc<TimerSlot>,
    /// 作业槽：声明表 + 入队端口，随事件替换。
    ///
    /// 与 [`Sandbox::timer`] 相反，这个槽**必须** clone 给 worker 线程：`ctx.jobs`
    /// 是在 handler 体内被调用的（`onMessage` 里入队），而入队要读到「本事件的
    /// messageId」与声明表，两样都在 worker 侧。宿主线程只用它装配与读
    /// [`Sandbox::is_jobs_assembled`]。
    jobs: Arc<JobSlot>,
    /// 会话等待槽：声明开关 + 等待端口 + 本事件的身份，随事件替换。
    ///
    /// 与 [`Sandbox::jobs`] 同一理由 clone 给 worker：`ctx.session.prompt` 是在 handler
    /// 体内被调用的，它要读「本事件属于哪个会话、谁在说话」，两样都在 worker 侧。
    session: Arc<crate::session::SessionSlot>,
    /// 出站槽：声明开关 + 清单目标 + 出站端口，随事件替换。
    ///
    /// 与 [`Sandbox::jobs`] 同一理由 clone 给 worker：`ctx.send` / `ctx.notify`
    /// 是在 handler 体内被调用的，它们读的是**这个槽**，不是事件里那份快照 ——
    /// 改清单（声明 / 通知目标）之后下一次调用就按新清单判，不需要重启 bot。
    send: Arc<SendSlot>,
}

/// Mutable debug state shared with the worker thread.
#[derive(Debug, Default)]
pub struct DebugState {
    /// Breakpoints, per project-relative path.
    pub breakpoints: Breakpoints,
    /// Set while the worker is parked at a breakpoint.
    pub paused: bool,
    /// `"<path>:<line>"` of conditional breakpoints whose condition already
    /// failed to compile or evaluate.
    ///
    /// A broken condition fails on *every* hit of its line, so warning each time
    /// would flood the log (and compete with the user's own events for IO).
    /// One warning per line is enough to make the mistake visible.
    pub(crate) warned_conditions: std::collections::BTreeSet<String>,
    /// `"<path>:<line>"` of logpoints whose message template already failed to
    /// compile.
    ///
    /// Same reason as [`Self::warned_conditions`]: a broken template is broken on
    /// every hit, so the warning is emitted once and the logpoint then stays
    /// silent instead of switching the whole probe path off.
    pub(crate) warned_logs: std::collections::BTreeSet<String>,
}

impl Sandbox {
    /// Start an isolate for `project` and evaluate its entry module.
    ///
    /// `project` is already compiled JavaScript; the isolate only loads and
    /// evaluates it. This call therefore reports the two failure classes that
    /// can only be seen at runtime: the isolate could not start at all
    /// ([`CodeError::SandboxDead`]) or the entry module itself misbehaved —
    /// threw ([`CodeError::JsException`]), looped ([`CodeError::Timeout`]) or
    /// exhausted the heap ([`CodeError::MemoryLimit`]).
    ///
    /// # Errors
    ///
    /// See above; a project whose `src/main.ts` does not default-export
    /// `defineBot({ onMessage })` is reported as [`CodeError::JsException`] with
    /// an explanatory message, because that is how the isolate sees it.
    pub fn spawn(project: CompiledProject, limits: Limits) -> Result<Self, CodeError> {
        Self::spawn_with_http(project, limits, HttpConfig::deny_all())
    }

    /// [`Sandbox::spawn`], with `http.get` allowed to reach the hosts `http`
    /// lists.
    ///
    /// [`Sandbox::spawn`] is this call with a deny-all [`HttpConfig`]: a caller
    /// that does not think about the network gets no network, which is the
    /// fail-closed default `hbot.json`'s empty `permissions.http` already
    /// implies.
    ///
    /// # Errors
    ///
    /// As [`Sandbox::spawn`].
    pub fn spawn_with_http(
        project: CompiledProject,
        limits: Limits,
        http: HttpConfig,
    ) -> Result<Self, CodeError> {
        let http = Arc::new(http);
        let (commands, inbox) = std::sync::mpsc::sync_channel(COMMAND_BUFFER);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker_project = Arc::new(project);
        // Built before the thread starts: the worker needs both, and the debug
        // state has to be observable through `Sandbox` while the entry module is
        // still evaluating.
        let debug = Arc::new(Mutex::new(DebugState::default()));
        let control = Arc::new(DebugControl::new());
        let device = Arc::new(DeviceSlot::new());
        let state = Arc::new(StateSlot::new());
        let ai = Arc::new(AiSlot::new());
        let timer = Arc::new(TimerSlot::new());
        let jobs = Arc::new(JobSlot::new());
        let worker_jobs = Arc::clone(&jobs);
        let session = Arc::new(crate::session::SessionSlot::new());
        let worker_session = Arc::clone(&session);
        let send = Arc::new(SendSlot::new());
        let worker_send = Arc::clone(&send);
        let worker_debug = Arc::clone(&debug);
        let worker_control = Arc::clone(&control);
        let worker_http = Arc::clone(&http);
        let worker_device = Arc::clone(&device);
        let worker_state = Arc::clone(&state);
        let worker_ai = Arc::clone(&ai);
        let worker = std::thread::Builder::new()
            .name("hbot-isolate".to_owned())
            .spawn(move || {
                worker(
                    &inbox,
                    worker_project,
                    limits,
                    worker_debug,
                    worker_control,
                    worker_http,
                    worker_device,
                    worker_state,
                    worker_ai,
                    worker_jobs,
                    worker_session,
                    worker_send,
                    &started_tx,
                    &finished_tx,
                )
            })
            .map_err(|error| CodeError::SandboxDead {
                message: format!("cannot spawn isolate thread: {error}"),
            })?;

        let sandbox = Self {
            commands,
            finished: Mutex::new(Some(finished_rx)),
            worker: Some(worker),
            limits,
            debug,
            control,
            device,
            state,
            ai,
            timer,
            jobs,
            session,
            send,
        };

        // Wait for the entry module to finish evaluating before handing out a
        // handle: "the bot started" must mean "onMessage is callable".
        match started_rx.recv_timeout(sandbox.backstop()) {
            Ok(Ok(())) => Ok(sandbox),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(CodeError::SandboxDead {
                message: "isolate did not finish loading the entry module in time".to_owned(),
            }),
        }
    }

    /// Assemble device capability into this sandbox (or replace the assembly).
    ///
    /// The gate and the backend are **ports** (`hbot_core::device`); the host owns
    /// the implementations. Nothing is judged here: a handler that calls
    /// `ctx.device.screen()` still goes through the one gate on every call, and an
    /// all-allowing gate is the only way a call reaches `backend`.
    ///
    /// Safe to call at any time, including between two dispatches: the isolate
    /// reads the slot per call, never a spawn-time snapshot (`docs/09` §4.2), so
    /// replacing a gate with a denying one makes the *next* call fail.
    ///
    /// Because the spawn signatures are frozen (`hbot-rpc` and the S1 bench call
    /// them), this is the only way in: a sandbox without it answers
    /// `HBOT_DEVICE_UNAVAILABLE` and never fabricates an empty result.
    pub fn set_device(&self, gate: Arc<dyn DeviceGate>, backend: Arc<dyn DeviceBackend>) {
        self.device.set_device(gate, backend);
    }

    /// Set the application allow-list handed to [`DeviceBackend::input`].
    ///
    /// The whitelist is the intersection the *host* already approved
    /// (`hbot.json`'s `permissions.device.apps` ∩ the user's setting); the backend
    /// only uses it as the last foreground check, it does not judge again.
    /// Empty (the default) means "no application may receive input": a sandbox
    /// that never sets this cannot type into anything, which is the fail-closed
    /// direction.
    pub fn set_device_apps(&self, apps: Vec<String>) {
        self.device.set_apps(apps);
    }

    /// Whether [`Sandbox::set_device`] has been called on this sandbox.
    pub fn is_device_assembled(&self) -> bool {
        self.device.is_assembled()
    }

    /// Assemble the state backend (`kv` + `history`) into this sandbox, or
    /// replace the current one.
    ///
    /// The two arguments are **ports** (`hbot_core::BotState`,
    /// `hbot_core::HistoryReader`); the host owns the implementations (SQLite in
    /// `hbot-store`, injected by `hbot-rpc`). Nothing is judged here, but nothing
    /// is fabricated either: a sandbox that was never assembled answers
    /// `HBOT_STATE_UNAVAILABLE` on every call rather than keeping the data in a
    /// map that would vanish with the process.
    ///
    /// Safe to call at any time, including between two dispatches: the isolate
    /// re-reads the slot per call, never a spawn-time snapshot (`docs/09` §4.2),
    /// so a host that assembles late (or replaces the backend) takes effect on
    /// the *next* call. Same reason as [`Sandbox::set_device`], and the same
    /// constraint: the spawn signatures are frozen, so a slot is the only way in.
    ///
    /// The bot and conversation a call acts on are **not** set here — they come
    /// from each event's `botId` / `conversationId` payload fields, so one
    /// isolate can serve many conversations without re-assembling.
    pub fn set_state(&self, state: Arc<dyn BotState>, history: Arc<dyn HistoryReader>) {
        self.state.set(state, history);
    }

    /// Whether [`Sandbox::set_state`] has been called on this sandbox.
    pub fn is_state_assembled(&self) -> bool {
        self.state.is_assembled()
    }

    /// Assemble (or replace) the model client used by `ctx.ai.complete`.
    ///
    /// The argument is a **port** (`hbot_core::AiClient`); the host owns the
    /// implementation (`hbot-agent`'s provider, injected by `hbot-rpc`). Nothing
    /// is judged here: whether the project may call it at all is a manifest
    /// question answered by the implementation, and a sandbox that was never
    /// assembled answers `HBOT_AI_UNAVAILABLE` rather than inventing a reply.
    ///
    /// Safe to call at any time, including between two dispatches: the isolate
    /// re-reads the slot per call, so switching models in the IDE takes effect on
    /// the *next* call without restarting the bot. Same constraint as
    /// [`Sandbox::set_device`]: the spawn signatures are frozen, so a slot is the
    /// only way in.
    pub fn set_ai(&self, client: Arc<dyn AiClient>) {
        self.ai.set(client);
    }

    /// Whether [`Sandbox::set_ai`] has been called on this sandbox.
    pub fn is_ai_assembled(&self) -> bool {
        self.ai.is_assembled()
    }

    /// Replace the declared schedule table.
    ///
    /// `schedules` comes from `hbot.json`'s `capabilities.timer`; `now` is the
    /// moment the table is installed, so the first fire is strictly after it.
    /// Empty is valid and means "no timers" — that is also the state right after
    /// [`Sandbox::spawn`], before the host has read the manifest.
    ///
    /// Safe to call while a dispatch is in flight: `due()` is a separate tick,
    /// and the worker never reads this slot on the handler path.
    pub fn set_schedules(&self, schedules: &[hbot_core::ScheduleSpec], now: hbot_core::Timestamp) {
        self.timer.set_schedules(schedules, now);
    }

    /// How many schedules are installed (0 before assembly).
    #[must_use]
    pub fn schedule_count(&self) -> usize {
        self.timer.len()
    }

    /// The schedules due at `now`, advancing each one's grid.
    ///
    /// The host calls this once per tick and dispatches one
    /// [`Sandbox::dispatch_timer`] per entry. It mutates the table, so a name
    /// comes back at most once per grid point.
    ///
    /// Returns [`hbot_core::DueSchedule`] rather than a bare name because the
    /// host also needs to know **which conversation** the plan replies to: a
    /// timer has no inbound message, so without a declared target the only
    /// honest thing `ctx.reply` can do is refuse (`HBOT_TIMER_NO_TARGET`).
    #[must_use]
    pub fn due(&self, now: hbot_core::Timestamp) -> Vec<hbot_core::DueSchedule> {
        self.timer.due(now)
    }

    /// The earliest pending fire, or `None` with no schedules.
    ///
    /// Lets a host sleep until the next fire instead of polling, without
    /// duplicating the schedule arithmetic.
    #[must_use]
    pub fn next_fire(&self) -> Option<hbot_core::Timestamp> {
        self.timer.next_at()
    }

    /// Run `onTimer(ctx, name)` for one schedule.
    ///
    /// Same blocking contract, deadline handling, log collection and failure
    /// shape as [`Sandbox::dispatch_collecting`]: a timer that throws must
    /// produce a log the user can see, not silently vanish into the tick.
    ///
    /// # Errors
    ///
    /// [`EventFailure`] carrying the same [`CodeError`] classes as a message
    /// event — plus one of its own: a project whose default export has no
    /// `onTimer` reports a [`CodeError::JsException`] naming the missing hook,
    /// rather than pretending the timer fired successfully.
    pub fn dispatch_timer_collecting(
        &self,
        due: &hbot_core::DueSchedule,
    ) -> Result<EventOutcome, EventFailure> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Timer {
            name: due.name.clone(),
            conversation: due.conversation.clone(),
            reply,
        })
        .map_err(EventFailure::without_output)?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                Err(EventFailure::without_output(CodeError::SandboxDead {
                    message: format!(
                        "isolate did not answer within {:?}; the worker thread is wedged",
                        self.backstop()
                    ),
                }))
            }
            Err(RecvTimeoutError::Disconnected) => Err(EventFailure::without_output(self.dead())),
        }
    }

    /// [`Sandbox::dispatch_timer_collecting`] without the collected logs.
    ///
    /// # Errors
    ///
    /// Same as the collecting form, minus [`EventFailure`].
    pub fn dispatch_timer(&self, due: &hbot_core::DueSchedule) -> Result<EventOutcome, CodeError> {
        self.dispatch_timer_collecting(due)
            .map_err(|failure| *failure.error)
    }

    /// 一条消息命中了一个等待：跑 `onResume(ctx, message)`。
    ///
    /// **阻塞契约与 `dispatch_*_collecting` 相同**：从 `spawn_blocking` 里调，
    /// 失败路径带回 `ctx.log` 已经写出的行（与 `dispatch_timer_collecting` 同形）。
    ///
    /// # Errors
    ///
    /// 项目的默认导出没有 `onResume` 时报 [`CodeError::JsException`] 并点名缺的钩子
    /// 与等待键 —— 与缺 `onJob` 同一处理，不静默成功。
    pub fn dispatch_resume_collecting(
        &self,
        message_payload: &str,
        wait_key: &str,
        wait_prompt: &str,
        conversation: Option<String>,
    ) -> Result<EventOutcome, EventFailure> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Resume {
            message_payload: message_payload.to_owned(),
            wait_key: wait_key.to_owned(),
            wait_prompt: wait_prompt.to_owned(),
            conversation,
            reply,
        })
        .map_err(EventFailure::without_output)?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                Err(EventFailure::without_output(CodeError::SandboxDead {
                    message: format!(
                        "isolate did not answer within {:?}; the worker thread is wedged",
                        self.backstop()
                    ),
                }))
            }
            Err(RecvTimeoutError::Disconnected) => Err(EventFailure::without_output(self.dead())),
        }
    }

    /// 装（或替换）会话等待的声明开关与端口。
    ///
    /// 声明与端口**一起**装：只装端口会让「声明了才准开等待」变成「装配了就能开」。
    pub fn set_session(&self, declared: bool, queue: Arc<dyn crate::session::WaitQueue>) {
        self.session.set_declared(declared);
        self.session.set_queue(queue);
    }

    /// 会话等待端口是否装上了（声明开关可以独立为假）。
    #[must_use]
    pub fn is_session_assembled(&self) -> bool {
        self.session.is_assembled()
    }
    /// 装（或替换）作业声明表与入队端口。
    ///
    /// `specs` 来自 `hbot.json` 的 `capabilities.jobs`，`queue` 是宿主实现的端口
    /// （`hbot-rpc` 那边落到 `bot_jobs` 表）。这里不判断任何规则：能不能入队由
    /// [`hbot_core::validate_request`] 与声明表在 [`crate::jobs::JobSlot`] 里判，
    /// 落不落库由端口决定。**没装配就入队**会得到 `HBOT_JOBS_UNAVAILABLE`，
    /// 而不是悄悄把作业收进内存（那样重启后消失会像正常行为）。
    ///
    /// 与其它槽位一样，可以在任何时刻替换：worker 每次 `ctx.jobs.enqueue` 都重读
    /// 这个槽，改清单后下一次入队就按新表判，不需要重启 bot。
    pub fn set_jobs(&self, specs: &[hbot_core::JobSpec], queue: Arc<dyn crate::jobs::JobQueue>) {
        self.jobs.set_specs(specs);
        self.jobs.set_queue(queue);
    }

    /// 作业端口是否装上了（声明表可以独立为空）。
    #[must_use]
    pub fn is_jobs_assembled(&self) -> bool {
        self.jobs.is_assembled()
    }

    /// 装（或替换）出口能力的声明开关、通知目标与出站端口。
    ///
    /// 三样**一起**装，理由同 [`Sandbox::set_session`]：只装端口会让「声明了才准发」
    /// 退化成「装配了就能发」，而只装声明会让清单说可以、调用报 `UNAVAILABLE`。
    ///
    /// `target` 是 `capabilities.notify` 里的目标；`None` 表示清单没声明该能力
    /// （`ctx.notify` 报 `HBOT_NOTIFY_NOT_DECLARED`，与「宿主没装队列」分开）。
    pub fn set_egress(
        &self,
        send_declared: bool,
        target: Option<hbot_core::NotifyTarget>,
        queue: Arc<dyn crate::send::SendQueue>,
    ) {
        self.send.set_send_declared(send_declared);
        self.send.set_notify_target(target);
        self.send.set_queue(queue);
    }

    /// 已声明的作业类型数（0 表示清单没写 `capabilities.jobs`）。
    #[must_use]
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// 跑一次 `onJob(ctx, name)`：宿主的作业循环取到一条到期作业后调它。
    ///
    /// 阻塞契约、预算、日志收集与失败形状都与 [`Sandbox::dispatch_timer_collecting`]
    /// 相同。区别在参数：作业带 `payload` 与幂等键，两者都要出现在 ctx 上，否则
    /// 作业体只能靠全局状态猜自己该做什么。
    ///
    /// # Errors
    ///
    /// [`EventFailure`]：项目的默认导出没有 `onJob` 时报 [`CodeError::JsException`]
    /// 并点名缺的钩子与作业名 —— 与缺 `onTimer` 同一处理，不静默成功。
    pub fn dispatch_job_collecting(
        &self,
        job_id: hbot_core::JobId,
        budget_ms: u64,
        name: &str,
        payload: serde_json::Value,
        idempotency_key: &str,
        conversation: Option<String>,
    ) -> Result<EventOutcome, EventFailure> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Job {
            job_id,
            budget_ms,
            name: name.to_owned(),
            payload,
            idempotency_key: idempotency_key.to_owned(),
            conversation,
            reply,
        })
        .map_err(EventFailure::without_output)?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                Err(EventFailure::without_output(CodeError::SandboxDead {
                    message: format!(
                        "isolate did not answer within {:?}; the worker thread is wedged",
                        self.backstop()
                    ),
                }))
            }
            Err(RecvTimeoutError::Disconnected) => Err(EventFailure::without_output(self.dead())),
        }
    }

    /// 跑一次 {BT}onWebhook{BT}，把 handler 的返回值一并带回来。
    ///
    /// 与 {BT}Sandbox::dispatch_job_collecting{BT} 同形，区别在第二个返回值：webhook 的应答体是
    /// handler 的**返回值**，不是 {BT}ctx.reply{BT} 收集到的文本 —— 调用方在等 HTTP 响应，只有
    /// 返回值能当应答。
    ///
    /// {BT}body{BT} 已由传输层按 UTF-8 校验；非法编码在更外层就变成 400，走不到这里。
    pub fn dispatch_webhook_collecting(
        &self,
        route: &str,
        method: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> Result<(EventOutcome, Option<serde_json::Value>), EventFailure> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Webhook {
            route: route.to_owned(),
            method: method.to_owned(),
            content_type: content_type.map(str::to_owned),
            body: body.to_owned(),
            reply,
        })
        .map_err(EventFailure::without_output)?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                Err(EventFailure::without_output(CodeError::SandboxDead {
                    message: format!(
                        "isolate did not answer within {:?}; the worker thread is wedged",
                        self.backstop()
                    ),
                }))
            }
            Err(RecvTimeoutError::Disconnected) => Err(EventFailure::without_output(self.dead())),
        }
    }
    /// Run `onMessage` for one JSON event.
    ///
    /// This is a blocking call: use it from `spawn_blocking` or a dedicated
    /// thread, never from an async reactor thread. The isolate enforces
    /// [`Limits::handler_timeout`]; this method only holds a longer backstop in
    /// case the worker thread is wedged.
    ///
    /// `Sandbox` is `Send + Sync`, so nothing stops several threads from calling
    /// this concurrently; the worker still runs one command at a time and answers
    /// them in arrival order. That is safe but not free: a burst deeper than the
    /// inbox turns into [`CodeError::SandboxDead`] instead of queueing forever.
    /// Per-conversation serialization upstream (`hbot-bot`) is what keeps that
    /// from happening in practice — do not rely on this type for it.
    ///
    /// # Errors
    ///
    /// [`CodeError::JsException`], [`CodeError::Timeout`],
    /// [`CodeError::MemoryLimit`], or [`CodeError::SandboxDead`] when the
    /// isolate is gone.
    ///
    /// This drops the logs a failed handler already wrote; call
    /// [`Sandbox::dispatch_collecting`] when they matter (the host does — see
    /// [`EventFailure`]).
    pub fn dispatch(&self, event: &serde_json::Value) -> Result<EventOutcome, CodeError> {
        // Thin wrapper over the collecting form: the isolate collects the logs
        // either way, and this signature predates `EventFailure`. Callers that
        // only care whether the event failed keep it unchanged.
        self.dispatch_collecting(event)
            .map_err(|failure| *failure.error)
    }

    /// [`Sandbox::dispatch`], keeping the `ctx.log()` lines a failed handler had
    /// already written.
    ///
    /// A handler that logs three lines and then throws must not lose them: the
    /// logs are usually the only clue to *why* it threw, and the isolate has
    /// already collected them by the time the exception surfaces. This is the
    /// same dispatch, with the collected output attached to the failure instead
    /// of dropped by [`Self::finish`].
    ///
    /// The successful path is byte-for-byte [`Sandbox::dispatch`]. On failure,
    /// `logs` is empty only when no handler ever ran: the payload did not
    /// serialise, the isolate was already gone, or its inbox stayed full.
    ///
    /// # Errors
    ///
    /// [`EventFailure`] carrying the same [`CodeError`] [`Sandbox::dispatch`]
    /// reports, plus everything `ctx.log()` captured before the failure.
    pub fn dispatch_collecting(
        &self,
        event: &serde_json::Value,
    ) -> Result<EventOutcome, EventFailure> {
        let payload = payload_of(event)?;
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Event {
            payload,
            reply,
            debug: false,
        })
        .map_err(EventFailure::without_output)?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                Err(EventFailure::without_output(CodeError::SandboxDead {
                    message: format!(
                        "isolate did not answer within {:?}; the worker thread is wedged",
                        self.backstop()
                    ),
                }))
            }
            Err(RecvTimeoutError::Disconnected) => Err(EventFailure::without_output(self.dead())),
        }
    }

    /// Current heap usage of the isolate.
    ///
    /// # Errors
    ///
    /// [`CodeError::SandboxDead`] when the isolate is gone.
    pub fn memory(&self) -> Result<MemoryReport, CodeError> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Memory { reply })?;
        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => Err(CodeError::SandboxDead {
                message: "isolate did not report memory in time".to_owned(),
            }),
            Err(RecvTimeoutError::Disconnected) => Err(self.dead()),
        }
    }

    /// The limits this isolate was created with.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Replace this isolate's breakpoints.
    ///
    /// Takes effect on the next instrumented line the worker reaches, so a
    /// running handler is never interrupted mid-statement. Safe to call from any
    /// thread and while a dispatch is in flight — including from inside
    /// [`DebugSink::paused`], which is how a debugger UI changes breakpoints
    /// while parked.
    ///
    /// A release build has no probes, so this stores a set nothing will consult.
    pub fn set_breakpoints(&self, breakpoints: Breakpoints) {
        // Publish the cheap "is there anything at all" flag first: a probe that
        // sees an empty set must never take this mutex.
        self.control.note_breakpoints(!breakpoints.is_empty());
        let mut debug = self
            .debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug.breakpoints = breakpoints;
    }

    /// Current breakpoints (a snapshot; never blocks on the worker).
    #[must_use]
    pub fn breakpoints(&self) -> Breakpoints {
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .breakpoints
            .clone()
    }

    /// Whether the worker is parked at a breakpoint right now.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paused
    }

    /// Run `onMessage` with a debug sink attached.
    ///
    /// With `sink: None` this is exactly [`Sandbox::dispatch`]: no probe stops,
    /// and a debug build behaves like the release one.
    ///
    /// With a sink, every instrumented line the worker reaches asks the sink
    /// whether to stop; when it stops, the sink runs **on the worker thread** and
    /// blocks there until it returns a [`DebugAction`]
    /// (`docs/02-architecture.md` §6.5.1). So:
    ///
    /// * call this from `spawn_blocking` or a dedicated thread, never from an
    ///   async reactor;
    /// * the sink must bound itself — `hbot_core::debug` asks for 5 minutes —
    ///   because this call waits for it;
    /// * a sink that never returns keeps this call waiting. It is not reported as
    ///   [`CodeError::SandboxDead`] while the isolate says it is paused, because
    ///   a user staring at a paused frame is not a wedged worker. The
    ///   [`Limits::handler_timeout`] budget keeps running underneath, so the
    ///   handler is still cut off with [`CodeError::Timeout`] as soon as it
    ///   resumes — a paused debugger cannot disable the timeout.
    ///
    /// # Errors
    ///
    /// Same as [`Sandbox::dispatch`], plus the timeout behaviour above. Logs a
    /// failed handler already wrote are dropped here, exactly as in
    /// [`Sandbox::dispatch`]; use [`Sandbox::dispatch_with_debug_collecting`]
    /// to keep them.
    pub fn dispatch_with_debug(
        &self,
        event: &serde_json::Value,
        sink: Option<&dyn DebugSink>,
    ) -> Result<EventOutcome, CodeError> {
        self.dispatch_with_debug_collecting(event, sink)
            .map_err(|failure| *failure.error)
    }

    /// [`Sandbox::dispatch_with_debug`], keeping the logs of a failed handler.
    ///
    /// The debugger needs the same diagnostic output as the plain path: someone
    /// stepping through a handler that then throws wants the lines it printed
    /// before the throw. Blocking contract is identical to
    /// [`Sandbox::dispatch_with_debug`] — call it from `spawn_blocking` or a
    /// dedicated thread.
    ///
    /// # Errors
    ///
    /// [`EventFailure`] carrying the same [`CodeError`]
    /// [`Sandbox::dispatch_with_debug`] reports, plus everything `ctx.log()`
    /// captured before the failure.
    pub fn dispatch_with_debug_collecting(
        &self,
        event: &serde_json::Value,
        sink: Option<&dyn DebugSink>,
    ) -> Result<EventOutcome, EventFailure> {
        let payload = payload_of(event)?;
        let armed = sink.is_some();
        // The guard serializes debug dispatches and keeps the sink slot alive;
        // `clear_sink` below runs for every exit path, including errors.
        let guard = self.control.begin_dispatch(sink);
        let outcome = self.dispatch_instrumented(payload, armed);
        self.control.clear_sink();
        drop(guard);
        outcome
    }

    /// Queue an event whose probes may stop, and wait for the reply.
    ///
    /// Split out of [`Sandbox::dispatch_with_debug_collecting`] so the sink slot
    /// is cleared on every path, including [`Sandbox::send`] failing.
    fn dispatch_instrumented(
        &self,
        payload: String,
        armed: bool,
    ) -> Result<EventOutcome, EventFailure> {
        let (reply, result) = std::sync::mpsc::channel();
        self.send(Command::Event {
            payload,
            reply,
            debug: armed,
        })
        .map_err(EventFailure::without_output)?;

        let backstop = self.backstop();
        let mut wedged_for = Duration::ZERO;
        loop {
            match result.recv_timeout(DEBUG_POLL) {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(EventFailure::without_output(self.dead()));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.is_paused() {
                        // Parked in the sink on purpose: restart the wedge clock
                        // instead of reporting a healthy isolate as dead.
                        wedged_for = Duration::ZERO;
                        continue;
                    }
                    wedged_for = wedged_for.saturating_add(DEBUG_POLL);
                    if wedged_for >= backstop {
                        return Err(EventFailure::without_output(CodeError::SandboxDead {
                            message: format!(
                                "isolate did not answer within {backstop:?}; the worker thread is wedged"
                            ),
                        }));
                    }
                }
            }
        }
    }

    fn backstop(&self) -> Duration {
        self.limits
            .handler_timeout
            .saturating_mul(3)
            .saturating_add(Duration::from_secs(1))
    }

    /// How long [`Drop`] waits for the worker to stop.
    ///
    /// The interrupt handler kills a spinning handler shortly after
    /// [`Limits::handler_timeout`], so that budget plus a grace second covers
    /// every case where QuickJS is actually executing code. Anything that takes
    /// longer is a worker wedged inside a host call or awaiting a promise that
    /// never settles, and waiting for it would hang the host.
    fn shutdown_budget(&self) -> Duration {
        self.limits
            .handler_timeout
            .saturating_add(Duration::from_secs(1))
    }

    /// Queue `command` for the worker without blocking forever.
    ///
    /// A blocking `send` on a full inbox is a host hang: the worker only drains
    /// it by executing commands, and a wedged worker never will. The backstop
    /// turns that into [`CodeError::SandboxDead`] instead.
    ///
    /// This polls because `std`'s `SyncSender::send_timeout` is still unstable;
    /// the retry interval is short enough that a healthy worker never notices,
    /// and the loop only runs when the inbox is actually full.
    fn send(&self, command: Command) -> Result<(), CodeError> {
        let backstop = self.backstop();
        let deadline = Instant::now() + backstop;
        let mut command = command;
        loop {
            match self.commands.try_send(command) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(self.dead()),
                Err(TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return Err(CodeError::SandboxDead {
                            message: format!(
                                "isolate inbox is still full after {backstop:?}; the worker thread is wedged"
                            ),
                        });
                    }
                    command = returned;
                    std::thread::sleep(SEND_RETRY);
                }
            }
        }
    }

    fn dead(&self) -> CodeError {
        CodeError::SandboxDead {
            message: "isolate worker stopped".to_owned(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Ask the worker to stop. `try_send` rather than `send`: a wedged worker
        // never drains the inbox, and blocking here is exactly the host hang
        // this method must not cause. A full inbox already means the worker is
        // wedged (one in-flight command plus a `COMMAND_BUFFER` sized backlog is
        // more than the per-conversation serialization upstream would produce);
        // a closed channel means it already stopped.
        let _ = self.commands.try_send(Command::Shutdown);

        // Joining unconditionally was the S1 bug: a worker stuck inside QuickJS
        // (or awaiting a promise that never settles) never returns, so dropping
        // a `Sandbox` hung the whole host with no output and no CPU use. Wait for
        // a bounded grace period, then detach the thread and leave a log line —
        // a leaked thread is bad, a frozen user application is worse.
        let budget = self.shutdown_budget();
        // Take the receiver out of its slot so the bounded wait below holds no
        // lock. Only this method ever takes it, so a second `Drop` cannot run and
        // a missing receiver can only mean "already stopped".
        let finished = self
            .finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let stopped = match finished {
            Some(finished) => finished.recv_timeout(budget),
            None => Err(RecvTimeoutError::Disconnected),
        };
        let Some(worker) = self.worker.take() else {
            return;
        };
        match stopped {
            // Normal stop — or the sender was dropped because the thread body
            // ended (including a panic), in which case joining cannot block.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if worker.join().is_err() {
                    // A panicking worker is a bug, and it is also invisible to
                    // the caller because the panic happened on another thread.
                    // Record it instead of swallowing it silently.
                    tracing::error!("hbot-isolate thread panicked");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::error!(
                    budget_ms = budget.as_millis(),
                    "hbot-isolate thread did not stop within the shutdown budget; detaching it \
                     (the isolate may be blocked in JavaScript or waiting on a promise that never settles)"
                );
                // Dropping the handle detaches the thread instead of joining it.
            }
        }
    }
}

/// Serialise one event payload for the worker.
///
/// Shared by every dispatch entry point so the "not JSON-serialisable" failure
/// is byte-identical whichever one the caller uses.
fn payload_of(event: &serde_json::Value) -> Result<String, EventFailure> {
    serde_json::to_string(event).map_err(|error| {
        EventFailure::without_output(CodeError::JsException {
            message: format!("event is not JSON-serialisable: {error}"),
            stack: None,
        })
    })
}

/// A request from the owner thread to the isolate thread.
enum Command {
    /// Run the handler for `payload` and answer on `reply`.
    Event {
        payload: String,
        /// The outcome *or* the failure with the handler's collected logs, so
        /// the error path does not throw away what the handler printed.
        reply: Sender<Result<EventOutcome, EventFailure>>,
        /// Whether the caller attached a [`DebugSink`], i.e. whether probes may
        /// stop during this event. `false` for [`Sandbox::dispatch`], so a
        /// debug build runs uninstrumented unless someone is watching.
        debug: bool,
    },
    /// Report heap usage.
    Memory {
        reply: Sender<Result<MemoryReport, CodeError>>,
    },
    /// Run `onTimer(ctx, name)` for one schedule and answer on `reply`.
    ///
    /// A separate command rather than a synthesised `Event`: a timer has no
    /// inbound message, so it must not go through `build_event` (which would
    /// clear the device/state identity and hand the handler a fake payload).
    ///
    /// `conversation` is the plan's declared reply target, straight from
    /// [`hbot_core::DueSchedule`]. `None` means the plan named none, and then
    /// `ctx.reply` throws `HBOT_TIMER_NO_TARGET` instead of letting the host
    /// guess a destination: guessing wrong means posting into a stranger's
    /// group.
    Timer {
        name: String,
        conversation: Option<String>,
        reply: Sender<Result<EventOutcome, EventFailure>>,
    },
    /// 跑一次 `onJob(ctx, name)`：宿主取到一条到期作业后派发，答案回 `reply`。
    ///
    /// 与 `Timer` 同理，是独立变体而不是合成的 `Event`：作业没有入站消息，
    /// 走 `build_event` 会清空设备/状态身份并给作业一个假 payload。
    ///
    /// `idempotency_key` 是落库时用的那一个（默认取原消息的 messageId）：作业体
    /// 拿它做自己的幂等判断，因为**宿主保证的是至少一次**，不是恰好一次。
    Job {
        job_id: hbot_core::JobId,
        /// 该作业类型的预算（毫秒），来自入队时那一行的 `budget_ms`。
        ///
        /// 跟着行走而不是跟着清单走：改清单不该让已经排队的那条作业换一个预算。
        budget_ms: u64,
        name: String,
        payload: serde_json::Value,
        idempotency_key: String,
        conversation: Option<String>,
        reply: Sender<Result<EventOutcome, EventFailure>>,
    },

    /// 跑一次 `onResume(ctx, message)`：一条消息命中了一个等待，宿主把这条消息交给
    /// 恢复钩子而不是 `onMessage`。
    ///
    /// 与 `Timer` / `Job` 同理是独立变体：恢复事件既有入站消息（`message`），又没有
    /// 「这个 handler 在回答当前消息」这层语义 —— 它回答的是**之前**那次提问。走
    /// `build_event` 会让 `ctx.session.prompt` 挂到这条消息自己身上，形成自唤醒。
    Resume {
        /// 唤醒它的那条消息的完整事件 JSON（`event_json` 的产物）。
        message_payload: String,
        wait_key: String,
        wait_prompt: String,
        /// 回复目标（等待行里的 `conversation_external_id`）。
        conversation: Option<String>,
        reply: Sender<Result<EventOutcome, EventFailure>>,
    },
    /// 跑一次 {BT}onWebhook(ctx){BT}：外部系统的一次 HTTP 请求，handler 的返回值就是应答。
    ///
    /// 与 {BT}Timer{BT} / {BT}Job{BT} 同理是独立变体：webhook 既没有入站消息也没有会话，走
    /// {BT}build_event{BT} 会给它一个假 payload，还会把设备/状态身份留在槽位上。
    ///
    /// 应答体不走 {BT}HostOutput{BT}（那是日志与回复的收集器），而是随这条命令的 {BT}reply{BT}
    /// 一起回来：调用方在等 HTTP 响应，晚一步就没有意义了。
    Webhook {
        route: String,
        method: String,
        content_type: Option<String>,
        /// 请求体（传输层已按 UTF-8 校验过，这里不会拿到半截字符）。
        body: String,
        reply: Sender<Result<(EventOutcome, Option<serde_json::Value>), EventFailure>>,
    },
    /// Stop the worker loop.
    Shutdown,
}

/// Thread body: build the realm once, then serve commands until shutdown.
#[allow(clippy::too_many_arguments)]
fn worker(
    inbox: &Receiver<Command>,
    project: Arc<CompiledProject>,
    limits: Limits,
    debug: Arc<Mutex<DebugState>>,
    control: Arc<DebugControl>,
    http: Arc<HttpConfig>,
    device: Arc<DeviceSlot>,
    state: Arc<StateSlot>,
    ai: Arc<AiSlot>,
    jobs: Arc<JobSlot>,
    session: Arc<crate::session::SessionSlot>,
    send: Arc<SendSlot>,
    started: &Sender<Result<(), CodeError>>,
    finished: &Sender<()>,
) {
    if let Err(error) = serve(
        inbox, project, limits, debug, control, http, device, state, ai, jobs, session, send,
        started,
    ) {
        tracing::error!(
            code = error.code(),
            error = %error,
            "hbot-code isolate worker stopped"
        );
        let code = error.code();
        let message = error.to_string();
        // Tell `Sandbox::spawn` if it is still waiting for the entry module.
        let _ = started.send(Err(error));
        // Callers blocked on a reply must not wait for their backstop.
        let failure = CodeError::SandboxDead {
            message: format!("{code}: {message}"),
        };
        // Reject queued commands until the owner shuts us down. `Shutdown` must
        // end this loop: the owner joins this thread while it still holds the
        // command sender, so the channel never disconnects on its own — a worker
        // that ignores the shutdown message deadlocks the host on drop.
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Shutdown => break,
                other => reject(other, &failure),
            }
        }
    }

    // Tell `Sandbox::drop` the thread is about to end so it stops waiting.
    let _ = finished.send(());
}

fn reject(command: Command, failure: &CodeError) {
    let message = failure.to_string();
    match command {
        Command::Event { reply, .. } => {
            // The isolate is gone, so no handler ran for this event: there is
            // nothing collected to report.
            let _ = reply.send(Err(EventFailure::without_output(CodeError::SandboxDead {
                message,
            })));
        }
        Command::Memory { reply } => {
            let _ = reply.send(Err(CodeError::SandboxDead { message }));
        }
        Command::Timer { reply, .. } => {
            let _ = reply.send(Err(EventFailure::without_output(CodeError::SandboxDead {
                message,
            })));
        }
        Command::Resume { reply, .. } => {
            let _ = reply.send(Err(EventFailure::without_output(CodeError::SandboxDead {
                message: message.clone(),
            })));
        }
        Command::Webhook { reply, .. } => {
            let _ = reply.send(Err(EventFailure::without_output(CodeError::SandboxDead {
                message,
            })));
        }
        Command::Job { reply, .. } => {
            // 作业体没跑：宿主按失败计入 attempts，并按声明的重试次数退避。
            let _ = reply.send(Err(EventFailure::without_output(CodeError::SandboxDead {
                message,
            })));
        }
        Command::Shutdown => {}
    }
}

fn trace(step: &str) {
    if std::env::var_os("HBOT_S1_TRACE").is_some() {
        eprintln!("[s1] {step}");
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    inbox: &Receiver<Command>,
    project: Arc<CompiledProject>,
    limits: Limits,
    debug: Arc<Mutex<DebugState>>,
    control: Arc<DebugControl>,
    http: Arc<HttpConfig>,
    device: Arc<DeviceSlot>,
    state: Arc<StateSlot>,
    ai: Arc<AiSlot>,
    jobs: Arc<JobSlot>,
    session: Arc<crate::session::SessionSlot>,
    send: Arc<SendSlot>,
    started: &Sender<Result<(), CodeError>>,
) -> Result<(), CodeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CodeError::SandboxDead {
            message: format!("cannot build isolate tokio runtime: {error}"),
        })?;
    let local = tokio::task::LocalSet::new();
    trace("runtime built");
    let quicksjs = AsyncRuntime::new().map_err(js_error)?;
    trace("async runtime created");
    let deadline = Arc::new(Deadline::new());
    let output = Arc::new(Mutex::new(HostOutput::default()));

    // Configure the realm before anything is evaluated: limits first, then the
    // loader, so even the entry module cannot dodge them.
    {
        let quicksjs_runtime = &quicksjs;
        let project = &project;
        let guard = Arc::clone(&deadline);
        let interrupt = Box::new(move || guard.check());
        local.block_on(&runtime, async move {
            quicksjs_runtime.set_memory_limit(limits.memory_bytes).await;
            quicksjs_runtime
                .set_max_stack_size(limits.stack_bytes)
                .await;
            quicksjs_runtime
                .set_interrupt_handler(Some(interrupt))
                .await;
            quicksjs_runtime
                .set_loader(
                    ProjectResolver::new(Arc::clone(project)),
                    ProjectLoader::new(Arc::clone(project)),
                )
                .await;
        });
    }

    let context = local
        .block_on(&runtime, async { AsyncContext::full(&quicksjs).await })
        .map_err(js_error)?;
    trace("async context ready");

    let entry = format!("{ENTRY_SPECIFIER_PREFIX}{}", project.entry());
    let boot = {
        let context = &context;
        let output = &output;
        let deadline = &deadline;
        let project = &project;
        let debug = &debug;
        let control = &control;
        let http = &http;
        let device = &device;
        let state = &state;
        let ai = &ai;
        let jobs = &jobs;
        let session = &session;
        let send = &send;
        local.block_on(&runtime, async move {
            context
                .async_with(async |ctx| {
                    bootstrap(
                        &ctx, &entry, output, deadline, project, debug, control, http, device,
                        state, ai, jobs, session, send, limits,
                    )
                    .await
                })
                .await
        })
    };
    let hooks = boot?;
    let handler = hooks.message;

    // Only now is the bot callable; unblock `Sandbox::spawn`.
    let _ = started.send(Ok(()));

    while let Ok(command) = inbox.recv() {
        match command {
            Command::Shutdown => break,
            Command::Memory { reply } => {
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(Ok(memory_report(usage, limits)));
            }
            Command::Event {
                payload,
                reply,
                debug: instrumented,
            } => {
                let raw = {
                    let context = &context;
                    let handler = &handler;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    let control = &control;
                    let device = &device;
                    let state = &state;
                    let ai = &ai;
                    let jobs = &jobs;
                    let session = &session;
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_event(
                                    &ctx,
                                    &payload,
                                    handler,
                                    output,
                                    deadline,
                                    project,
                                    control,
                                    device,
                                    state,
                                    ai,
                                    jobs,
                                    session,
                                    instrumented,
                                    limits,
                                )
                                .await
                            })
                            .await
                    })
                };
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(finish(raw, memory_report(usage, limits), limits));
            }
            Command::Timer {
                name,
                conversation,
                reply,
            } => {
                let raw = {
                    let context = &context;
                    let timer_handler = &hooks.timer;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    let control = &control;
                    let device = &device;
                    let state = &state;
                    let ai = &ai;
                    let conversation = conversation.as_deref();
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_timer(
                                    &ctx,
                                    &name,
                                    conversation,
                                    timer_handler,
                                    output,
                                    deadline,
                                    project,
                                    control,
                                    device,
                                    state,
                                    ai,
                                    limits,
                                )
                                .await
                            })
                            .await
                    })
                };
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(finish(raw, memory_report(usage, limits), limits));
            }
            Command::Job {
                job_id,
                budget_ms,
                name,
                payload,
                idempotency_key,
                conversation,
                reply,
            } => {
                let raw = {
                    let context = &context;
                    let job_handler = &hooks.job;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    let control = &control;
                    let device = &device;
                    let state = &state;
                    let ai = &ai;
                    let jobs = &jobs;
                    let conversation = conversation.as_deref();
                    let payload = &payload;
                    let idempotency_key = idempotency_key.as_str();
                    let budget = Duration::from_millis(budget_ms);
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_job(
                                    &ctx,
                                    job_id,
                                    &name,
                                    payload,
                                    idempotency_key,
                                    conversation,
                                    budget,
                                    job_handler,
                                    output,
                                    deadline,
                                    project,
                                    control,
                                    device,
                                    state,
                                    ai,
                                    jobs,
                                )
                                .await
                            })
                            .await
                    })
                };
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(finish(raw, memory_report(usage, limits), limits));
            }
            Command::Webhook {
                route,
                method,
                content_type,
                body,
                reply,
            } => {
                let raw = {
                    let context = &context;
                    let webhook_handler = &hooks.webhook;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    let control = &control;
                    let ai = &ai;
                    let content_type = content_type.as_deref();
                    let budget = limits.handler_timeout;
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_webhook(
                                    &ctx,
                                    &route,
                                    &method,
                                    content_type,
                                    &body,
                                    budget,
                                    webhook_handler,
                                    output,
                                    deadline,
                                    project,
                                    control,
                                    ai,
                                )
                                .await
                            })
                            .await
                    })
                };
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let RawWebhook { event, response } = raw;
                let finished = finish(event, memory_report(usage, limits), limits)
                    .map(|outcome| (outcome, response));
                let _ = reply.send(finished);
            }
            Command::Resume {
                message_payload,
                wait_key,
                wait_prompt,
                conversation,
                reply,
            } => {
                let raw = {
                    let context = &context;
                    let resume_handler = &hooks.resume;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    let control = &control;
                    let device = &device;
                    let state = &state;
                    let ai = &ai;
                    let jobs = &jobs;
                    let session = &session;
                    let conversation = conversation.as_deref();
                    let message_payload = message_payload.as_str();
                    let wait_key = wait_key.as_str();
                    let wait_prompt = wait_prompt.as_str();
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_resume(
                                    &ctx,
                                    message_payload,
                                    wait_key,
                                    wait_prompt,
                                    conversation,
                                    resume_handler,
                                    output,
                                    deadline,
                                    project,
                                    control,
                                    device,
                                    state,
                                    ai,
                                    jobs,
                                    session,
                                    limits,
                                )
                                .await
                            })
                            .await
                    })
                };
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(finish(raw, memory_report(usage, limits), limits));
            }
        }
    }

    Ok(())
}

/// Turn QuickJS' allocator numbers into the host-facing report.
fn memory_report(usage: rquickjs::qjs::JSMemoryUsage, limits: Limits) -> MemoryReport {
    MemoryReport {
        used_bytes: usage.memory_used_size,
        heap_bytes: usage.malloc_size,
        // Echo our own cap rather than `malloc_limit`, so a report is always
        // consistent with the [`Limits`] the caller configured even if QuickJS
        // rounded the value.
        limit_bytes: i64::try_from(limits.memory_bytes).unwrap_or(i64::MAX),
    }
}

/// The hooks a project's default export provides, saved across dispatches.
struct Hooks {
    /// `onMessage`, always required.
    message: Persistent<Function<'static>>,
    /// `onTimer`, optional: a project with no `capabilities.timer` has no
    /// reason to define one, and a table installed later must not need a restart.
    timer: Option<Persistent<Function<'static>>>,
    /// `onJob`，可选：没声明 `capabilities.jobs` 的项目没有理由定义它。
    ///
    /// 与 `onTimer` 一样在 boot 时保存，不在 `defineBot` 里校验：清单与钩子
    /// 可以任一方先到，而一个不是函数的 `onJob` 值得在开机时点名，
    /// 而不是留到第一条作业才 TypeError。
    job: Option<Persistent<Function<'static>>>,
    /// `onResume`，可选：没声明 `capabilities.state.session` 的项目没有理由定义它。
    ///
    /// 缺它时不是开机错误：与 `onJob` 同一处理 —— 第一次唤醒会报一条点名
    /// `onResume` 的错（见 `run_resume`）。
    resume: Option<Persistent<Function<'static>>>,
    /// {BT}onWebhook{BT}，可选：没声明 {BT}capabilities.webhook{BT} 的项目没有理由定义它。
    ///
    /// 缺它时不是开机错误：与 {BT}onJob{BT} 同一处理 —— 第一条外部请求会报一条点名
    /// {BT}onWebhook{BT} 的错（见 {BT}run_webhook{BT}），而不是让调用方收到一个看起来成功的空应答。
    webhook: Option<Persistent<Function<'static>>>,
}

/// Evaluate `src/main.ts` and capture its hooks.
#[allow(clippy::too_many_arguments)]
async fn bootstrap<'js>(
    ctx: &Ctx<'js>,
    entry: &str,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &Arc<CompiledProject>,
    debug: &Arc<Mutex<DebugState>>,
    control: &Arc<DebugControl>,
    http: &Arc<HttpConfig>,
    device: &Arc<DeviceSlot>,
    state: &Arc<StateSlot>,
    ai: &Arc<AiSlot>,
    jobs: &Arc<JobSlot>,
    session: &Arc<crate::session::SessionSlot>,
    send: &Arc<SendSlot>,
    limits: Limits,
) -> Result<Hooks, CodeError> {
    deadline.reset();
    deadline.arm(limits.handler_timeout);

    let result = async {
        host_api::install(
            ctx,
            Arc::clone(output),
            Arc::clone(deadline),
            Arc::clone(http),
            Arc::clone(device),
            Arc::clone(state),
            Arc::clone(ai),
            Arc::clone(jobs),
            Arc::clone(session),
            Arc::clone(send),
        )?;
        // The probe functions exist in both builds so the loader never has to
        // care, but `__hbot_dbg` starts `false`: a release build never emits a
        // call, and a debug build never pays for one until a sink is attached.
        DebugRuntime::new(
            Arc::clone(control),
            Arc::clone(debug),
            Arc::clone(project),
            Arc::clone(deadline),
        )
        .install(ctx)?;
        trace("host api installed");

        // The single `Module::import` that kicks off the loader chain; every
        // further module is pulled in by QuickJS itself through
        // `ProjectResolver`/`ProjectLoader`.
        //
        // A rejected import (syntax-level TypeError, `ReferenceError` from a
        // missing global, a throw at module top level) settles this promise with
        // an exception instead of staying pending, so `describe` — not `js_error`
        // — is what turns it into a message and a remapped TypeScript stack.
        let namespace = Module::import(ctx, entry.to_owned())
            .map_err(|error| describe(ctx, error, project))?
            .into_future::<Object<'js>>()
            .await
            .map_err(|error| describe(ctx, error, project))?;
        trace("entry module evaluated");

        let default = namespace
            .get::<_, Value<'js>>("default")
            .map_err(js_error)?;
        let config = Object::from_value(default).map_err(|_| CodeError::JsException {
            message: format!(
                "{entry} must `export default defineBot({{ onMessage }})`; its default export is not an object"
            ),
            stack: None,
        })?;

        let handler = config
            .get::<_, Option<Function<'js>>>("onMessage")
            .map_err(js_error)?
            .ok_or_else(|| CodeError::JsException {
                message: format!("{entry}: defineBot({{ ... }}) has no `onMessage` function"),
                stack: None,
            })?;
        // Optional, and validated here rather than in `defineBot`: a project may
        // declare `capabilities.timer` and define `onTimer` in either order, and
        // an `onTimer` that is not callable is a programming error worth naming
        // at boot, not a TypeError deep inside the first tick.
        let timer = config
            .get::<_, Option<Function<'js>>>("onTimer")
            .map_err(js_error)?
            .map(|function| Persistent::save(ctx, function));
        // 同上，作业钩子也是可选的。缺它时不是开机错误：清单可能先声明了作业类型
        // 而代码还没写钩子，那时第一次派发会报一条点名 `onJob` 的错（见 run_job）。
        let job = config
            .get::<_, Option<Function<'js>>>("onJob")
            .map_err(js_error)?
            .map(|function| Persistent::save(ctx, function));

        // 同上：恢复钩子也是可选的。缺它时第一次唤醒会点名报错，不静默成功。
        let resume = config
            .get::<_, Option<Function<'js>>>("onResume")
            .map_err(js_error)?
            .map(|function| Persistent::save(ctx, function));

        // 同上：webhook 钩子可选。缺它时第一条外部请求点名报错 —— 否则调用方只会收到
        // 一个 204 空应答，看起来像成功了。
        let webhook = config
            .get::<_, Option<Function<'js>>>("onWebhook")
            .map_err(js_error)?
            .map(|function| Persistent::save(ctx, function));

        Ok::<_, CodeError>(Hooks {
            message: Persistent::save(ctx, handler),
            timer,
            job,
            resume,
            webhook,
        })
    }
    .await;

    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    result.map_err(|error| classify(error, interrupted, limits))
}

/// One handler invocation, with the deadline armed and host output captured.
#[allow(clippy::too_many_arguments)]
async fn run_event<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    control: &Arc<DebugControl>,
    device: &Arc<DeviceSlot>,
    state: &Arc<StateSlot>,
    ai: &Arc<AiSlot>,
    jobs: &Arc<JobSlot>,
    session: &Arc<crate::session::SessionSlot>,
    instrumented: bool,
    limits: Limits,
) -> RawEvent {
    deadline.reset();
    deadline.arm(limits.handler_timeout);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    // `armed` is what the Rust fast path reads; `__hbot_dbg` is what makes the
    // emitted probe skip the call entirely. Both are per event: a debug build
    // dispatched without a sink must cost what the release build costs.
    control.begin_event(instrumented);
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, instrumented);

    let started = Instant::now();
    let result = invoke(
        ctx, payload, handler, project, device, state, ai, jobs, session,
    )
    .await;
    let duration = started.elapsed();

    control.end_event();
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);
    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    let (logs, replies) = output.lock().unwrap_or_else(PoisonError::into_inner).take();

    RawEvent {
        logs,
        replies,
        duration,
        interrupted,
        result,
    }
}

/// Call `onMessage(event)` and drive the returned value to completion.
#[allow(clippy::too_many_arguments)]
async fn invoke<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
    project: &CompiledProject,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
    session: &crate::session::SessionSlot,
) -> Result<(), CodeError> {
    match invoke_inner(ctx, payload, handler, device, state, ai, jobs, session).await {
        Ok(()) => Ok(()),
        Err(error) => Err(describe(ctx, error, project)),
    }
}

/// One timer fire, with the deadline armed and host output captured.
///
/// Deliberately the same shape as [`run_event`]: the whole point of arming the
/// deadline is that a runaway `onTimer` is stopped by the same interrupt
/// handler, and the point of collecting output is that `ctx.log` inside a timer
/// still reaches the run log instead of vanishing into the tick.
#[allow(clippy::too_many_arguments)]
async fn run_timer<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    conversation: Option<&str>,
    handler: &Option<Persistent<Function<'static>>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    control: &Arc<DebugControl>,
    device: &Arc<DeviceSlot>,
    state: &Arc<StateSlot>,
    ai: &Arc<AiSlot>,
    limits: Limits,
) -> RawEvent {
    let started = Instant::now();
    let Some(handler) = handler else {
        // Not a timeout and not a runtime crash: the project simply has no timer
        // hook while its manifest declared a schedule. Naming that plainly is
        // the difference between "your schedule is fine, your code is missing
        // the hook" and a mute tick the user can never diagnose.
        let error = CodeError::JsException {
            message: format!(
                "定时器 `{name}` 到点了，但项目的默认导出没有 `onTimer` 函数（defineBot 里要写 `async onTimer(ctx, name)`）"
            ),
            stack: None,
        };
        return RawEvent {
            logs: Vec::new(),
            replies: Vec::new(),
            duration: started.elapsed(),
            interrupted: false,
            result: Err(error),
        };
    };

    deadline.reset();
    deadline.arm(limits.handler_timeout);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    control.begin_event(true);
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, true);

    let result = invoke_timer(
        ctx,
        name,
        conversation,
        handler,
        output,
        project,
        device,
        state,
        ai,
    )
    .await;
    let duration = started.elapsed();

    control.end_event();
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);
    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    let (logs, replies) = output.lock().unwrap_or_else(PoisonError::into_inner).take();

    RawEvent {
        logs,
        replies,
        duration,
        interrupted,
        result,
    }
}

/// 一次作业执行：预算来自作业行，日志收集与失败形状照 `run_timer`。
///
/// 与定时器的三处不同：
/// ① 预算来自**作业类型**（`JobSpec::budget_ms`，核心契约最长 10 分钟），不是 handler
///   的 2 秒。这正是 C4 存在的理由：`onMessage` 里入队后立刻返回，重活在别处跑。
///   仍然用同一个 deadline 槽与同一个 interrupt 处理器 —— 超预算的作业要被内核打断，
///   而不是把 isolate 线程占住到宿主关停。
/// ② 作业带 `payload` 与幂等键，两者都挂到 ctx 上；作业体拿它们决定自己做什么，
///   而不是靠全局状态去猜。
/// ③ 作业**不是**调试会话的一部分：调用方没有 sink，探针恒关（`instrumented=false`）。
#[allow(clippy::too_many_arguments)]
async fn run_job<'js>(
    ctx: &Ctx<'js>,
    job_id: hbot_core::JobId,
    name: &str,
    payload: &serde_json::Value,
    idempotency_key: &str,
    conversation: Option<&str>,
    budget: Duration,
    handler: &Option<Persistent<Function<'static>>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    control: &Arc<DebugControl>,
    device: &Arc<DeviceSlot>,
    state: &Arc<StateSlot>,
    ai: &Arc<AiSlot>,
    jobs: &Arc<JobSlot>,
) -> RawEvent {
    let started = Instant::now();
    let Some(handler) = handler else {
        // 与缺 `onTimer` 同一处理：清单声明了作业类型、代码没写钩子，要点名说清楚，
        // 而不是让作业循环把它当成「跑成功了」然后标 done。
        let error = CodeError::JsException {
            message: format!(
                "作业 `{name}`（{job_id}）要跑了，但项目的默认导出没有 `onJob` 函数（defineBot 里要写 `async onJob(ctx, name)`）"
            ),
            stack: None,
        };
        return RawEvent {
            logs: Vec::new(),
            replies: Vec::new(),
            duration: started.elapsed(),
            interrupted: false,
            result: Err(error),
        };
    };

    deadline.reset();
    deadline.arm(budget);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    control.begin_event(false);
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);

    let result = invoke_job(
        ctx,
        name,
        payload,
        idempotency_key,
        conversation,
        handler,
        output,
        project,
        device,
        state,
        ai,
        jobs,
    )
    .await;
    let duration = started.elapsed();

    control.end_event();
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);
    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    let (logs, replies) = output.lock().unwrap_or_else(PoisonError::into_inner).take();

    RawEvent {
        logs,
        replies,
        duration,
        interrupted,
        result,
    }
}

/// 一次 webhook 调用的原始结果：常规事件结果 + handler 的返回值。
///
/// 与 [`RawEvent`] 分开是刻意的：只有这一路有返回值，给其余命令都挂一个恒为 `None`
/// 的字段会让人以为消息/定时器/作业的 handler 也能"回答"什么。
struct RawWebhook {
    event: RawEvent,
    response: Option<serde_json::Value>,
}

/// 跑一次 {BT}onWebhook{BT}：预算、日志收集、返回值都归这里。
#[allow(clippy::too_many_arguments)]
async fn run_webhook<'js>(
    ctx: &Ctx<'js>,
    route: &str,
    method: &str,
    content_type: Option<&str>,
    body: &str,
    budget: Duration,
    handler: &Option<Persistent<Function<'static>>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    control: &Arc<DebugControl>,
    ai: &AiSlot,
) -> RawWebhook {
    let started = Instant::now();
    let Some(handler) = handler else {
        // 清单声明了路由、代码没写钩子：点名说清楚。否则调用方只会收到一个 204 空应答，
        // 看起来像成功了 —— 这类"不报错的失效"比崩掉难查一个数量级。
        let error = CodeError::JsException {
            message: format!(
                "路由 `{route}` 收到请求了，但项目的默认导出没有 `onWebhook` 函数（defineBot 里要写 `async onWebhook(ctx)`）"
            ),
            stack: None,
        };
        return RawWebhook {
            event: RawEvent {
                logs: Vec::new(),
                replies: Vec::new(),
                duration: started.elapsed(),
                interrupted: false,
                result: Err(error),
            },
            response: None,
        };
    };

    deadline.reset();
    deadline.arm(budget);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    control.begin_event(false);
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);

    let result = invoke_webhook(ctx, route, method, content_type, body, handler, project, ai).await;
    let duration = started.elapsed();

    control.end_event();
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);
    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    let (logs, replies) = output.lock().unwrap_or_else(PoisonError::into_inner).take();

    let (result, response) = match result {
        Ok(response) => (Ok(()), response),
        Err(error) => (Err(error), None),
    };

    RawWebhook {
        event: RawEvent {
            logs,
            replies,
            duration,
            interrupted,
            result,
        },
        response,
    }
}

/// 调用 {BT}onWebhook{BT}，把返回值转成 JSON。
///
/// {BT}undefined{BT} / {BT}null{BT} 是合法的"无应答体"（204），不是错误；其余值走
/// {BT}JSON.stringify{BT}，所以 {BT}{ status: 418, body: "teapot" }{BT} 这类信封天然可表达。
#[allow(clippy::too_many_arguments)]
async fn invoke_webhook<'js>(
    ctx: &Ctx<'js>,
    route: &str,
    method: &str,
    content_type: Option<&str>,
    body: &str,
    handler: &Persistent<Function<'static>>,
    project: &CompiledProject,
    ai: &AiSlot,
) -> Result<Option<serde_json::Value>, CodeError> {
    let result = async {
        let request_event = build_webhook_event(ctx, route, method, content_type, body, ai)?;
        let function = handler.clone().restore(ctx)?;
        let pending: MaybePromise<'js> = function.call((request_event,))?;
        let value: Value<'js> = pending.into_future::<Value<'js>>().await?;
        json_of_return(ctx, &value)
    }
    .await;
    result.map_err(|error| describe(ctx, error, project))
}

/// 把 handler 的返回值转成 {BT}serde_json::Value{BT}；{BT}undefined{BT} / {BT}null{BT} → {BT}None{BT}。
fn json_of_return<'js>(
    ctx: &Ctx<'js>,
    value: &Value<'js>,
) -> Result<Option<serde_json::Value>, rquickjs::Error> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let Some(text) = ctx.json_stringify(value.clone())? else {
        return Ok(None);
    };
    let text = text.to_string()?;
    // 序列化已经成功，解析失败只可能是我们自己的类型问题；退化成 None（204）
    // 比把一条 500 抛给外部系统好，日志里仍能看到 handler 的 ctx.log。
    Ok(serde_json::from_str(&text).ok())
}
#[allow(clippy::too_many_arguments)]
async fn invoke_job<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    payload: &serde_json::Value,
    idempotency_key: &str,
    conversation: Option<&str>,
    handler: &Persistent<Function<'static>>,
    output: &Arc<Mutex<HostOutput>>,
    project: &CompiledProject,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
) -> Result<(), CodeError> {
    let result = async {
        let event = build_job_event(
            ctx,
            name,
            payload,
            idempotency_key,
            conversation,
            output,
            device,
            state,
            ai,
            jobs,
        )?;
        let function = handler.clone().restore(ctx)?;
        let pending: MaybePromise<'js> = function.call((event, name))?;
        pending.into_future::<Value<'js>>().await?;
        Ok::<(), rquickjs::Error>(())
    }
    .await;
    result.map_err(|error| describe(ctx, error, project))
}

/// 一次等待被唤醒：跑 `onResume(ctx, message)`。
///
/// 与 [`run_timer`] / [`run_job`] 同一个形状（预算、日志收集、失败形状都一致），
/// 区别只在 ctx：恢复事件**有**入站消息，所以它比作业 ctx 宽 —— 挂 `ctx.message`
/// （唤醒它的那条）与 `ctx.waitKey` / `ctx.waitPrompt`，但**仍然不挂** `kv` /
/// `history` / `device` / `jobs` / `session`：
///
/// * 不挂 `kv`/`history`/`device`：恢复上下文里的「当前会话」是等待行声明的那个，
///   而不是这条消息自己的会话（用户可以换一个人来答 `scope: conversation`）；
/// * 不挂 `jobs`/`session`：一次唤醒不该再开新等待或新作业 —— 那会让「谁在等谁」
///   变成一棵树，而重建它需要读整张表。
#[allow(clippy::too_many_arguments)]
async fn run_resume<'js>(
    ctx: &Ctx<'js>,
    message_payload: &str,
    wait_key: &str,
    wait_prompt: &str,
    conversation: Option<&str>,
    handler: &Option<Persistent<Function<'static>>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    control: &Arc<DebugControl>,
    device: &Arc<DeviceSlot>,
    state: &Arc<StateSlot>,
    ai: &Arc<AiSlot>,
    jobs: &Arc<JobSlot>,
    session: &Arc<crate::session::SessionSlot>,
    limits: Limits,
) -> RawEvent {
    let started = Instant::now();
    let Some(handler) = handler else {
        // 与缺 `onJob` 同一处理：项目声明了 session 却没人实现钩子，要点名说清楚，
        // 而不是让这次唤醒静默成功（那样用户会以为「我回的话 bot 收到了」）。
        let error = CodeError::JsException {
            message: format!(
                "等待 `{wait_key}` 被一条消息唤醒了，但项目的默认导出没有 `onResume` 函数（defineBot 里要写 `async onResume(ctx, message)`）"
            ),
            stack: None,
        };
        return RawEvent {
            logs: Vec::new(),
            replies: Vec::new(),
            duration: started.elapsed(),
            interrupted: false,
            result: Err(error),
        };
    };

    deadline.reset();
    deadline.arm(limits.handler_timeout);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    control.begin_event(false);
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);

    let result = invoke_resume(
        ctx,
        message_payload,
        wait_key,
        wait_prompt,
        conversation,
        handler,
        output,
        project,
        device,
        state,
        ai,
        jobs,
        session,
    )
    .await;
    let duration = started.elapsed();

    control.end_event();
    let _ = ctx.globals().set(instrument::DEBUG_FLAG, false);
    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    let (logs, replies) = output.lock().unwrap_or_else(PoisonError::into_inner).take();

    RawEvent {
        logs,
        replies,
        duration,
        interrupted,
        result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn invoke_resume<'js>(
    ctx: &Ctx<'js>,
    message_payload: &str,
    wait_key: &str,
    wait_prompt: &str,
    conversation: Option<&str>,
    handler: &Persistent<Function<'static>>,
    output: &Arc<Mutex<HostOutput>>,
    project: &CompiledProject,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
    session: &crate::session::SessionSlot,
) -> Result<(), CodeError> {
    let result = async {
        let event = build_resume_event(
            ctx,
            message_payload,
            wait_key,
            wait_prompt,
            conversation,
            output,
            device,
            state,
            ai,
            jobs,
            session,
        )?;
        let function = handler.clone().restore(ctx)?;
        let message = event.get::<_, Value<'js>>("message")?;
        let pending: MaybePromise<'js> = function.call((event, message))?;
        pending.into_future::<Value<'js>>().await?;
        Ok::<(), rquickjs::Error>(())
    }
    .await;
    result.map_err(|error| describe(ctx, error, project))
}

/// 交给 `onResume(ctx, message)` 的 ctx。
///
/// 两个参数是**同一条消息**（`ctx` 是它的宿主面，`message` 是它的原始字段）：模板里
/// `ctx.text` 与 `message.text` 都能写，读的是同一份数据。
///
/// 身份仍然显式清空：恢复上下文里的「当前会话」是等待行声明的那个（可能不是这条
/// 消息的会话 —— `scope: conversation` 时任何人答都算），所以 `device` / `state` 的
/// 身份不能借自这条消息。
#[allow(clippy::too_many_arguments)]
fn build_resume_event<'js>(
    ctx: &Ctx<'js>,
    message_payload: &str,
    wait_key: &str,
    wait_prompt: &str,
    conversation: Option<&str>,
    output: &Arc<Mutex<HostOutput>>,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
    session: &crate::session::SessionSlot,
) -> rquickjs::Result<Object<'js>> {
    let parsed: Value<'js> = ctx.json_parse(message_payload.to_owned())?;
    let message = match Object::from_value(parsed.clone()) {
        Ok(object) => object,
        Err(_) => {
            let wrapper = Object::new(ctx.clone())?;
            wrapper.set("payload", parsed)?;
            wrapper
        }
    };

    let resume_event = Object::new(ctx.clone())?;
    resume_event.set("waitKey", wait_key)?;
    resume_event.set("waitPrompt", wait_prompt)?;
    resume_event.set("message", message.clone())?;
    // 把这条消息的常用字段平铺到 ctx 上：模板里写 `ctx.text` 比 `ctx.message.text` 顺，
    // 而两者指向同一份数据（不是两份可以各自过期的拷贝）。
    for field in [
        "text",
        "senderId",
        "senderName",
        "conversationExternalId",
        "messageId",
    ] {
        if let Some(value) = message.get::<_, Option<Value<'js>>>(field)? {
            resume_event.set(field, value)?;
        }
    }

    // 每次显式清空：恢复事件的身份是等待行的那个会话，不是这条消息的。
    device.set_current_bot(None);
    state.set_current_bot(None, None);
    ai.reset_event();
    jobs.set_current_message(None);
    session.set_current(conversation.map(str::to_owned), None);

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    let has_target = conversation.is_some();
    let reply = {
        let output = Arc::clone(output);
        Function::new(
            ctx.clone(),
            move |handle: Ctx<'js>, text: String| -> rquickjs::Result<()> {
                if !has_target {
                    return Err(Exception::throw_message(
                        &handle,
                        &format!(
                            "{}: 这次恢复没有声明回复目标，ctx.reply 不知道发给谁",
                            crate::session::NO_TARGET_CODE
                        ),
                    ));
                }
                output
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_reply(text);
                Ok(())
            },
        )
        .and_then(|function| function.with_name("reply"))?
    };
    resume_event.set("reply", reply)?;
    resume_event.set("log", host.get::<_, Function<'js>>("log")?)?;
    resume_event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    resume_event.set("ai", host.get::<_, Object<'js>>("ai")?)?;
    Ok(resume_event)
}
/// 交给 `onJob` 的 ctx，作业名同时作为第二个参数。
///
/// 比消息 ctx 还窄（`kv` / `history` / `device` / `jobs` 都没有），与 `onTimer` 同一套
/// 理由：作业没有入站身份，挂上这三个面就等于让它沿用上一个事件的人。作业要写状态，
/// 由它入队时那条消息去写 —— 那是有身份的。
#[allow(clippy::too_many_arguments)]
fn build_job_event<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    payload: &serde_json::Value,
    idempotency_key: &str,
    conversation: Option<&str>,
    output: &Arc<Mutex<HostOutput>>,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
) -> rquickjs::Result<Object<'js>> {
    let job_event = Object::new(ctx.clone())?;
    job_event.set("name", name)?;
    job_event.set("payload", ctx.json_parse(payload.to_string())?)?;
    job_event.set("idempotencyKey", idempotency_key)?;
    // 每次显式清空：作业没有身份，任何残留都会变成「读到上一个人的东西」。
    device.set_current_bot(None);
    state.set_current_bot(None, None);
    ai.reset_event();
    // 幂等键也清掉：作业事件里 `ctx.jobs.enqueue` 没有消息 id 可兜底，
    // 不显式清就会拿上一条消息的 messageId 当新作业的幂等键（两条不同的活撞成一条）。
    jobs.set_current_message(None);

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    let reply = {
        let output = Arc::clone(output);
        let has_target = conversation.is_some();
        Function::new(
            ctx.clone(),
            move |handle: Ctx<'js>, message: String| -> rquickjs::Result<()> {
                if !has_target {
                    return Err(Exception::throw_message(
                        &handle,
                        &format!("{}: 这条作业没有声明回复目标，ctx.reply 不知道发给谁。入队时写 options.conversation（群或私聊的 externalId），只想记日志就用 ctx.log", crate::jobs::NO_TARGET_CODE),
                    ));
                }
                output
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_reply(message);
                Ok(())
            },
        )
        .and_then(|function| function.with_name("reply"))?
    };
    job_event.set("reply", reply)?;
    job_event.set("log", host.get::<_, Function<'js>>("log")?)?;
    job_event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    job_event.set("ai", host.get::<_, Object<'js>>("ai")?)?;
    Ok(job_event)
}
/// {BT}onWebhook{BT} 的上下文：请求字段 + {BT}log{BT} / {BT}sleep{BT} / {BT}ai{BT}。
///
/// 比消息 ctx 窄得多，理由与 {BT}build_timer_event{BT} 相同：外部请求没有会话，也就没有
/// 回复目标。这里**没有** {BT}ctx.reply{BT}（不是"有但会报错"）—— webhook 的应答走 handler
/// 的返回值，多一个 {BT}reply{BT} 只会让人以为消息真发出去了。{BT}device{BT} / {BT}kv{BT} /
/// {BT}history{BT} / {BT}jobs{BT} / {BT}session{BT} 同样不挂：它们都要一个 bot 或会话身份，而这一路
/// 只有"哪个路由 + 一段体"。
fn build_webhook_event<'js>(
    ctx: &Ctx<'js>,
    route: &str,
    method: &str,
    content_type: Option<&str>,
    body: &str,
    ai: &AiSlot,
) -> rquickjs::Result<Object<'js>> {
    let request_event = Object::new(ctx.clone())?;
    request_event.set("route", route)?;
    request_event.set("method", method)?;
    request_event.set("contentType", content_type)?;
    request_event.set("body", body)?;
    // 体是 JSON 就顺手给一份解析好的，省得每个项目自己 try/catch 一次；不是 JSON 就是
    // undefined —— 作者写 `ctx.bodyJson.x` 时能立刻看出"这不是 JSON"，而不是拿到 null。
    //
    // 先在 Rust 侧试一次 `serde_json`，成功了才交给 `json_parse`：非 JSON 的体是**正常形状**
    // （表单提交、纯文本回调），不该抛异常，也不该让 handler 每次自己 try/catch。
    let body_json: Option<Value<'js>> = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => ctx.json_parse(value.to_string()).ok(),
        Err(_) => None,
    };
    match body_json {
        Some(value) => request_event.set("bodyJson", value)?,
        None => request_event.set("bodyJson", Value::new_undefined(ctx.clone()))?,
    }
    // 这一路没有 bot / 会话，任何残留都是"读到上一个人的东西"。
    ai.reset_event();

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    request_event.set("log", host.get::<_, Function<'js>>("log")?)?;
    request_event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    request_event.set("ai", host.get::<_, Object<'js>>("ai")?)?;
    Ok(request_event)
}
// Nine arguments like its siblings above: they are the same slot set (device / state /
// ai) threaded down from `serve`, and bundling them into a struct would only move the
// noise to every call site while making the ordering invisible.
#[allow(clippy::too_many_arguments)]
async fn invoke_timer<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    conversation: Option<&str>,
    handler: &Persistent<Function<'static>>,
    output: &Arc<Mutex<HostOutput>>,
    project: &CompiledProject,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
) -> Result<(), CodeError> {
    let result = async {
        let event = build_timer_event(ctx, name, conversation, output, device, state, ai)?;
        let function = handler.clone().restore(ctx)?;
        let pending: MaybePromise<'js> = function.call((event, name))?;
        pending.into_future::<Value<'js>>().await?;
        Ok::<(), rquickjs::Error>(())
    }
    .await;
    result.map_err(|error| describe(ctx, error, project))
}

/// The context handed to `onTimer`, plus the timer's name as the second
/// argument.
///
/// Deliberately **narrower** than a message event: `reply`, `log`, `sleep` and
/// `ai` are there, while `kv`, `history` and `device` are not. Those three are
/// per-bot-identity surfaces, and a timer carries no conversation, so exposing
/// them would either fail at call time (a confusing `HBOT_STATE_INVALID` from a
/// hook that looked declared) or, worse, silently reuse the previous event's
/// identity and let a schedule read someone else's messages. A schedule that
/// needs persistent state has to model it as a message-triggered action — that
/// is what `bot_jobs` will be for.
fn build_timer_event<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    conversation: Option<&str>,
    output: &Arc<Mutex<HostOutput>>,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
) -> rquickjs::Result<Object<'js>> {
    let timer_event = Object::new(ctx.clone())?;
    // Also on the object, so `ctx.name` works whether a project reads the second
    // argument or the event itself.
    timer_event.set("name", name)?;
    // Clear the identity anyway: nothing on this context can use it, and leaving
    // the previous event's bot parked on the slots is how a later refactor grows
    // an accidental cross-event read.
    device.set_current_bot(None);
    state.set_current_bot(None, None);
    ai.reset_event();

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    // `reply` is **not** the message-event one: a timer has no inbound message,
    // so the destination can only come from the plan's declared conversation.
    //
    // With a target, this pushes onto the same `HostOutput::replies` list the
    // message path uses (the host routes it from `DueSchedule::conversation`).
    // Without one, it throws `HBOT_TIMER_NO_TARGET` rather than letting the
    // message path's reply silently pick a conversation — guessing means posting
    // into a stranger's group.
    let reply = {
        let output = Arc::clone(output);
        let has_target = conversation.is_some();
        Function::new(
            ctx.clone(),
            move |handle: Ctx<'js>, message: String| -> rquickjs::Result<()> {
                if !has_target {
                    return Err(Exception::throw_message(
                        &handle,
                        &format!(
                            "{}: 这个定时器没有声明回复目标，ctx.reply 不知道发给谁。\
                             在 hbot.json 的 capabilities.timer 里给这条计划写 `conversation`\
                             （群或私聊的 externalId），只想记日志就用 ctx.log",
                            crate::timer::NO_TARGET_CODE
                        ),
                    ));
                }
                output
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_reply(message);
                Ok(())
            },
        )
        .and_then(|function| function.with_name("reply"))?
    };
    timer_event.set("reply", reply)?;
    timer_event.set("log", host.get::<_, Function<'js>>("log")?)?;
    timer_event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    timer_event.set("ai", host.get::<_, Object<'js>>("ai")?)?;
    Ok(timer_event)
}

#[allow(clippy::too_many_arguments)]
async fn invoke_inner<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
    session: &crate::session::SessionSlot,
) -> rquickjs::Result<()> {
    let event = build_event(ctx, payload, device, state, ai, jobs, session)?;
    let function = handler.clone().restore(ctx)?;
    // `MaybePromise` accepts both `onMessage(ctx) {}` and
    // `async onMessage(ctx) {}`; awaiting it is also what drives any
    // `await sleep(...)` the handler performs.
    let pending: MaybePromise<'js> = function.call((event,))?;
    // The handler's resolved value is irrelevant; only that it settled matters.
    pending.into_future::<Value<'js>>().await?;
    Ok(())
}

/// Turn the JSON payload into the object handed to `onMessage`, plus the host
/// functions under the names `docs/02-architecture.md` §6.1 documents
/// (`ctx.reply`, `ctx.log`, `ctx.sleep`), the device surface (`ctx.device`) and
/// the state surface (`ctx.kv`, `ctx.history`).
///
/// The payload is also where the device gate and the state slot learn *whose*
/// data to use: the isolate itself has no bot or conversation identity (a
/// [`CompiledProject`] does not carry one and the spawn signature is frozen), so
/// `botId` / `conversationId` from the event JSON are remembered on the slots for
/// the duration of the call. A payload without them **clears** the identity,
/// which makes every device call fail closed with `HBOT_DEVICE_FAILED:
/// ...没有 botId...` and every state call fail with `HBOT_STATE_INVALID`,
/// instead of inheriting the previous event's bot or reading another
/// conversation's history.
fn build_event<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    device: &DeviceSlot,
    state: &StateSlot,
    ai: &AiSlot,
    jobs: &JobSlot,
    session: &crate::session::SessionSlot,
) -> rquickjs::Result<Object<'js>> {
    let parsed: Value<'js> = ctx.json_parse(payload.to_owned())?;
    let event = match Object::from_value(parsed.clone()) {
        Ok(object) => object,
        // A non-object payload is still delivered, wrapped, so a handler never
        // has to guess what it was given.
        Err(_) => {
            let wrapper = Object::new(ctx.clone())?;
            wrapper.set("payload", parsed)?;
            wrapper
        }
    };

    device.set_current_bot(bot_id_of(payload));
    state.set_current_bot(bot_id_of(payload), conversation_id_of(payload));
    // 每事件清零调用额度：isolate 跨事件复用，不清零就等于按 isolate 计。
    ai.reset_event();
    // 缺省幂等键取本条消息的 messageId（[crate::jobs] 模块文档承诺的那条判据）：
    // 没有它就退化成「每次 enqueue 都要显式写 key」，而漏写就是重复干活。
    jobs.set_current_message(message_id_of(payload));
    // 会话等待的归属键同样取自这条事件：`prompt` 只能在消息事件里调用，而它要写
    // 「哪个会话、谁在等」。定时器 / 作业 / 恢复事件不经过这里，它们的槽位身份由
    // 各自的 build_*_event 显式清空（不写就报错，不继承上一条消息）。
    session.set_current(conversation_external_id_of(payload), sender_id_of(payload));

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    event.set("reply", host.get::<_, Function<'js>>("reply")?)?;
    event.set("log", host.get::<_, Function<'js>>("log")?)?;
    event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    event.set("device", host.get::<_, Object<'js>>("device")?)?;
    event.set("kv", host.get::<_, Object<'js>>("kv")?)?;
    event.set("history", host.get::<_, Function<'js>>("history")?)?;
    event.set("ai", host.get::<_, Object<'js>>("ai")?)?;
    event.set("jobs", host.get::<_, Object<'js>>("jobs")?)?;
    event.set("session", host.get::<_, Object<'js>>("session")?)?;
    // 出站是**平铺**在 ctx 上的两个函数（与 reply / history 同形），不是对象：
    // `dts` 的 message_ctx_functions 按「4 空格缩进且含左括号」收集它们。
    event.set("send", host.get::<_, Function<'js>>("send")?)?;
    event.set("notify", host.get::<_, Function<'js>>("notify")?)?;
    Ok(event)
}

/// The `botId` the event JSON carries, if it can be read as a [`BotId`].
///
/// Deliberately lenient about *shape* and strict about *content*: a missing,
/// non-string or unparsable value yields `None`, and the gate turns `None` into a
/// refusal (`crates::device::gate::DeviceFailure::NoBotIdentity`) rather than a
/// default session.
fn bot_id_of(payload: &str) -> Option<hbot_core::BotId> {
    let parsed: serde_json::Value = serde_json::from_str(payload).ok()?;
    let raw = parsed.get("botId")?.as_str()?;
    raw.parse().ok()
}

/// The `messageId` the event JSON carries, verbatim.
///
/// No parsing: the queue's idempotency key is opaque text (1–128 bytes) and the
/// platform's message id is already unique, so re-encoding it would only invent
/// a second identity to keep in sync. Missing or non-string yields `None`,
/// which makes `ctx.jobs.enqueue` demand an explicit `idempotencyKey` instead
/// of silently reusing a previous event's.
/// 这条事件属于哪个会话（平台 `externalId`，不是本地 `ConversationId`）。
///
/// 等待的归属与回复目标都用平台 ID：会话行可能还没建（第一条消息正在处理它），
/// 而 `externalId` 在那之前就已经有了。缺失或非字符串返回 `None`，槽位据此拒绝
/// `prompt`（而不是挂到一个空会话上）。
fn conversation_external_id_of(payload: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(payload).ok()?;
    parsed
        .get("conversationExternalId")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

/// 这条事件的发送者（平台 `senderId`）。
///
/// 与 [`conversation_external_id_of`] 同一口径：缺失时返回 `None`，槽位拒绝 `prompt`。
/// 一个没有发送者的消息事件不可能是「有人在等回话」。
fn sender_id_of(payload: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(payload).ok()?;
    parsed
        .get("senderId")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}
fn message_id_of(payload: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(payload).ok()?;
    parsed.get("messageId")?.as_str().map(str::to_owned)
}

/// The `conversationId` the event JSON carries, if it parses as a
/// [`hbot_core::ConversationId`].
///
/// Same leniency rules as [`bot_id_of`], and the same fail-closed outcome:
/// `ctx.history()` without it rejects with `HBOT_STATE_INVALID` rather than
/// reading whatever conversation the previous event used.
fn conversation_id_of(payload: &str) -> Option<hbot_core::ConversationId> {
    let parsed: serde_json::Value = serde_json::from_str(payload).ok()?;
    let raw = parsed.get("conversationId")?.as_str()?;
    raw.parse().ok()
}

/// Describe a failed JavaScript call, preferring the thrown object over the
/// generic `rquickjs::Error`, and remapping the stack back to TypeScript.
fn describe(ctx: &Ctx<'_>, error: rquickjs::Error, project: &CompiledProject) -> CodeError {
    let fallback = error.to_string();
    if !ctx.has_exception() {
        return CodeError::JsException {
            message: fallback,
            stack: None,
        };
    }

    let thrown = ctx.catch();
    let exception = Object::from_value(thrown)
        .ok()
        .and_then(Exception::from_object);
    let message = exception
        .as_ref()
        .and_then(Exception::message)
        .unwrap_or(fallback);
    let stack = exception
        .as_ref()
        .and_then(Exception::stack)
        .map(|stack| project.remap_stack(&stack));

    CodeError::JsException { message, stack }
}

/// A finished invocation, before the heap sample is attached.
struct RawEvent {
    logs: Vec<String>,
    replies: Vec<String>,
    duration: Duration,
    interrupted: bool,
    result: Result<(), CodeError>,
}

/// Attach the heap sample to one invocation's result.
///
/// The error branch carries `raw.logs` too: the handler ran, so whatever it
/// logged before failing is genuine diagnostic output. Dropping it here was the
/// bug that made "my bot threw and my three `ctx.log` lines vanished" possible.
fn finish(
    raw: RawEvent,
    memory: MemoryReport,
    limits: Limits,
) -> Result<EventOutcome, EventFailure> {
    match raw.result {
        Ok(()) => Ok(EventOutcome {
            logs: raw.logs,
            replies: raw.replies,
            duration: raw.duration,
            memory,
        }),
        Err(error) => Err(EventFailure {
            logs: raw.logs,
            error: Box::new(classify(error, raw.interrupted, limits)),
        }),
    }
}

/// Map "the isolate had to be interrupted" and "QuickJS ran out of heap" to
/// their dedicated error codes.
///
/// The interrupt flag is authoritative: a script may throw a `TypeError`
/// containing the word "timeout" and this must not be mistaken for one.
fn classify(error: CodeError, interrupted: bool, limits: Limits) -> CodeError {
    if interrupted {
        return CodeError::Timeout {
            budget_ms: limits.handler_timeout_ms(),
        };
    }

    if let CodeError::JsException { ref message, .. } = error
        && is_out_of_memory(message)
    {
        return CodeError::MemoryLimit {
            limit_bytes: u64::try_from(limits.memory_bytes).unwrap_or(u64::MAX),
        };
    }

    error
}

fn is_out_of_memory(message: &str) -> bool {
    message.to_ascii_lowercase().contains("out of memory")
}

/// Resolver: specifier → project-relative module name.
struct ProjectResolver {
    project: Arc<CompiledProject>,
}

impl ProjectResolver {
    fn new(project: Arc<CompiledProject>) -> Self {
        Self { project }
    }
}

impl Resolver for ProjectResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        // The same rule the compile-time gate applies, enforced again here so a
        // hand-built module table cannot smuggle a bare import past the AST scan
        // (`HBOT_BARE_IMPORT`).
        modules::resolve_module(base, name, |candidate| self.project.contains(candidate))
            .map_err(|error| rquickjs::Error::new_resolving_message(base, name, error.to_string()))
    }
}

/// Loader: module name → declared QuickJS module.
struct ProjectLoader {
    project: Arc<CompiledProject>,
}

impl ProjectLoader {
    fn new(project: Arc<CompiledProject>) -> Self {
        Self { project }
    }
}

impl Loader for ProjectLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        if name == modules::HOST_MODULE_ID {
            return Module::declare(ctx.clone(), name.to_owned(), host_api::HOST_MODULE_SOURCE);
        }

        let source = self
            .project
            .module(name)
            .map(|module| module.js.clone())
            .ok_or_else(|| {
                rquickjs::Error::new_loading_message(
                    name.to_owned(),
                    format!("module `{name}` is not in the compiled project"),
                )
            })?;

        Module::declare(ctx.clone(), name.to_owned(), source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance point for `Sandbox`'s auto traits.
    ///
    /// `hbot-rpc` boxes a sandbox into an executor that must be `Send + Sync`,
    /// and `Arc<Sandbox>: Send` additionally requires `Sandbox: Sync`. This
    /// failed when `finished: Receiver<()>` was stored bare: `Receiver` is
    /// `!Sync`, so the whole chain `Sandbox -> Arc<Sandbox> -> Worker` was
    /// `!Send` and the RPC crate could not compile. It is a compile-time
    /// assertion on purpose — a runtime test could pass while the bound rots.
    #[test]
    fn sandbox_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}

        assert_send_sync::<Sandbox>();
        // `Arc<T>: Send` needs `T: Send + Sync`, which is the bound the RPC
        // crate's executor actually requires of whatever it stores.
        assert_send::<Arc<Sandbox>>();
    }

    #[test]
    fn out_of_memory_messages_are_recognised() {
        assert!(is_out_of_memory("out of memory"));
        assert!(is_out_of_memory("InternalError: Out Of Memory"));
        assert!(!is_out_of_memory("timeout"));
        assert!(!is_out_of_memory("cannot read property of undefined"));
    }

    #[test]
    fn interrupt_flag_wins_over_the_message() {
        let limits = Limits::default();
        let error = CodeError::JsException {
            message: "timeout".to_owned(),
            stack: None,
        };
        assert_eq!(
            classify(error, true, limits).code(),
            crate::error::code::TIMEOUT
        );
    }

    #[test]
    fn out_of_memory_is_classified_as_memory_limit() {
        let limits = Limits::new().with_memory_bytes(1024);
        let error = CodeError::JsException {
            message: "out of memory".to_owned(),
            stack: None,
        };
        let classified = classify(error, false, limits);
        assert_eq!(classified.code(), crate::error::code::MEMORY_LIMIT);
        assert!(matches!(
            classified,
            CodeError::MemoryLimit { limit_bytes: 1024 }
        ));
    }
}
