//! The debugger's half of the host surface: `__hbot_dbg`, `__hbot_trace`,
//! `__hbot_pause` and `__hbot_stack`.
//!
//! [`crate::instrument`] emits probes that call into here; this module decides
//! whether a probe stops the isolate, builds the [`PausedAt`] frame the UI
//! shows, and blocks on [`DebugSink::paused`] until the user says what to do.
//!
//! # Why there is a "cheap" answer and an "expensive" answer
//!
//! A probe runs before *every* statement, so the common case (nothing is being
//! debugged, or nothing on this line should stop) must not touch user values.
//! `__hbot_trace` is that case: a handful of atomics and a `BTreeSet` lookup.
//! Only when it says "this might stop" does the probe construct the locals
//! closure, and only when `__hbot_pause` decides to really stop do we
//! stringify anything (`docs/02-architecture.md` §6.5: probes are O(1) unless a
//! breakpoint hits).
//!
//! # Logical call stack
//!
//! QuickJS has no debugger API, so "the stack" is read from
//! `new Error().stack` at the moment a probe is a stop candidate. That gives
//! the JS call stack, which is the logical stack the debugger wants — the frame
//! count is the depth used by step-over/step-out, and the frames become
//! [`PausedAt::call_stack`]. Two limits follow from that choice, both documented
//! in §6.5: an `await`ed continuation starts a fresh stack (no async stack), and
//! frames inside this module are filtered out because they are debugger
//! plumbing, not user code.
//!
//! # Threading
//!
//! Every function here runs on the isolate worker thread, inside the same
//! `Ctx` that runs the bot, so [`DebugSink::paused`] is called from the worker
//! thread and not from the thread that dispatched the event — exactly the
//! constraint in §6.5.1. The sink is handed over as a raw pointer by
//! [`DebugControl`]; see [`SinkSlot`] for the safety argument.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use hbot_core::debug::{DebugAction, DebugFrame, DebugScope, DebugSink, PausedAt};
use rquickjs::{Ctx, Function, Object, Value};
use serde_json::Value as Json;

use super::isolate::DebugState;
use super::js_error;
use crate::compile::CompiledProject;
use crate::error::CodeError;
use crate::instrument::{DEBUG_FLAG, PAUSE_FN, TRACE_FN};
use crate::limits::Deadline;

/// `globalThis.__hbot_stack` — the only way to read the JS stack from Rust.
pub(crate) const STACK_FN: &str = "__hbot_stack";

/// Source of [`STACK_FN`]. Installed once per context.
///
/// `Error.stack` is a QuickJS extension that exists because the engine is built
/// with `CONFIG_BIGNUM`/stack traces enabled; every frame is
/// `<name> (<module>:<line>:<column>)`.
///
/// The function expression is *named* on purpose: an anonymous function would
/// show up as `<anonymous>` in its own stack trace, which is exactly the frame
/// [`parse_stack`] filters out — leaving the debugger's own frame in the call
/// stack as if it were user code.
const STACK_SOURCE: &str = r#"(function __hbot_stack() {
    try {
        return String(new Error().stack || "");
    } catch (error) {
        return "";
    }
})"#;

/// Upper bound on the variables reported for one frame.
const MAX_VARIABLES: usize = 32;
/// Upper bound on one variable's JSON text. Larger values become
/// `<unserializable>` rather than blowing up the event payload.
const MAX_VALUE_BYTES: usize = 4096;

/// Non-serializable values are reported as this string, never dropped
/// (`hbot_core::debug` contract).
const UNSERIALIZABLE: &str = "<unserializable>";

/// Name a frame gets when QuickJS could not name it.
const ANONYMOUS: &str = "<anonymous>";

/// Everything a probe needs, cloned into the registered host functions.
#[derive(Clone)]
pub(crate) struct DebugRuntime {
    control: Arc<DebugControl>,
    state: Arc<Mutex<DebugState>>,
    project: Arc<CompiledProject>,
    deadline: Arc<Deadline>,
}

impl DebugRuntime {
    pub(crate) fn new(
        control: Arc<DebugControl>,
        state: Arc<Mutex<DebugState>>,
        project: Arc<CompiledProject>,
        deadline: Arc<Deadline>,
    ) -> Self {
        Self {
            control,
            state,
            project,
            deadline,
        }
    }

