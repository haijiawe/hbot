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

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::MaybePromise;
use rquickjs::{
    AsyncContext, AsyncRuntime, Ctx, Exception, Function, Module, Object, Persistent, Value,
};

use crate::compile::CompiledProject;
use crate::error::CodeError;
use crate::limits::{Deadline, Limits};
use crate::modules;

use super::host_api::{self, HostOutput};
use super::{EventOutcome, MemoryReport, js_error};

/// How many commands may queue up before `dispatch` blocks.
///
/// Per-conversation serialization lives one layer up (`hbot-bot`); this is only
/// the isolate's inbox, and it stays small on purpose so a burst cannot grow
/// host memory without bound.
const COMMAND_BUFFER: usize = 8;

/// Prefix that turns a project-relative module name into a relative specifier.
///
/// QuickJS resolves the entry against an empty base when the import comes from
/// Rust, so it must read `./src/main.ts` to take the relative branch of
/// [`modules::resolve_module`].
const ENTRY_SPECIFIER_PREFIX: &str = "./";

/// A handle to a running isolate.
///
/// Dropping the handle shuts the isolate down and joins its thread.
pub struct Sandbox {
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
    limits: Limits,
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
        let (commands, inbox) = std::sync::mpsc::sync_channel(COMMAND_BUFFER);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker_project = Arc::new(project);
        let worker = std::thread::Builder::new()
            .name("hbot-isolate".to_owned())
            .spawn(move || worker(&inbox, worker_project, limits, &started_tx))
            .map_err(|error| CodeError::SandboxDead {
                message: format!("cannot spawn isolate thread: {error}"),
            })?;

        let sandbox = Self {
            commands,
            worker: Some(worker),
            limits,
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

    /// Run `onMessage` for one JSON event.
    ///
    /// This is a blocking call: use it from `spawn_blocking` or a dedicated
    /// thread, never from an async reactor thread. The isolate enforces
    /// [`Limits::handler_timeout`]; this method only holds a longer backstop in
    /// case the worker thread is wedged.
    ///
    /// # Errors
    ///
    /// [`CodeError::JsException`], [`CodeError::Timeout`],
    /// [`CodeError::MemoryLimit`], or [`CodeError::SandboxDead`] when the
    /// isolate is gone.
    pub fn dispatch(&self, event: &serde_json::Value) -> Result<EventOutcome, CodeError> {
        let payload = serde_json::to_string(event).map_err(|error| CodeError::JsException {
            message: format!("event is not JSON-serialisable: {error}"),
            stack: None,
        })?;
        let (reply, result) = std::sync::mpsc::channel();
        self.commands
            .send(Command::Event { payload, reply })
            .map_err(|_| self.dead())?;

        match result.recv_timeout(self.backstop()) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => Err(CodeError::SandboxDead {
                message: format!(
                    "isolate did not answer within {:?}; the worker thread is wedged",
                    self.backstop()
                ),
            }),
            Err(RecvTimeoutError::Disconnected) => Err(self.dead()),
        }
    }

