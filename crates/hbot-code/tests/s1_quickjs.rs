//! S1 acceptance tests: a multi-file TypeScript project, compiled into QuickJS,
//! calling the host's async API, under the sandbox limits.
//!
//! These are the tests `docs/spikes/S1-quickjs.md` quotes. They use real files in
//! the system temp directory and a real isolate thread — no mocks — because the
//! point of S1 is to find out whether the real thing works.

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
        "hbot-s1-{name}-{}-{}",
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

fn code_of(error: &CodeError) -> &'static str {
    error.code()
}

// ---------------------------------------------------------------------------
// 1. Multi-file relative imports + async host API.
// ---------------------------------------------------------------------------

const MAIN_TS: &str = r#"import { defineBot } from 'hbot';
import { decorate } from './lib';

export default defineBot({
  async onMessage(ctx) {
    const text = decorate(ctx.payload.text);
    // Awaiting a Rust-backed promise must work while the handler is suspended.
    await ctx.sleep(5);
    ctx.log('handled');
    ctx.reply(text);
  },
});
"#;

const LIB_TS: &str = r#"export function decorate(text: string): string {
  return `echo: ${text}!`;
}
"#;

#[test]
fn multi_file_project_calls_host_async_api() {
    let root = project(
        "multi-file",
        &[("src/main.ts", MAIN_TS), ("src/lib.ts", LIB_TS)],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    assert_eq!(compiled.stats().files, 2, "both modules compiled");

    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");
    let outcome = sandbox
        .dispatch(&json!({ "payload": { "text": "hi" } }))
        .expect("handler runs");

    assert_eq!(outcome.replies, vec!["echo: hi!".to_owned()]);
    assert_eq!(outcome.logs, vec!["handled".to_owned()]);
    // `sleep(5)` actually suspended: the handler cannot have finished sooner.
    assert!(
        outcome.duration >= Duration::from_millis(5),
        "sleep did not suspend, handler took {:?}",
        outcome.duration
    );
}

#[test]
fn repeated_events_reuse_the_same_isolate() {
    let root = project(
        "repeat",
        &[("src/main.ts", MAIN_TS), ("src/lib.ts", LIB_TS)],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");

    for round in 0..3 {
        let event = json!({ "payload": { "text": format!("r{round}") } });
        let outcome = sandbox.dispatch(&event).expect("handler runs");
        assert_eq!(outcome.replies, vec![format!("echo: r{round}!")]);
    }
}

// ---------------------------------------------------------------------------
// 2. Bare imports are rejected.
// ---------------------------------------------------------------------------

#[test]
fn bare_imports_are_rejected_at_compile_time() {
    let root = project(
        "bare",
        &[(
            "src/main.ts",
            "import _ from 'lodash';\nexport default defineBot;\n",
        )],
    );
    let error = compile_project(root.path()).expect_err("bare import must be rejected");
    assert_eq!(code_of(&error), "HBOT_BARE_IMPORT");
    let text = error.to_string();
    assert!(text.contains("lodash"), "{text}");
}

#[test]
fn importing_the_host_module_is_allowed() {
    let root = project(
        "host-module",
        &[(
            "src/main.ts",
            r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('ok');
  },
});
"#,
        )],
    );
    let compiled = compile_project(root.path()).expect("hbot is the allowed bare module");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");
    let outcome = sandbox.dispatch(&json!({})).expect("handler runs");
    assert_eq!(outcome.replies, vec!["ok".to_owned()]);
}

// ---------------------------------------------------------------------------
// 3. Deadlines interrupt a runaway handler, and the isolate survives.
// ---------------------------------------------------------------------------

const SPIN_TS: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    if (ctx.spin) {
      while (true) {
        // Pure JS loop: only the interrupt handler can stop this.
      }
    }
    ctx.reply('alive');
  },
});
"#;

#[test]
fn infinite_loop_is_interrupted() {
    let root = project("timeout", &[("src/main.ts", SPIN_TS)]);
    let compiled = compile_project(root.path()).expect("project compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(200));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate boots");

    let started = Instant::now();
    let error = sandbox
        .dispatch(&json!({ "spin": true }))
        .expect_err("a runaway handler must not return normally");
    let elapsed = started.elapsed();

    assert_eq!(code_of(&error), "HBOT_TIMEOUT");
    assert!(error.to_string().contains("200"), "{error}");
    // Comfortably below the 2 s default budget and below the dispatch backstop.
    assert!(
        elapsed < Duration::from_secs(2),
        "interrupt was not prompt: {elapsed:?}"
    );
}