    /// Install `__hbot_dbg`, `__hbot_stack`, `__hbot_trace` and `__hbot_pause`
    /// on `globalThis`.
    ///
    /// These are internal globals, deliberately *not* part of `hbot.d.ts`: user
    /// code never calls them and the d.ts stays a description of the public
    /// contract (`docs/02-architecture.md` §6.1).
    pub(crate) fn install<'js>(&self, ctx: &Ctx<'js>) -> Result<(), CodeError> {
        let globals = ctx.globals();
        globals.set(DEBUG_FLAG, false).map_err(js_error)?;

        let stack: Function<'js> = ctx.eval(STACK_SOURCE).map_err(js_error)?;
        globals.set(STACK_FN, stack).map_err(js_error)?;

        let trace = self.clone();
        let trace_fn = Function::new(
            ctx.clone(),
            move |file: u32, line: u32, column: u32| -> bool {
                trace.should_stop(file, line, column)
            },
        )
        .map_err(js_error)?;
        globals.set(TRACE_FN, trace_fn).map_err(js_error)?;

        let pause = self.clone();
        let pause_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, file: u32, line: u32, column: u32, locals: Function<'js>| {
                pause.on_probe(&ctx, file, line, column, locals);
            },
        )
        .map_err(js_error)?;
        globals.set(PAUSE_FN, pause_fn).map_err(js_error)?;

        Ok(())
    }

    /// The O(1) question every probe asks: could anything stop on this line?
    ///
    /// Deliberately allocation-free and lock-free on the common path. It may
    /// answer `true` for a line that will not actually stop (a step in flight
    /// stops on an unknown line, and a file with breakpoints has them on other
    /// lines); [`Self::on_probe`] makes the real decision.
    fn should_stop(&self, file: u32, line: u32, _column: u32) -> bool {
        if !self.control.armed.load(Ordering::Acquire) {
            return false;
        }
        // An event that is already over budget must not pause: let the QuickJS
        // interrupt handler turn the overrun into `CodeError::Timeout` instead
        // of parking the isolate while the budget quietly runs out.
        if self.deadline.expired() {
            return false;
        }
        let stepping = self.control.stepping.load(Ordering::Acquire);
        if !stepping && !self.control.has_breakpoints.load(Ordering::Acquire) {
            return false;
        }
        let Some(path) = self.project.debug_info().and_then(|info| info.path(file)) else {
            return false;
        };
        if stepping {
            // A step stops on some *other* line; only a stack inspection can
            // tell which, so this is a candidate by definition.
            return true;
        }
        match self.state.lock() {
            Ok(state) => state.breakpoints.hits(path, line),
            Err(poisoned) => poisoned.into_inner().breakpoints.hits(path, line),
        }
    }

    /// The expensive half: decide, assemble the frame, and block on the sink.
    fn on_probe<'js>(
        &self,
        ctx: &Ctx<'js>,
        file: u32,
        line: u32,
        column: u32,
        locals: Function<'js>,
    ) {
        if !self.control.armed.load(Ordering::Acquire) {
            return;
        }
        let Some(path) = self
            .project
            .debug_info()
            .and_then(|info| info.path(file))
            .map(str::to_owned)
        else {
            return;
        };
        if self.deadline.expired() {
            return;
        }

        // A `Continue` resumes the *rest* of the statement it stopped on. If the
        // next statement shares that line (`a(); b();`), stopping again would
        // look like the debugger ignored the command, so the line stays
        // suppressed until execution reaches a different line. The cost of this
        // rule is that a loop written entirely on one line stops once per event
        // — documented in `docs/02-architecture.md` §6.5 (line granularity).
        let step = match self.control.session.lock() {
            Ok(mut session) => {
                if let Some((suppressed_file, suppressed_line)) = session.suppress.as_ref() {
                    if suppressed_file == &path && *suppressed_line == line {
                        return;
                    }
                    session.suppress = None;
                }
                session.step
            }
            Err(poisoned) => poisoned.into_inner().step,
        };

        let breakpoint = match self.state.lock() {
            Ok(state) => state.breakpoints.hits(&path, line),
            Err(poisoned) => poisoned.into_inner().breakpoints.hits(&path, line),
        };
        if !breakpoint && step.is_none() {
            return;
        }

        // Only now is it worth reading the stack, and only now does the locals
        // closure get called.
        let mut call_stack = self.capture_stack(ctx);
        let depth = call_stack.len().max(1);

        if let Some(step) = step {
            let reached = match step {
                Step::Into => true,
                Step::Over => depth <= self.session_depth(),
                Step::Out => depth < self.session_depth(),
            };
            if !reached {
                return;
            }
        }

        let variables = self.read_locals(ctx, locals);
        let frame_name = call_stack
            .last()
            .map(|frame| frame.name.clone())
            .unwrap_or_else(|| ANONYMOUS.to_owned());

        // `call_stack` is innermost-first from QuickJS; the contract wants the
        // outermost frame first with the current one last.
        call_stack.reverse();
        match call_stack.last_mut() {
            // The probe knows the exact source position; the stack only knows
            // where in the *generated* line the call happened.
            Some(current) => {
                current.file = path.clone();
                current.line = line;
                current.column = column;
            }
            None => call_stack.push(DebugFrame {
                name: frame_name.clone(),
                file: path.clone(),
                line,
                column,
            }),
        }

        let at = PausedAt {
            file: path.clone(),
            line,
            column,
            scopes: vec![DebugScope {
                name: frame_name,
                variables,
            }],
            call_stack,
        };

        if let Ok(mut state) = self.state.lock() {
            state.paused = true;
        }
        // The sink is called without holding `DebugState`: a sink that calls
        // `Sandbox::set_breakpoints` (a natural thing for a debugger UI to do)
        // would otherwise deadlock on this thread.
        let action = self.control.call_sink(at);
        if let Ok(mut state) = self.state.lock() {
            state.paused = false;
        }

        self.record_action(&path, line, depth, action);
    }

    fn session_depth(&self) -> usize {
        match self.control.session.lock() {
            Ok(session) => session.depth,
            Err(poisoned) => poisoned.into_inner().depth,
        }
    }

    /// Turn the sink's answer into the next stop condition.
    fn record_action(&self, path: &str, line: u32, depth: usize, action: DebugAction) {
        let Ok(mut session) = self.control.session.lock() else {
            return;
        };
        match action {
            DebugAction::Continue => {
                session.step = None;
                session.suppress = Some((path.to_owned(), line));
            }
            DebugAction::StepOver => {
                session.step = Some(Step::Over);
                session.depth = depth;
                session.suppress = None;
            }
            DebugAction::StepInto => {
                session.step = Some(Step::Into);
                session.depth = depth;
                session.suppress = None;
            }
            DebugAction::StepOut => {
                // There is no frame above the entry handler: nothing could ever
                // satisfy `depth < 1`, so treat it as "run to the end" rather
                // than leaving a step armed forever.
                if depth > 1 {
                    session.step = Some(Step::Out);
                    session.depth = depth;
                    session.suppress = None;
                } else {
                    session.step = None;
                    session.suppress = Some((path.to_owned(), line));
                }
            }
        }
        self.control
            .stepping
            .store(session.step.is_some(), Ordering::Release);
    }

    /// Read `globalThis.__hbot_stack()` and resolve every frame to TypeScript.
    ///
    /// Returns frames innermost-first, debugger frames removed.
    fn capture_stack<'js>(&self, ctx: &Ctx<'js>) -> Vec<DebugFrame> {
        let Ok(stack) = ctx.globals().get::<_, Function<'js>>(STACK_FN) else {
            return Vec::new();
        };
        let text: rquickjs::String<'js> = match stack.call(()) {
            Ok(text) => text,
            Err(_) => {
                // Reading our own helper threw, which is not user code failing:
                // clear it so the handler keeps running with an empty stack.
                let _ = ctx.catch();
                return Vec::new();
            }
        };
        let text = text.to_string().unwrap_or_default();
        parse_stack(&text, &self.project)
    }

    /// Call the locals closure and keep whatever is JSON-serializable.
    ///
    /// A variable that throws while being read (temporal dead zone, a proxy
    /// trap) is already dropped by the closure; a value that cannot be
    /// stringified is reported as `<unserializable>` rather than silently
    /// disappearing (`hbot_core::debug` contract).
    fn read_locals<'js>(&self, ctx: &Ctx<'js>, locals: Function<'js>) -> BTreeMap<String, Json> {
        let mut variables = BTreeMap::new();
        let object: Object<'js> = match locals.call(()) {
            Ok(object) => object,
            Err(_) => {
                let _ = ctx.catch();
                return variables;
            }
        };
        for key in object.keys::<String>() {
            if variables.len() >= MAX_VARIABLES {
                break;
            }
            let Ok(key) = key else {
                continue;
            };
            let rendered = match object.get::<_, Value<'js>>(key.as_str()) {
                Ok(value) => render_value(ctx, value),
                Err(_) => Json::String(UNSERIALIZABLE.to_owned()),
            };
            variables.insert(key, rendered);
        }
        if ctx.has_exception() {
            // Nothing above may leave a pending exception: the next statement
            // in the user's handler would inherit it.
            let _ = ctx.catch();
        }
        variables
    }
}

