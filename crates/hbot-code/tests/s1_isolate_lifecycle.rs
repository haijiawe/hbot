//! S1 regressions: what an isolate must do when a project is *wrong*.
//!
//! The S1 blocker was not a slow load — it was a hang. `Sandbox::spawn` dropped
//! its own `Sandbox` on the error path, and `Drop` joined the worker
//! unconditionally while the worker sat in its "reject everything until
//! shutdown" loop, so every failed bootstrap froze the host with the CPU idle.
//! These tests pin both halves of the fix:
//!
//! * a failing entry module reports a real JavaScript message and returns;
//! * a worker that cannot be stopped is detached after a bounded wait instead
//!   of blocking `Drop` forever.
//!
//! They are also the regression test for the exact project shape that used to
//! hang (`export default defineBot({ ... })` referencing bare globals), kept
//! here as a permanent fixture instead of the throwaway `repro_bench.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hbot_code::{CodeError, Limits, Sandbox, compile_project};
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
        "hbot-s1-lifecycle-{name}-{}-{}",
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

/// The shape `docs/02-architecture.md` §6.1 documents: the host API comes from
/// the `hbot` module, and `ctx.text` is the incoming message text.
const DOCUMENTED_TS: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  onMessage: async (ctx) => {
    await reply(`echo: ${ctx.text}`);
  },
});
"#;

/// The exact project that used to hang `s1_probe`/`repro_bench`: `defineBot` and
/// `reply` are referenced as bare globals. Nothing installs those globals
/// (`host_api::install` only defines `globalThis.__hbot`), so the entry module
/// throws before any handler exists.
const BARE_GLOBALS_TS: &str = r#"export default defineBot({
  onMessage: async (ctx) => {
    await reply(ctx.chatId, ctx.text);
  },
});
"#;

#[test]
fn documented_project_shape_spawns_and_dispatches() {
    let root = project("documented", &[("src/main.ts", DOCUMENTED_TS)]);
    let compiled = compile_project(root.path()).expect("project compiles");

    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");
    let outcome = sandbox
        .dispatch(&json!({ "text": "hi" }))
        .expect("handler runs");

    // One reply, and it echoes the handler-visible `ctx.text`: the sandbox
    // really ran the handler, it did not just report success.
    assert_eq!(outcome.replies, vec!["echo: hi".to_owned()]);
    assert!(outcome.duration > Duration::ZERO);
}

#[test]
fn project_with_bare_globals_reports_a_js_error_instead_of_hanging() {
    let root = project("bare-globals", &[("src/main.ts", BARE_GLOBALS_TS)]);
    let compiled = compile_project(root.path()).expect("project compiles");

    let started = Instant::now();
    let error = match Sandbox::spawn(compiled, Limits::default()) {
        Ok(_) => panic!("a ReferenceError in the entry module cannot boot a sandbox"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();

    // The old bug: this call never returned. Anything near the 6 s backstop
    // would mean the deadlock is back.
    assert!(
        elapsed < Duration::from_secs(5),
        "spawn took {elapsed:?}; the error path is blocking again"
    );
    let CodeError::JsException { message, .. } = &error else {
        panic!("expected the JavaScript error, got {error:?}");
    };
    assert!(
        message.contains("defineBot"),
        "the JS error must name the missing global, got: {message}"
    );
}

#[test]
fn project_that_spins_while_loading_fails_fast() {
    // A module that never finishes evaluating. The interrupt handler is armed
    // for the whole bootstrap, so this is a timeout, not a hang.
    let root = project(
        "spin-at-load",
        &[(
            "src/main.ts",
            "while (true) {}\nexport default { onMessage() {} };\n",
        )],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(200));

    let started = Instant::now();
    let error = match Sandbox::spawn(compiled, limits) {
        Ok(_) => panic!("a spinning module cannot boot"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();

    assert_eq!(error.code(), "HBOT_TIMEOUT", "{error}");
    assert!(
        elapsed < Duration::from_secs(5),
        "spawn took {elapsed:?}; the error path is blocking again"
    );
}

#[test]
fn dropping_a_wedged_isolate_does_not_hang() {
    // The handler returns a promise that never settles, which parks the isolate
    // thread inside `LocalSet::block_on` with no JavaScript left to interrupt:
    // the worker cannot be stopped by any deadline. `Drop` must give up after
    // its bounded grace period and detach the thread.
    let root = project(
        "wedged",
        &[(
            "src/main.ts",
            "import { defineBot } from 'hbot';\n\
             \n\
             export default defineBot({\n\
             \x20 onMessage: async () => {\n\
             \x20   await new Promise(() => {});\n\
             \x20 },\n\
             });\n",
        )],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(100));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate boots");

    // The worker never answers, so this is the backstop (3×100 ms + 1 s).
    let dispatch_error = sandbox
        .dispatch(&json!({ "text": "hi" }))
        .expect_err("a handler that never settles must be reported");
    assert_eq!(dispatch_error.code(), "HBOT_SANDBOX_DEAD");

    let started = Instant::now();
    drop(sandbox);
    let elapsed = started.elapsed();

    // Grace period is handler_timeout + 1 s = 1.1 s. Allow slack for a loaded
    // CI machine, but stay far below "forever".
    assert!(
        elapsed < Duration::from_secs(3),
        "dropping a wedged isolate took {elapsed:?}; Drop is joining a stuck thread"
    );
}
