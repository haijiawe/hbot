//! Concrete S1 numbers: compile time, isolate startup, per-event latency,
//! isolate heap, and the process RSS floor.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p hbot-code --example s1_bench
//! ```
//!
//! The point is to replace "QuickJS should be small" with measured values, as
//! `docs/spikes/S1-quickjs.md` requires. Nothing here is a benchmark harness —
//! it is one script that prints numbers a human can paste into the spike doc.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hbot_code::{
    Compiler, DebugAction, DebugSink, Limits, PausedAt, Sandbox, compile_project,
    compile_project_debug,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join("hbot-code-s1-bench");
    let _ = std::fs::remove_dir_all(&root);

    println!("== S1 measurements ==");
    println!("process rss at start: {} KiB", rss_kib());

    for files in [1usize, 10, 50] {
        let project = write_project(&root, files)?;
        let mut compiler = Compiler::new();

        // Cold: nothing cached.
        let cold = Instant::now();
        let compiled = compiler.compile_project(&project)?;
        let cold_elapsed = cold.elapsed();
        let stats = compiled.stats();

        // Warm: every module served from the content-hash cache.
        let warm = Instant::now();
        let warm_compiled = compiler.compile_project(&project)?;
        let warm_elapsed = warm.elapsed();
        let warm_stats = warm_compiled.stats();

        println!(
            "compile {files:>2} files: cold {:>8.2?} (misses {}), warm {:>8.2?} (hits {})",
            cold_elapsed, stats.cache_misses, warm_elapsed, warm_stats.cache_hits
        );

        let spawn_start = Instant::now();
        let sandbox = Sandbox::spawn(warm_compiled, Limits::default())?;
        let spawn_elapsed = spawn_start.elapsed();
        println!("  isolate spawn (incl. preload): {spawn_elapsed:>8.2?}");

        // 1000 events through one isolate.
        let mut total = Duration::ZERO;
        let mut worst = Duration::ZERO;
        let mut replies = 0usize;
        for index in 0..1000 {
            let event = serde_json::json!({
                "type": "message",
                "text": format!("hello {index}"),
                "chatId": "g1",
                "userId": "u1",
            });
            let started = Instant::now();
            let outcome = sandbox.dispatch(&event)?;
            let elapsed = started.elapsed();
            total += elapsed;
            worst = worst.max(elapsed);
            replies += outcome.replies.len();
        }
        let mean = total / 1000;
        let p50ish = {
            // Cheap second pass only for a rough percentile; 1000 samples.
            let mut samples: Vec<Duration> = Vec::with_capacity(1000);
            for index in 0..1000 {
                let event = serde_json::json!({
                    "type": "message",
                    "text": format!("p {index}"),
                    "chatId": "g1",
                    "userId": "u1",
                });
                let started = Instant::now();
                let _ = sandbox.dispatch(&event)?;
                samples.push(started.elapsed());
            }
            samples.sort_unstable();
            samples[500]
        };
        let memory = sandbox.memory()?;
        println!(
            "  1000 events: mean {mean:>8.2?}, p50 {:>8.2?}, worst {worst:>8.2?}, replies {replies}",
            p50ish
        );
        println!(
            "  isolate heap: used {} KiB, malloc {} KiB, limit {} KiB",
            memory.used_bytes / 1024,
            memory.heap_bytes / 1024,
            memory.limit_bytes / 1024
        );
        println!("  process rss now: {} KiB", rss_kib());
        drop(sandbox);
    }

    // ---- instrumentation cost: debug build vs release build -------------
    //
    // `docs/02-architecture.md` §6.5 sets the target: the instrumented build may
    // cost at most 5× the release build per event. Three rows, because they
    // answer three different questions:
    //
    // * release — the baseline, no probes at all;
    // * debug, no sink — what a debug build costs when nobody is debugging
    //   (one global flag read per statement, the probes short-circuit in JS);
    // * debug, sink, no breakpoints — the worst case short of actually stopping:
    //   every statement calls into `__hbot_trace`, which reads its atomics and
    //   returns false.
    //
    // A breakpoint hit is not measured here: it blocks on a human, so it has no
    // meaningful per-event latency.
    println!("\n== instrumentation cost (debug build vs release build) ==");
    let mut worst_ratio = 0.0f64;
    for files in [1usize, 10] {
        let project_dir = write_project(&root, files)?;
        let release = compile_project(&project_dir)?;
        let debug = compile_project_debug(&project_dir)?;
        let released: usize = release.modules().values().map(|m| m.js.len()).sum();
        let instrumented: usize = debug.modules().values().map(|m| m.js.len()).sum();
        let probes = instrumented.saturating_sub(released);

        let release_mean = measure(&release, None, 1000)?;
        let debug_idle = measure(&debug, None, 1000)?;
        let debug_armed = measure(&debug, Some(&NoStop), 1000)?;
        let ratio = debug_armed.as_secs_f64() / release_mean.as_secs_f64().max(f64::EPSILON);
        worst_ratio = worst_ratio.max(ratio);

        println!(
            "  {files:>2} files: release {release_mean:>10.2?} | debug idle {debug_idle:>10.2?} \
             | debug armed {debug_armed:>10.2?} | ratio {ratio:>4.2}x",
        );
        println!(
            "    emitted js: {released} B -> {instrumented} B (+{} B of probes)",
            probes
        );
    }
    // Probe-dense handler: the event loop above is dominated by promise
    // scheduling, which is identical in both builds, so it barely exercises the
    // probes. A handler that spends its time in a computation loop is where the
    // per-statement cost is actually visible — 3 statements × 2000 iterations =
    // 6000 probes per event.
    //
    // Last measured (Windows 11, Rust 1.96, release example), two runs a minute
    // apart: 117.82µs → 546.68µs and 123.69µs → 573.36µs, both 4.64×, ~71 ns per
    // probe. That is the row the ≤5× target is really about; it passes with ~8%
    // headroom, so adding work to the probe fast path (another branch, another
    // atomic, a lock) is what would break it. The numbers are repeated in
    // `tests/s1_debug_instrument.rs` so a regression is visible next to the tests
    // as well.
    println!("\n== probe-dense handler (2000-iteration loop, 3 statements each) ==");
    let hot = write_hot_project(&root)?;
    let hot_release = compile_project(&hot)?;
    let hot_debug = compile_project_debug(&hot)?;
    let hot_release_mean = measure(&hot_release, None, 200)?;
    let hot_debug_mean = measure(&hot_debug, Some(&NoStop), 200)?;
    let hot_ratio = hot_debug_mean.as_secs_f64() / hot_release_mean.as_secs_f64().max(f64::EPSILON);
    worst_ratio = worst_ratio.max(hot_ratio);
    println!(
        "  release {hot_release_mean:>10.2?} | debug armed {hot_debug_mean:>10.2?} \
         | ratio {hot_ratio:>4.2}x"
    );

    println!(
        "  worst ratio {worst_ratio:.2}x (target <= 5x): {}",
        if worst_ratio <= 5.0 {
            "ok"
        } else {
            "REGRESSION"
        }
    );

    // Baseline for the external sampler: every isolate from the loop above is
    // dropped, so whatever RSS is left is the process floor.
    let baseline_hold = hold_millis("HBOT_S1_BASELINE_HOLD_MS", 4000);
    if baseline_hold > 0 {
        println!("0 isolates alive; holding {baseline_hold} ms for baseline RSS sampling");
        hold(baseline_hold);
    }

    // Many isolates in one process: the per-bot cost that decides "8 bots fit".
    let project = write_project(&root, 1)?;
    let compiled = compile_project(&project)?;
    let mut sandboxes = Vec::new();
    let spawn_all = Instant::now();
    for _ in 0..8 {
        sandboxes.push(Sandbox::spawn(compiled.clone(), Limits::default())?);
    }
    println!(
        "spawn 8 isolates: {:>8.2?} total, process rss {} KiB (0 = read it with scripts/mem-probe.ps1)",
        spawn_all.elapsed(),
        rss_kib()
    );

    // Keep all eight realms alive long enough for an external sampler to catch
    // the peak: a frozen process is easy to miss from outside, and the number
    // that matters ("does one bot cost one QuickJS heap?") is the peak, not the
    // after-drop floor.
    let hold_for = hold_millis("HBOT_S1_HOLD_MS", 6000);
    if hold_for > 0 {
        println!("8 isolates alive; holding {hold_for} ms for external RSS sampling");
        hold(hold_for);
    }
    drop(sandboxes);

    println!("process rss after drop: {} KiB", rss_kib());
    Ok(())
}