fn render_value<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Json {
    match ctx.json_stringify(value) {
        Ok(Some(text)) => {
            let text = text.to_string().unwrap_or_default();
            if text.len() > MAX_VALUE_BYTES {
                return Json::String(UNSERIALIZABLE.to_owned());
            }
            serde_json::from_str(&text).unwrap_or(Json::String(UNSERIALIZABLE.to_owned()))
        }
        _ => Json::String(UNSERIALIZABLE.to_owned()),
    }
}

/// Parse a QuickJS stack trace into frames.
///
/// QuickJS writes one line per frame:
///
/// ```text
///     at helper (src/lib.ts:12:9)
///     at onMessage (src/main.ts:4:3)
///     at <anonymous> (native)
/// ```
///
/// Frames without a source position, and frames belonging to this module's own
/// helpers (they are the debugger, not the program), are dropped.
fn parse_stack(stack: &str, project: &CompiledProject) -> Vec<DebugFrame> {
    let mut frames = Vec::new();
    for line in stack.lines() {
        let Some(rest) = line.trim().strip_prefix("at ") else {
            continue;
        };
        // `name (location)` — the location is the part after the last " (".
        let Some(open) = rest.rfind(" (") else {
            continue;
        };
        let name = rest[..open].trim();
        let location = rest[open + 2..].trim();
        let Some(location) = location.strip_suffix(')') else {
            continue;
        };
        if location == "native" || location == "missing" {
            continue;
        }
        if name.starts_with("__hbot") {
            continue;
        }
        // `module:line:column`, and the module name itself may contain ':' on
        // Windows-style paths, so split from the right.
        let mut parts = location.rsplitn(3, ':');
        let (Some(column), Some(line), Some(module)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(generated_line), Ok(generated_column)) =
            (line.parse::<u32>(), column.parse::<u32>())
        else {
            continue;
        };
        let (source_line, source_column) = project
            .source_position(module, generated_line)
            .unwrap_or((generated_line, generated_column));
        frames.push(DebugFrame {
            name: if name.is_empty() {
                ANONYMOUS.to_owned()
            } else {
                name.to_owned()
            },
            file: module.to_owned(),
            line: source_line,
            column: source_column,
        });
    }
    frames
}