#[test]
fn isolate_survives_an_interrupted_handler() {
    let root = project("recover", &[("src/main.ts", SPIN_TS)]);
    let compiled = compile_project(root.path()).expect("project compiles");
    let limits = Limits::default().with_handler_timeout(Duration::from_millis(200));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate boots");

    let _ = sandbox
        .dispatch(&json!({ "spin": true }))
        .expect_err("first event times out");

    // The interrupt throws an *uncatchable* exception inside the isolate; the
    // isolate itself must stay usable for the next event.
    let outcome = sandbox
        .dispatch(&json!({}))
        .expect("isolate still answers after a timeout");
    assert_eq!(outcome.replies, vec!["alive".to_owned()]);
}

// ---------------------------------------------------------------------------
// 4. The heap ceiling is enforced.
// ---------------------------------------------------------------------------

const HOG_TS: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  onMessage() {
    const chunks = [];
    for (;;) {
      chunks.push(new Array(1 << 16).fill(1));
    }
  },
});
"#;

#[test]
fn memory_limit_is_enforced() {
    let root = project("memory", &[("src/main.ts", HOG_TS)]);
    let compiled = compile_project(root.path()).expect("project compiles");
    // 8 MiB cap, and a deadline long enough that the allocator fails first.
    let limits = Limits::default()
        .with_memory_bytes(8 * 1024 * 1024)
        .with_handler_timeout(Duration::from_secs(10));
    let sandbox = Sandbox::spawn(compiled, limits).expect("isolate boots");

    let error = sandbox
        .dispatch(&json!({}))
        .expect_err("the handler must not be allowed to exhaust host memory");
    assert_eq!(code_of(&error), "HBOT_MEMORY_LIMIT", "{error}");
    assert!(error.to_string().contains("8388608"), "{error}");
}

// ---------------------------------------------------------------------------
// 5. A thrown error maps back to the TypeScript line.
// ---------------------------------------------------------------------------

#[test]
fn runtime_errors_map_back_to_the_typescript_line() {
    let root = project(
        "stack",
        &[
            (
                "src/main.ts",
                r#"import { defineBot } from 'hbot';
import { boom } from './lib';

export default defineBot({
  onMessage() {
    boom();
  },
});
"#,
            ),
            (
                "src/lib.ts",
                r#"export function boom() {
  throw new Error('kaboom');
}
"#,
            ),
        ],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");

    let error = sandbox.dispatch(&json!({})).expect_err("boom() throws");
    assert_eq!(code_of(&error), "HBOT_JS_EXCEPTION");
    assert!(error.to_string().contains("kaboom"), "{error}");

    let position = error.position().expect("a stack position was mapped");
    assert_eq!(position.file, "src/lib.ts");
    assert_eq!(position.line, 2, "throw is on line 2 of src/lib.ts");
}

// ---------------------------------------------------------------------------
// 6. Compile-time limits.
// ---------------------------------------------------------------------------

#[test]
fn missing_entry_is_reported() {
    let root = project("no-entry", &[("src/lib.ts", "export const x = 1;\n")]);
    let error = compile_project(root.path()).expect_err("src/main.ts is required");
    assert_eq!(code_of(&error), "HBOT_NO_ENTRY");
}

#[test]
fn escaping_imports_are_rejected() {
    let root = project(
        "escape",
        &[(
            "src/main.ts",
            "import { x } from '../../outside';\nexport default x;\n",
        )],
    );
    let error = compile_project(root.path()).expect_err("escaping the project must fail");
    assert_eq!(code_of(&error), "HBOT_IMPORT_ESCAPE", "{error}");
}

#[test]
fn missing_relative_module_is_reported() {
    let root = project(
        "missing-module",
        &[(
            "src/main.ts",
            "import { x } from './nope';\nexport default x;\n",
        )],
    );
    // Linking is static on purpose: a missing module is a *compile* error with
    // the offending `import` position, not a failure discovered half way through
    // the isolate boot. Measured in S1: a rejected load settles the import
    // promise with an exception (it does not stay pending), so catching it at
    // compile time is about the error message and the position, not about
    // avoiding a hang.
    let error = compile_project(root.path()).expect_err("./nope must not link");
    assert_eq!(code_of(&error), "HBOT_MODULE_NOT_FOUND", "{error}");
    let text = error.to_string();
    assert!(text.contains("nope"), "{text}");
    assert!(text.contains("src/main.ts:1:1"), "{text}");
}