/// Read a millisecond budget from the environment, falling back to `default`.
fn hold_millis(variable: &str, default: u64) -> u64 {
    std::env::var(variable)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Sleep `total` ms in slices, printing progress so a stalled hold is visible.
fn hold(total: u64) {
    let mut waited = 0u64;
    while waited < total {
        let slice = 500u64.min(total - waited);
        std::thread::sleep(Duration::from_millis(slice));
        waited += slice;
        println!("  ... {waited} ms, process rss {} KiB", rss_kib());
    }
}

fn write_project(root: &Path, files: usize) -> Result<PathBuf, std::io::Error> {
    let dir = root.join(format!("p{files}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;

    // `files - 1` library modules, each imported by the entry, so the link pass
    // and the module loader both do real work.
    let mut entry = String::new();
    for index in 0..files.saturating_sub(1) {
        let name = format!("lib{index}");
        let body = format!(
            "export function f{index}(value: string): string {{\n\
             \x20 const padded: string = `[{name}] ${{value}}`;\n\
             \x20 return padded;\n\
             }}\n"
        );
        std::fs::write(dir.join("src").join(format!("{name}.ts")), body)?;
        entry.push_str(&format!("import {{ f{index} }} from './{name}';\n"));
    }
    entry.push_str("import { defineBot, reply } from 'hbot';\n\n");
    entry.push_str(
        "export default defineBot({\n\
         \x20 onMessage: async (ctx) => {\n",
    );
    for index in 0..files.saturating_sub(1) {
        entry.push_str(&format!(
            "    const v{index}: string = f{index}(ctx.text);\n"
        ));
    }
    entry.push_str("    await reply(ctx.text);\n");
    entry.push_str("  },\n});\n");
    std::fs::write(dir.join("src").join("main.ts"), entry)?;
    Ok(dir)
}

/// A sink that never asks the isolate to stop.
///
/// Attaching it is the "instrumented, armed, but no breakpoint on this line"
/// case: every probe calls into `__hbot_trace`, which reads its atomics and says
/// no. That is the expensive end of not-actually-debugging, so it is the number
/// to compare against the 5× target.
struct NoStop;

impl DebugSink for NoStop {
    fn paused(&self, _at: PausedAt) -> DebugAction {
        DebugAction::Continue
    }
}

/// Mean dispatch time over 1000 events on a fresh isolate.
///
/// A fresh isolate per row keeps the comparison honest: no warm-up ordering and
/// no shared realm state between the release and debug runs.
fn measure(
    compiled: &hbot_code::CompiledProject,
    sink: Option<&dyn DebugSink>,
    events: u32,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let sandbox = Sandbox::spawn(compiled.clone(), Limits::default())?;
    let mut total = Duration::ZERO;
    for index in 0..events {
        let event = serde_json::json!({
            "type": "message",
            "text": format!("bench {index}"),
        });
        let started = Instant::now();
        match sink {
            Some(sink) => {
                sandbox.dispatch_with_debug(&event, Some(sink))?;
            }
            None => {
                sandbox.dispatch(&event)?;
            }
        }
        total += started.elapsed();
    }
    Ok(total / events.max(1))
}

/// A project whose handler is dominated by statements rather than by awaiting.
///
/// Used for the instrumentation-cost measurement: 2000 iterations × 3 statements
/// means 6000 probes per event, so the probe overhead is not hidden behind the
/// promise machinery both builds share.
fn write_hot_project(root: &Path) -> Result<PathBuf, std::io::Error> {
    let dir = root.join("hot");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    let main = "import { defineBot, reply } from 'hbot';\n\
                \n\
                export default defineBot({\n\
                \x20 onMessage: async (ctx) => {\n\
                \x20   let total = 0;\n\
                \x20   for (let index = 0; index < 2000; index = index + 1) {\n\
                \x20     const step = index % 7;\n\
                \x20     total = total + step;\n\
                \x20   }\n\
                \x20   await reply(String(total));\n\
                \x20 },\n\
                });\n";
    std::fs::write(dir.join("src").join("main.ts"), main)?;
    Ok(dir)
}

/// Resident set size of this process, in KiB. Windows via `tasklist` would add a
/// process spawn per sample, so read the working set through the OS directly.
fn rss_kib() -> u64 {
    #[cfg(windows)]
    {
        // `GetProcessMemoryInfo` needs a Windows API dependency; the harness that
        // runs this example (`scripts/mem-probe.ps1`) reports the same number
        // from outside. Report 0 here rather than adding a dependency to a
        // production crate.
        0
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0)
    }
}