/// One outstanding step request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Stop at the next statement of this frame or shallower.
    Over,
    /// Stop at the next statement, deeper frames included.
    Into,
    /// Stop at the next statement of a shallower frame.
    Out,
}

#[derive(Debug, Default)]
struct Session {
    /// `None` means "run until the next breakpoint".
    step: Option<Step>,
    /// Stack depth recorded when `step` was requested.
    depth: usize,
    /// `(file, line)` that must not stop again — see [`DebugRuntime::on_probe`].
    suppress: Option<(String, u32)>,
}

/// Debugger state shared between the thread that dispatches an event and the
/// isolate worker thread.
#[derive(Debug, Default)]
pub(crate) struct DebugControl {
    /// True while an event is being run *for a debugger*. Read by every probe.
    armed: AtomicBool,
    /// Mirrors "the breakpoint set is not empty" so the fast path needs no lock.
    has_breakpoints: AtomicBool,
    /// True while a step request is outstanding.
    stepping: AtomicBool,
    /// The sink borrowed for the duration of one debug dispatch.
    sink: Mutex<Option<SinkSlot>>,
    /// Serializes debug dispatches: one sink slot, one pause at a time.
    dispatch: Mutex<()>,
    session: Mutex<Session>,
}

impl DebugControl {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publish whether there are breakpoints, for the probe fast path.
    pub(crate) fn note_breakpoints(&self, any: bool) {
        self.has_breakpoints.store(any, Ordering::Release);
    }

