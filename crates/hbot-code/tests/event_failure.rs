//! A failed handler must not lose the `ctx.log()` lines it had already written.
//!
//! Root cause this file pins: `hbot-code` collected host output in `RawEvent`,
//! but `finish` returned only the `CodeError` on the error path, so every line
//! printed before a throw vanished. The host then had nothing to persist and the
//! user saw "my bot failed — and the three lines I logged are gone too".
//!
//! These are real isolates and real projects (same style as `s1_quickjs.rs`):
//! the bug lived in the collection/return path, which a mock would hide.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hbot_code::{CodeError, Limits, Sandbox, compile_project, compile_project_debug};
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

fn project(name: &str, main_ts: &str) -> Project {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "hbot-logcap-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src").join("main.ts");
    std::fs::create_dir_all(path.parent().expect("src directory")).expect("create fixture dir");
    std::fs::write(&path, main_ts).expect("write fixture file");
    Project(root)
}

/// Logs twice, then throws: the exact shape of the reported bug.
const LOG_THEN_THROW: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.log('before');
    ctx.log('second');
    throw new Error('boom');
  },
});
"#;

/// Logs once, then spins: the deadline interrupts it.
const LOG_THEN_SPIN: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.log('before');
    for (;;) {}
  },
});
"#;

fn code_of(error: &CodeError) -> &'static str {
    error.code()
}

// ---------------------------------------------------------------------------
// 1. The regression: logs survive a thrown exception.
// ---------------------------------------------------------------------------

#[test]
fn logs_written_before_a_throw_survive_the_failure() {
    let root = project("throw", LOG_THEN_THROW);
    let compiled = compile_project(root.path()).expect("project compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");

    let failure = sandbox
        .dispatch_collecting(&json!({}))
        .expect_err("the handler throws");

    assert_eq!(
        failure.logs,
        vec!["before".to_owned(), "second".to_owned()],
        "logs written before the throw must come back on the error path"
    );
    assert_eq!(
        code_of(&failure.error),
        "HBOT_JS_EXCEPTION",
        "a thrown Error stays a JS exception (not a timeout / memory limit)"
    );
    assert!(
        failure.error.to_string().contains("boom"),
        "{}",
        failure.error
    );
}

// ---------------------------------------------------------------------------
// 2. The old signature keeps its exact behaviour (no logs, same error).
// ---------------------------------------------------------------------------

#[test]
fn plain_dispatch_still_reports_the_same_error_without_logs() {
    let root = project("plain", LOG_THEN_THROW);
    let compiled = compile_project(root.path()).expect("project compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");

    let error = sandbox
        .dispatch(&json!({}))
        .expect_err("the handler throws");

    // `dispatch` is a thin `map_err` over the collecting form: existing callers
    // (`hbot-rpc`, `hbot-parity`, examples) must see byte-identical errors.
    assert_eq!(code_of(&error), "HBOT_JS_EXCEPTION");
    assert!(error.to_string().contains("boom"), "{error}");
}

// ---------------------------------------------------------------------------
// 3. The debug entry point collects on failure too.
// ---------------------------------------------------------------------------

#[test]
fn debug_dispatch_collects_logs_on_failure_too() {
    let root = project("debug", LOG_THEN_THROW);
    // Debug build so the call matches what the runtime uses (`compile_project_debug`).
    let compiled = compile_project_debug(root.path()).expect("project compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");

    let failure = sandbox
        .dispatch_with_debug_collecting(&json!({}), None)
        .expect_err("the handler throws");

    assert_eq!(
        failure.logs,
        vec!["before".to_owned(), "second".to_owned()],
        "the debugger's dispatch path must keep the logs as well"
    );
    assert_eq!(code_of(&failure.error), "HBOT_JS_EXCEPTION");
}

// ---------------------------------------------------------------------------
// 4. A timeout is also a failure: the lines before it still count.
// ---------------------------------------------------------------------------

#[test]
fn logs_written_before_a_timeout_survive_the_failure() {
    let root = project("timeout", LOG_THEN_SPIN);
    let compiled = compile_project(root.path()).expect("project compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(200));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate boots");

    let failure = sandbox
        .dispatch_collecting(&json!({}))
        .expect_err("a runaway handler must not return normally");

    assert_eq!(
        code_of(&failure.error),
        "HBOT_TIMEOUT",
        "the interrupt flag still classifies this as a timeout: {}",
        failure.error
    );
    assert_eq!(
        failure.logs,
        vec!["before".to_owned()],
        "logs printed before the deadline fired are still diagnostic evidence"
    );
}
