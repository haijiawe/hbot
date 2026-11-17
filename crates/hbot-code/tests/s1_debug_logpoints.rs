//! S1 acceptance tests for **logpoints** on the instrumented (`debug`) build.
//!
//! A logpoint is a breakpoint that does not stop: the same probe fires, the
//! template is expanded against the current locals, the text goes out through
//! [`hbot_code::DebugSink::logged`], and the handler keeps running. These tests
//! exercise the real pipeline (TypeScript → SWC strip + probes → QuickJS → the
//! isolate thread) exactly like `s1_debug_instrument.rs`; nothing is mocked.
//!
//! The four things that must hold, and why each one is a real risk:
//!
//! 1. **It prints and does not pause.** The expensive failure mode is "a
//!    logpoint that stops", because a stopped handler is a bot that stops
//!    answering.
//! 2. **A throwing expression is local damage.** One bad expression must cost
//!    one `"<错误>"` fragment, not the whole message and never the handler.
//! 3. **Breakpoint + logpoint on one line prints first, then pauses.** The UI
//!    must already have the message when the paused frame arrives, otherwise the
//!    panel shows a stop with no explanation.
//! 4. **A broken template is silent, and says so once.** A typo must not cost a
//!    warning per hit (a loop would flood the log), must not stop the handler,
//!    and must not turn the *other* logpoints of the file off.
//!
//! Cost: the probe shape did not change, so a handler with no breakpoints and no
//! logpoints pays exactly what it paid before this feature (`debug idle`
//! 0× overhead: every probe is one `globalThis.__hbot_dbg` read that is `false`).
//! All logpoint work lives behind `__hbot_trace` returning `true`, i.e. inside
//! the slow path that already cost 4.64× (`docs/02-architecture.md` §6.5).
//! The template is compiled **once per `(file, line)`** in JS and cached, the
//! same shape as the condition cache in `debug_host.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use hbot_code::{
    Breakpoints, DebugAction, DebugSink, Limits, LoggedAt, PausedAt, Sandbox, compile_project_debug,
};
use serde_json::json;

/// A throwaway project directory, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Best effort: a leftover temp directory must not fail a test run.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn project(name: &str, files: &[(&str, &str)]) -> Project {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "hbot-s1-logpoints-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    for (relative, contents) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }
    Project(root)
}

/// 1-based line of the first line of `source` containing `needle`.
fn line_of(source: &str, needle: &str) -> u32 {
    let index = source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not in the fixture"));
    u32::try_from(index).expect("line fits in u32") + 1
}

fn event() -> serde_json::Value {
    json!({ "payload": { "text": "hi" } })
}

/// Handler with a local a template can read: `count === 3`.
const TEMPLATE_HOST: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    const count = 3;
    ctx.log(String(count));
    ctx.log('done');
  },
});
"#;

/// One thing the isolate told the sink, in arrival order.
///
/// Ordering is the point: "the message arrives before the pause" is a contract,
/// and two separate `Vec`s cannot assert it.
enum Seen {
    Logged(LoggedAt),
    Paused(PausedAt),
}

/// A [`DebugSink`] that records logs and stops in one ordered list.
#[derive(Default)]
struct Sink {
    seen: Mutex<Vec<Seen>>,
}

impl Sink {
    fn messages(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|seen| match seen {
                Seen::Logged(at) => Some(at.message.clone()),
                Seen::Paused(_) => None,
            })
            .collect()
    }

    fn stop_count(&self) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|seen| matches!(seen, Seen::Paused(_)))
            .count()
    }

    /// The lines the isolate stopped at, in arrival order.
    fn paused_lines(&self) -> Vec<u32> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|seen| match seen {
                Seen::Logged(_) => None,
                Seen::Paused(at) => Some(at.line),
            })
            .collect()
    }

    /// The sequence of kinds, e.g. `["logged", "paused"]`.
    fn order(&self) -> Vec<&'static str> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|seen| match seen {
                Seen::Logged(_) => "logged",
                Seen::Paused(_) => "paused",
            })
            .collect()
    }
}

impl DebugSink for Sink {
    fn paused(&self, at: PausedAt) -> DebugAction {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen::Paused(at));
        // Every logpoint test wants the handler to finish; a stop here is what
        // the test is asserting *against*.
        DebugAction::Continue
    }

    fn logged(&self, at: LoggedAt) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen::Logged(at));
    }
}

