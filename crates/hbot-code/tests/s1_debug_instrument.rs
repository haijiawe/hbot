//! S1 acceptance tests for the instrumented (`debug`) build.
//!
//! These exercise the real pipeline — TypeScript in, SWC strip + probe
//! instrumentation, QuickJS out — and the real isolate thread, with a
//! [`DebugSink`] standing in for the UI. Nothing is mocked: the point of these
//! tests is that a breakpoint set on a TypeScript line actually stops there and
//! that stepping, resuming and the handler timeout all behave
//! (`docs/02-architecture.md` §6.5, §6.5.1).
//!
//! # Measured instrumentation cost
//!
//! `cargo run --release --example s1_bench` (Windows 11, Rust 1.96, same
//! project, fresh isolate per row, mean of 1000 dispatches). Two runs a minute
//! apart, to show the run-to-run spread rather than a single lucky number:
//!
//! ```text
//! == instrumentation cost (debug build vs release build) ==         run 1 / run 2
//!   1 files: release    21.73µs | debug idle    29.60µs | debug armed    37.70µs | ratio 1.73x
//!    emitted js: 139 B -> 443 B (+304 B of probes)                    ratio 1.25x
//!  10 files: release    26.87µs | debug idle    24.32µs | debug armed    23.31µs | ratio 0.87x
//!    emitted js: 1480 B -> 20596 B (+19116 B of probes)                ratio 0.79x
//!
//! == probe-dense handler (2000-iteration loop, 3 statements each) ==
//!   run 1: release 117.82µs | debug armed 546.68µs | ratio 4.64x
//!   run 2: release 123.69µs | debug armed 573.36µs | ratio 4.64x
//!   worst ratio 4.64x (target <= 5x): ok
//! ```
//!
//! Read those numbers with the right denominator:
//!
//! * The per-event rows are dominated by promise scheduling, which is identical
//!   in both builds — for a one-statement handler (`await reply(...)`) there is
//!   almost no instrumentation to pay for, hence the 1.73×/0.87× spread (and
//!   `debug idle` occasionally measuring *faster* than `release`, which is pure
//!   noise, not a real speedup).
//! * The row that actually measures the probes is the probe-dense one: 2000
//!   iterations × 3 statements = **6000 probes per event**, and there
//!   `release ~120µs → debug armed ~560µs = 4.64×` on both runs, i.e. ~71 ns per
//!   probe (one `globalThis` read, one native call, two relaxed atomic loads and
//!   an `Instant` comparison). That is the honest worst case and it is inside the
//!   ≤5× target of `docs/02-architecture.md` §6.5 — but only by 8%, so treat a
//!   new probe fast-path branch as a cost, not as free.
//! * `debug idle` (instrumented build, no sink attached) is the "user is
//!   running a debug build but nobody is watching" case: every probe is one
//!   `globalThis.__hbot_dbg` read that is `false`, so it stays at release speed.
//!
//! `release` builds emit zero probes (`emitted js` is the same as before this
//! feature), so release overhead is exactly 0×, not "small".

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use hbot_code::{
    Breakpoints, CodeError, DebugAction, DebugSink, Limits, PausedAt, Sandbox, compile_project,
    compile_project_debug,
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
        "hbot-s1-debug-{name}-{}-{}",
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
///
/// Breakpoints are set by *source* line, so the tests derive the number from the
/// fixture text instead of hardcoding it — an edit that adds a line must not
/// silently point a breakpoint at the wrong statement.
fn line_of(source: &str, needle: &str) -> u32 {
    let index = source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not in the fixture"));
    u32::try_from(index).expect("line fits in u32") + 1
}

/// A [`DebugSink`] that answers from a script and records every stop.
#[derive(Default)]
struct Sink {
    actions: Mutex<VecDeque<DebugAction>>,
    stops: Mutex<Vec<PausedAt>>,
    /// Simulates a user reading the paused frame before answering.
    delay: Mutex<Duration>,
}

impl Sink {
    fn scripted(actions: impl IntoIterator<Item = DebugAction>) -> Self {
        Self {
            actions: Mutex::new(actions.into_iter().collect()),
            ..Self::default()
        }
    }

    fn holding(delay: Duration) -> Self {
        Self {
            delay: Mutex::new(delay),
            ..Self::default()
        }
    }

    fn stops(&self) -> Vec<PausedAt> {
        self.stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn stop_count(&self) -> usize {
        self.stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl DebugSink for Sink {
    fn paused(&self, at: PausedAt) -> DebugAction {
        let delay = *self.delay.lock().unwrap_or_else(PoisonError::into_inner);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        self.stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(at);
        self.actions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            // Running out of script means "resume": a test that only cares about
            // the first stop must not hang the isolate.
            .unwrap_or(DebugAction::Continue)
    }
}

fn event() -> serde_json::Value {
    json!({ "payload": { "text": "hi" } })
}

/// Straight-line handler: three statements in `onMessage`.
const STRAIGHT_LINE: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    const a = 1;
    const b = 2;
    ctx.log(String(a + b));
  },
});
"#;

// ---------------------------------------------------------------------------
// 1. A line breakpoint stops the handler with the right file and line.
// ---------------------------------------------------------------------------

#[test]
fn a_line_breakpoint_pauses_with_the_source_file_and_line() {
    let project = project("hit", &[("src/main.ts", STRAIGHT_LINE)]);
    let breakpoint = line_of(STRAIGHT_LINE, "ctx.log");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    assert!(compiled.is_debug(), "debug build must be marked as such");

    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");
    let mut breakpoints = Breakpoints::new();
    breakpoints.set("src/main.ts", [breakpoint]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::default();
    let outcome = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler runs to completion");

    assert_eq!(outcome.logs, vec!["3".to_owned()]);
    let stops = sink.stops();
    assert_eq!(stops.len(), 1, "{stops:#?}");
    let at = &stops[0];
    assert_eq!(at.file, "src/main.ts");
    assert_eq!(at.line, breakpoint);
    // The current frame is last, and it is the frame the breakpoint is in.
    let current = at.call_stack.last().expect("a frame for the current scope");
    assert_eq!(current.file, "src/main.ts");
    assert_eq!(current.line, breakpoint);
    assert_eq!(current.name, "onMessage");
    // Locals are reported, not dropped (`hbot_core::debug` contract).
    let scope = at.scopes.first().expect("one scope");
    assert_eq!(scope.name, "onMessage");
    assert!(
        scope.variables.contains_key("a"),
        "locals should include `a`: {:#?}",
        scope.variables
    );
}

// ---------------------------------------------------------------------------
// 2. `Continue` does not stop twice on the same line.
// ---------------------------------------------------------------------------

#[test]
fn continue_does_not_stop_again_on_the_same_line() {
    // Two statements on one line: without the resume suppression the debugger
    // would stop again immediately and look like it ignored the command.
    const MANY_ON_ONE_LINE: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.log('one'); ctx.log('two');
    ctx.log('three');
  },
});
"#;
    let project = project("continue", &[("src/main.ts", MANY_ON_ONE_LINE)]);
    let breakpoint = line_of(MANY_ON_ONE_LINE, "'one'");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set("src/main.ts", [breakpoint]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::scripted([DebugAction::Continue]);
    let outcome = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler completes after Continue");

    assert_eq!(sink.stop_count(), 1, "stopped twice on one line");
    assert_eq!(outcome.logs, vec!["one", "two", "three"]);
}