    /// Prepare for an event.
    ///
    /// Called on the worker thread before the handler runs. `armed` becomes
    /// `true` only when a sink is installed, so `dispatch` and
    /// `dispatch_with_debug(event, None)` behave identically.
    pub(crate) fn begin_event(&self, armed: bool) {
        if let Ok(mut session) = self.session.lock() {
            session.step = None;
            session.depth = 0;
            session.suppress = None;
        }
        self.stepping.store(false, Ordering::Release);
        self.armed.store(armed, Ordering::Release);
    }

    /// Retire the event's debug state.
    pub(crate) fn end_event(&self) {
        self.armed.store(false, Ordering::Release);
        self.stepping.store(false, Ordering::Release);
        if let Ok(mut session) = self.session.lock() {
            session.step = None;
            session.depth = 0;
            session.suppress = None;
        }
    }

    /// Take the dispatch lock and install `sink` for the next event.
    ///
    /// The guard is held until [`Self::clear_sink`] runs, so no second debug
    /// dispatch can replace the slot while the worker may still call it.
    pub(crate) fn begin_dispatch(&self, sink: Option<&dyn DebugSink>) -> MutexGuard<'_, ()> {
        let guard = match self.dispatch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Ok(mut slot) = self.sink.lock() {
            *slot = sink.map(SinkSlot::new);
        }
        guard
    }

    /// Drop the borrowed sink.
    ///
    /// Waits for the sink lock, so if the worker is inside
    /// [`DebugSink::paused`] this blocks until it returns — which is what makes
    /// the borrow installed by [`Self::begin_dispatch`] sound.
    pub(crate) fn clear_sink(&self) {
        match self.sink.lock() {
            Ok(mut slot) => *slot = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }

    fn call_sink(&self, at: PausedAt) -> DebugAction {
        let guard = match self.sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(slot) = guard.as_ref() else {
            return DebugAction::Continue;
        };
        // A panicking sink must not unwind through the QuickJS C frames and
        // leave the isolate wedged: the handler keeps running instead.
        //
        // SAFETY: the sink lock is held and the slot is `Some`, which is exactly
        // the window described on [`SinkSlot`].
        match panic::catch_unwind(AssertUnwindSafe(|| unsafe { slot.sink() }.paused(at))) {
            Ok(action) => action,
            Err(_) => {
                tracing::error!("debug sink panicked; continuing the handler");
                DebugAction::Continue
            }
        }
    }
}