/// Runs one dispatch with a single logpoint on the `ctx.log(String(count))` line,
/// returning `(handler logs, sink, line)`.
fn run_with_template(name: &str, template: &str) -> (Vec<String>, Sink, u32) {
    let project = project(name, &[("src/main.ts", TEMPLATE_HOST)]);
    let line = line_of(TEMPLATE_HOST, "ctx.log(String(count))");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set_logs("src/main.ts", [(line, template.to_owned())]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::default();
    let outcome = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("a logpoint must never fail the handler");
    (outcome.logs, sink, line)
}

// ---------------------------------------------------------------------------
// 1. A logpoint expands its template and does not pause.
// ---------------------------------------------------------------------------

#[test]
fn a_logpoint_expands_the_template_and_does_not_pause() {
    let (logs, sink, line) = run_with_template("expand", "{count} items");

    assert_eq!(
        sink.messages(),
        vec!["3 items".to_owned()],
        "the template must expand against the handler's locals"
    );
    assert_eq!(
        sink.stop_count(),
        0,
        "a logpoint must never pause: a stopped handler is a bot that stops answering"
    );
    assert_eq!(
        sink.order(),
        vec!["logged"],
        "only the log should be emitted"
    );
    assert_eq!(
        logs,
        vec!["3", "done"],
        "the handler must run to completion after a logpoint"
    );

    // The event carries the source location, which is all the UI needs to jump.
    let seen = sink.seen.lock().unwrap_or_else(PoisonError::into_inner);
    let Seen::Logged(at) = &seen[0] else {
        panic!("expected a log as the first event");
    };
    assert_eq!(at.file, "src/main.ts");
    assert_eq!(at.line, line);
}

// ---------------------------------------------------------------------------
// 2. A throwing expression costs one fragment, not the message or the handler.
// ---------------------------------------------------------------------------

#[test]
fn a_throwing_expression_becomes_an_error_placeholder() {
    let (logs, sink, _line) = run_with_template("throws", "{missingHelper(count)} items");

    let messages = sink.messages();
    assert_eq!(
        messages.len(),
        1,
        "the logpoint must still print: {messages:#?}"
    );
    let message = &messages[0];
    assert!(
        message.contains("<错误>"),
        "a throwing expression must be replaced by the placeholder: {message:?}"
    );
    assert!(
        message.ends_with(" items"),
        "the rest of the template must survive: {message:?}"
    );
    assert_eq!(
        sink.stop_count(),
        0,
        "an expression error must not stop the handler"
    );
    assert_eq!(
        logs,
        vec!["3", "done"],
        "an expression error must not interrupt the handler"
    );
}

// ---------------------------------------------------------------------------
// 3. Breakpoint + logpoint on one line: print first, then pause.
// ---------------------------------------------------------------------------

#[test]
fn a_line_that_is_both_a_breakpoint_and_a_logpoint_logs_before_it_pauses() {
    let project = project("both", &[("src/main.ts", TEMPLATE_HOST)]);
    let line = line_of(TEMPLATE_HOST, "ctx.log(String(count))");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    // Two tables, same line: the breakpoint stops, the logpoint prints.
    breakpoints.set("src/main.ts", [(line, None)]);
    breakpoints.set_logs("src/main.ts", [(line, "n={count}".to_owned())]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::default();
    let outcome = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler completes after Continue");

    assert_eq!(
        sink.order(),
        vec!["logged", "paused"],
        "the message must be out before the paused frame arrives (the panel shows both)"
    );
    assert_eq!(sink.messages(), vec!["n=3".to_owned()]);
    assert_eq!(sink.stop_count(), 1, "the breakpoint half must still stop");
    assert_eq!(
        sink.paused_lines(),
        vec![line],
        "the stop must be at the very line that carried both"
    );
    assert_eq!(outcome.logs, vec!["3", "done"]);
}

// ---------------------------------------------------------------------------
// 4. A broken template is silent and warns exactly once.
// ---------------------------------------------------------------------------

/// All-thread log buffer: templates are expanded on the **isolate thread**, so a
/// thread-local subscriber would never see the warning.
static LOGS: std::sync::LazyLock<Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Clone)]
struct GlobalWriter;

impl Write for GlobalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for GlobalWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        Self
    }
}

fn install_log_capture() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(GlobalWriter)
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn captured_logs() -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap_or_else(PoisonError::into_inner)).to_string()
}

#[test]
fn a_broken_template_is_silent_and_warns_only_once() {
    install_log_capture();
    // `{ }` is a compile error (empty expression). Two dispatches share one
    // isolate ⇒ one failed compile ⇒ exactly one warning.
    let project = project("broken", &[("src/main.ts", TEMPLATE_HOST)]);
    let line = line_of(TEMPLATE_HOST, "ctx.log(String(count))");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set_logs("src/main.ts", [(line, "{ }".to_owned())]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::default();
    let first = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("a broken template must not fail the handler");
    let second = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("a broken template must not fail the handler");

    assert!(
        sink.messages().is_empty(),
        "a template that does not compile must stay silent: {:#?}",
        sink.messages()
    );
    assert_eq!(sink.stop_count(), 0, "a broken logpoint must not stop");
    assert_eq!(first.logs, vec!["3", "done"]);
    assert_eq!(second.logs, vec!["3", "done"]);

    let captured = captured_logs();
    let warnings = captured.matches("this logpoint is silent").count();
    assert_eq!(
        warnings, 1,
        "a broken template must warn once per line, not per hit:\n{captured}"
    );
}

// ---------------------------------------------------------------------------
// 5. Regression: with nothing set, the handler behaves exactly as before.
// ---------------------------------------------------------------------------

#[test]
fn nothing_changes_when_no_breakpoint_or_logpoint_is_set() {
    let project = project("quiet", &[("src/main.ts", TEMPLATE_HOST)]);
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    // No `set_breakpoints` call at all: the probe fast path must answer `false`
    // and nothing else may run.
    let plain = sandbox.dispatch(&event()).expect("handler runs");

    let sink = Sink::default();
    let instrumented = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler runs with a sink attached");

    assert_eq!(
        instrumented.logs, plain.logs,
        "an armed but uninteresting probe must not change behaviour"
    );
    assert_eq!(sink.order(), Vec::<&'static str>::new());
    assert_eq!(instrumented.logs, vec!["3", "done"]);
}