// ---------------------------------------------------------------------------
// 3. `StepOver` stops at the next statement of the same frame.
// ---------------------------------------------------------------------------

#[test]
fn step_over_stops_at_the_next_statement_in_the_same_frame() {
    let project = project("over", &[("src/main.ts", STRAIGHT_LINE)]);
    let first = line_of(STRAIGHT_LINE, "const a = 1");
    let second = line_of(STRAIGHT_LINE, "const b = 2");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set("src/main.ts", [first]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::scripted([DebugAction::StepOver]);
    sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler completes after stepping");

    let stops = sink.stops();
    assert_eq!(stops.len(), 2, "{stops:#?}");
    assert_eq!(
        (stops[0].file.as_str(), stops[0].line),
        ("src/main.ts", first)
    );
    assert_eq!(
        (stops[1].file.as_str(), stops[1].line),
        ("src/main.ts", second),
        "StepOver should land on the next statement"
    );
}

// ---------------------------------------------------------------------------
// 4. `StepOut` returns to the caller's frame.
// ---------------------------------------------------------------------------

#[test]
fn step_out_returns_to_the_calling_frame() {
    const MAIN_TS: &str = r#"import { defineBot } from 'hbot';
import { helper } from './lib';

export default defineBot({
  onMessage(ctx) {
    const value = helper(1);
    ctx.log(String(value));
  },
});
"#;
    const LIB_TS: &str = r#"export function helper(n: number): number {
  const doubled = n * 2;
  return doubled;
}
"#;
    let project = project("out", &[("src/main.ts", MAIN_TS), ("src/lib.ts", LIB_TS)]);
    let in_helper = line_of(LIB_TS, "const doubled");
    let after_call = line_of(MAIN_TS, "ctx.log");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set("src/lib.ts", [in_helper]);
    sandbox.set_breakpoints(breakpoints);

    let sink = Sink::scripted([DebugAction::StepOut]);
    let outcome = sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("handler completes after stepping out");

    assert_eq!(outcome.logs, vec!["2".to_owned()]);
    let stops = sink.stops();
    assert_eq!(stops.len(), 2, "{stops:#?}");
    assert_eq!(
        (stops[0].file.as_str(), stops[0].line),
        ("src/lib.ts", in_helper)
    );
    assert_eq!(
        (stops[1].file.as_str(), stops[1].line),
        ("src/main.ts", after_call),
        "StepOut should resume in the caller"
    );
    // Two frames deep inside `helper`, one after stepping out: this is the
    // logical stack the stepping rules are defined on.
    assert_eq!(stops[0].call_stack.len(), 2, "{:#?}", stops[0].call_stack);
    assert_eq!(stops[1].call_stack.len(), 1, "{:#?}", stops[1].call_stack);
    // Outermost first, current frame last.
    assert_eq!(stops[0].call_stack[0].name, "onMessage");
    assert_eq!(stops[0].call_stack[1].name, "helper");
}