    /// Current heap usage of the isolate.
    ///
    /// # Errors
    ///
    /// [`CodeError::SandboxDead`] when the isolate is gone.
    pub fn memory(&self) -> Result<MemoryReport, CodeError> {
        let (reply, result) = std::sync::mpsc::channel();
        self.commands
            .send(Command::Memory { reply })
            .map_err(|_| self.dead())?;
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

    fn backstop(&self) -> Duration {
        self.limits
            .handler_timeout
            .saturating_mul(3)
            .saturating_add(Duration::from_secs(1))
    }

    fn dead(&self) -> CodeError {
        CodeError::SandboxDead {
            message: "isolate worker stopped".to_owned(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Ask the worker to stop; dropping the sender afterwards also unblocks a
        // worker that is waiting in `recv`.
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            // A panicking worker is a bug, and it is also invisible to the
            // caller because the panic happened on another thread. Record it
            // instead of swallowing it silently.
            tracing::error!("hbot-isolate thread panicked");
        }
    }
}

/// A request from the owner thread to the isolate thread.
enum Command {
    /// Run the handler for `payload` and answer on `reply`.
    Event {
        payload: String,
        reply: Sender<Result<EventOutcome, CodeError>>,
    },
    /// Report heap usage.
    Memory {
        reply: Sender<Result<MemoryReport, CodeError>>,
    },
    /// Stop the worker loop.
    Shutdown,
}

/// Thread body: build the realm once, then serve commands until shutdown.
fn worker(
    inbox: &Receiver<Command>,
    project: Arc<CompiledProject>,
    limits: Limits,
    started: &Sender<Result<(), CodeError>>,
) {
    if let Err(error) = serve(inbox, project, limits, started) {
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
        for command in inbox.iter() {
            reject(command, &failure);
        }
    }
}

fn reject(command: Command, failure: &CodeError) {
    let message = failure.to_string();
    match command {
        Command::Event { reply, .. } => {
            let _ = reply.send(Err(CodeError::SandboxDead { message }));
        }
        Command::Memory { reply } => {
            let _ = reply.send(Err(CodeError::SandboxDead { message }));
        }
        Command::Shutdown => {}
    }
}

fn trace(step: &str) {
    if std::env::var_os("HBOT_S1_TRACE").is_some() {
        eprintln!("[s1] {step}");
    }
}

fn serve(
    inbox: &Receiver<Command>,
    project: Arc<CompiledProject>,
    limits: Limits,
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
    let handler = {
        let context = &context;
        let output = &output;
        let deadline = &deadline;
        local.block_on(&runtime, async move {
            context
                .async_with(async |ctx| bootstrap(&ctx, &entry, output, deadline, limits).await)
                .await
        })?
    };

    // Only now is the bot callable; unblock `Sandbox::spawn`.
    let _ = started.send(Ok(()));

    while let Ok(command) = inbox.recv() {
        match command {
            Command::Shutdown => break,
            Command::Memory { reply } => {
                let usage = local.block_on(&runtime, async { quicksjs.memory_usage().await });
                let _ = reply.send(Ok(memory_report(usage, limits)));
            }
            Command::Event { payload, reply } => {
                let raw = {
                    let context = &context;
                    let handler = &handler;
                    let output = &output;
                    let deadline = &deadline;
                    let project = &project;
                    local.block_on(&runtime, async move {
                        context
                            .async_with(async |ctx| {
                                run_event(
                                    &ctx, &payload, handler, output, deadline, project, limits,
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

/// Evaluate `src/main.ts` and capture its `onMessage` function.
async fn bootstrap<'js>(
    ctx: &Ctx<'js>,
    entry: &str,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    limits: Limits,
) -> Result<Persistent<Function<'static>>, CodeError> {
    deadline.reset();
    deadline.arm(limits.handler_timeout);

    let result = async {
        host_api::install(ctx, Arc::clone(output), Arc::clone(deadline))?;
        trace("host api installed");

        // The single `Module::import` that kicks off the loader chain; every
        // further module is pulled in by QuickJS itself through
        // `ProjectResolver`/`ProjectLoader`.
        let namespace = Module::import(ctx, entry.to_owned())
            .map_err(js_error)?
            .into_future::<Object<'js>>()
            .await
            .map_err(js_error)?;
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

        Ok::<_, CodeError>(Persistent::save(ctx, handler))
    }
    .await;

    deadline.disarm();
    let interrupted = deadline.take_interrupted();
    result.map_err(|error| classify(error, interrupted, limits))
}

/// One handler invocation, with the deadline armed and host output captured.
async fn run_event<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
    output: &Arc<Mutex<HostOutput>>,
    deadline: &Arc<Deadline>,
    project: &CompiledProject,
    limits: Limits,
) -> RawEvent {
    deadline.reset();
    deadline.arm(limits.handler_timeout);
    output
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();

    let started = Instant::now();
    let result = invoke(ctx, payload, handler, project).await;
    let duration = started.elapsed();

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
async fn invoke<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
    project: &CompiledProject,
) -> Result<(), CodeError> {
    match invoke_inner(ctx, payload, handler).await {
        Ok(()) => Ok(()),
        Err(error) => Err(describe(ctx, error, project)),
    }
}

async fn invoke_inner<'js>(
    ctx: &Ctx<'js>,
    payload: &str,
    handler: &Persistent<Function<'static>>,
) -> rquickjs::Result<()> {
    let event = build_event(ctx, payload)?;
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
/// (`ctx.reply`, `ctx.log`, `ctx.sleep`).
fn build_event<'js>(ctx: &Ctx<'js>, payload: &str) -> rquickjs::Result<Object<'js>> {
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

    let host: Object<'js> = ctx.globals().get("__hbot")?;
    event.set("reply", host.get::<_, Function<'js>>("reply")?)?;
    event.set("log", host.get::<_, Function<'js>>("log")?)?;
    event.set("sleep", host.get::<_, Function<'js>>("sleep")?)?;
    Ok(event)
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

fn finish(raw: RawEvent, memory: MemoryReport, limits: Limits) -> Result<EventOutcome, CodeError> {
    match raw.result {
        Ok(()) => Ok(EventOutcome {
            logs: raw.logs,
            replies: raw.replies,
            duration: raw.duration,
            memory,
        }),
        Err(error) => Err(classify(error, raw.interrupted, limits)),
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