/// A borrowed [`DebugSink`] that outlives the call by contract rather than by
/// its Rust lifetime.
///
/// The borrow is `Option<&dyn DebugSink>` in
/// [`crate::Sandbox::dispatch_with_debug`], but the sink has to be called on the
/// isolate worker thread, which needs a `Send` value. The safety argument is a
/// scoped-thread one:
///
/// * the slot is only filled by `begin_dispatch`, which holds the dispatch lock
///   and therefore keeps the caller (and its borrow) alive;
/// * the worker only dereferences the pointer while holding the sink lock;
/// * `clear_sink` takes that same lock before the caller returns, so it cannot
///   finish while the worker is still inside [`DebugSink::paused`];
/// * after `clear_sink`, the worker sees `None` and never touches the pointer.
///
/// The blunt consequence: `DebugSink::paused` must itself be bounded, because
/// `dispatch_with_debug` waits for it. `hbot_core::debug` requires that of every
/// implementation (5 minutes).
#[derive(Debug)]
struct SinkSlot(*const (dyn DebugSink + 'static));

// SAFETY: see the type comment. The pointer is never dereferenced outside the
// window described above, and `DebugSink: Send + Sync` guarantees the pointee
// itself can be used from another thread.
unsafe impl Send for SinkSlot {}

impl SinkSlot {
    fn new(sink: &dyn DebugSink) -> Self {
        // SAFETY: the lifetime is erased here and re-established by the slot
        // protocol above; `clear_sink` runs before the borrow can expire.
        let pointer: *const (dyn DebugSink + '_) = sink;
        Self(unsafe {
            std::mem::transmute::<*const (dyn DebugSink + '_), *const (dyn DebugSink + 'static)>(
                pointer,
            )
        })
    }

    /// # Safety
    ///
    /// Caller must hold the sink lock that [``DebugControl::clear_sink``] takes,
    /// and the borrowed sink must still be alive (see the type comment).
    unsafe fn sink(&self) -> &(dyn DebugSink + 'static) {
        // SAFETY: delegated to the caller by this function's contract.
        unsafe { &*self.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(files: &[(&str, &str)]) -> CompiledProject {
        use std::sync::atomic::AtomicU32;
        // Tests run in parallel, so the directory name must not be derived from
        // the fixture contents alone: two fixtures with the same shape would
        // share (and delete) each other's directory.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "hbot-code-debug-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (name, content) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(&path, content).expect("fixture");
        }
        let project = crate::compile::compile_project_debug(&root).expect("compiles");
        let _ = std::fs::remove_dir_all(&root);
        project
    }

    #[test]
    fn stack_frames_are_parsed_and_resolved_to_typescript() {
        let project = project_with(&[(
            "src/main.ts",
            "export function onMessage(ctx: any) {\n  const a = 1;\n  ctx.log(String(a));\n}\n",
        )]);
        // Instrumentation inserts a probe before every statement, so the
        // generated line of `ctx.log` is not the source line any more: find it
        // in the emitted JavaScript, then let `parse_stack` map it back. The
        // assertion is that the frame is reported at the *original* line 3 — a
        // breakpoint set there is what the probe has to match.
        let module = project.module("src/main.ts").expect("module");
        let generated_line = u32::try_from(
            module
                .js
                .lines()
                .position(|line| line.contains("ctx.log"))
                .expect("statement in emitted js"),
        )
        .expect("line fits")
            + 1;
        assert_ne!(generated_line, 3, "the probe should have shifted the line");

        let stack = format!(
            "    at onMessage (src/main.ts:{generated_line}:3)\n    at <anonymous> (native)\n"
        );
        let frames = parse_stack(&stack, &project);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].name, "onMessage");
        assert_eq!(frames[0].file, "src/main.ts");
        assert_eq!(frames[0].line, 3);
    }

    #[test]
    fn debugger_frames_and_native_frames_are_dropped() {
        let project = project_with(&[(
            "src/main.ts",
            "export function onMessage() {\n  const a = 1;\n}\n",
        )]);
        let stack = "    at __hbot_stack (nofile:1:1)\n    at __hbot_pause (native)\n    \
                     at onMessage (src/main.ts:2:3)\n    at missing (missing)\n";
        let frames = parse_stack(stack, &project);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].name, "onMessage");
    }

    #[test]
    fn a_frame_that_does_not_exist_in_the_project_still_parses() {
        let project = project_with(&[("src/main.ts", "export function onMessage() {}\n")]);
        let frames = parse_stack("    at go (src/gone.ts:9:4)\n", &project);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].file, "src/gone.ts");
        // No mapping exists, so the generated position is the best answer.
        assert_eq!((frames[0].line, frames[0].column), (9, 4));
    }

    #[test]
    fn flag_transitions_are_visible_to_the_fast_path() {
        let control = DebugControl::new();
        assert!(!control.armed.load(Ordering::Acquire));
        control.begin_event(true);
        assert!(control.armed.load(Ordering::Acquire));
        control.note_breakpoints(true);
        assert!(control.has_breakpoints.load(Ordering::Acquire));
        control.end_event();
        assert!(!control.armed.load(Ordering::Acquire));
        assert!(!control.stepping.load(Ordering::Acquire));
    }
}