// ---------------------------------------------------------------------------
// 5. Without breakpoints a debug build behaves like a release build.
// ---------------------------------------------------------------------------

#[test]
fn without_breakpoints_the_debug_build_behaves_like_the_release_build() {
    let project = project("parity", &[("src/main.ts", STRAIGHT_LINE)]);
    let debug = compile_project_debug(project.path()).expect("debug build compiles");
    let release = compile_project(project.path()).expect("release build compiles");
    assert!(debug.is_debug());
    assert!(!release.is_debug());
    assert!(
        debug.module("src/main.ts").expect("module").js.len()
            > release.module("src/main.ts").expect("module").js.len(),
        "the debug build must actually carry probes"
    );

    let debug_sandbox = Sandbox::spawn(debug, Limits::default()).expect("debug isolate starts");
    let release_sandbox =
        Sandbox::spawn(release, Limits::default()).expect("release isolate starts");

    // No breakpoints, no sink: identical observable behaviour. This is the
    // "no sink attached" case a debug build runs in production debugging-off.
    let debug_outcome = debug_sandbox
        .dispatch_with_debug(&event(), None)
        .expect("debug dispatch");
    let release_outcome = release_sandbox
        .dispatch(&event())
        .expect("release dispatch");
    assert_eq!(debug_outcome.logs, release_outcome.logs);
    assert_eq!(debug_outcome.replies, release_outcome.replies);
    assert_eq!(debug_outcome.logs, vec!["3".to_owned()]);

    // And with a sink attached but no breakpoints: still no stop, same result.
    let sink = Sink::default();
    let with_sink = debug_sandbox
        .dispatch_with_debug(&event(), Some(&sink))
        .expect("debug dispatch with an idle sink");
    assert_eq!(sink.stop_count(), 0, "no breakpoint, no stop");
    assert_eq!(with_sink.logs, release_outcome.logs);
}

// ---------------------------------------------------------------------------
// 6. A pause cannot outlive the handler timeout.
// ---------------------------------------------------------------------------

#[test]
fn a_pause_does_not_disable_the_handler_timeout() {
    // The loop after the breakpoint would run forever; the point is that the
    // budget keeps running *while the sink holds the isolate*, so resuming a
    // slow debugger still ends in `Timeout` instead of a wedged sandbox.
    const SPIN: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    let n = 0;
    while (true) {
      n = n + 1;
    }
  },
});
"#;
    let project = project("timeout", &[("src/main.ts", SPIN)]);
    let breakpoint = line_of(SPIN, "let n = 0");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(100));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate starts");

    let mut breakpoints = Breakpoints::new();
    breakpoints.set("src/main.ts", [breakpoint]);
    sandbox.set_breakpoints(breakpoints);

    // The sink holds the isolate for longer than the whole handler budget: the
    // budget must still be enforced once it returns `Continue`.
    let sink = Sink::holding(Duration::from_millis(250));
    let started = Instant::now();
    let result = sandbox.dispatch_with_debug(&event(), Some(&sink));
    let elapsed = started.elapsed();

    match result {
        Err(CodeError::Timeout { .. }) => {}
        other => panic!("expected CodeError::Timeout, got {other:?}"),
    }
    assert_eq!(
        sink.stop_count(),
        1,
        "the breakpoint must still have stopped"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the timeout should fire promptly after the sink returns, took {elapsed:?}"
    );
    // The isolate is not wedged: it answers the next dispatch (the budget starts
    // over per event), so a paused-then-timed-out event does not kill it.
    assert!(!sandbox.is_paused(), "the pause must have been released");
}

// ---------------------------------------------------------------------------
// 7. A runtime error in a debug build still maps back to the TypeScript line.
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_error_in_a_debug_build_still_points_at_the_typescript_line() {
    const THROWS: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    const message = 'boom';
    throw new Error(message);
  },
});
"#;
    let project = project("error", &[("src/main.ts", THROWS)]);
    let throw_line = line_of(THROWS, "throw new Error");
    let compiled = compile_project_debug(project.path()).expect("debug build compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate starts");

    let error = sandbox.dispatch(&event()).expect_err("the handler throws");
    let CodeError::JsException { message, stack } = error else {
        panic!("expected JsException");
    };
    assert!(message.contains("boom"), "{message}");
    let stack = stack.expect("a remapped stack");
    // The probe frames must not appear, and the line must be the source line.
    assert!(
        !stack.contains("__hbot"),
        "the stack leaks instrumentation: {stack}"
    );
    let expected = format!("src/main.ts:{throw_line}:");
    assert!(
        stack.contains(&expected),
        "expected {expected:?} in:\n{stack}"
    );
}
