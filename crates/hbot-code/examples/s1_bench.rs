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

use hbot_code::{Compiler, Limits, Sandbox, compile_project};

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

    // Many isolates in one process: the per-bot cost that decides "8 bots fit".
    let project = write_project(&root, 1)?;
    let compiled = compile_project(&project)?;
    let mut sandboxes = Vec::new();
    let spawn_all = Instant::now();
    for _ in 0..8 {
        sandboxes.push(Sandbox::spawn(compiled.clone(), Limits::default())?);
    }
    println!(
        "spawn 8 isolates: {:>8.2?} total, process rss {} KiB",
        spawn_all.elapsed(),
        rss_kib()
    );
    drop(sandboxes);

    println!("process rss after drop: {} KiB", rss_kib());
    Ok(())
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
    entry.push_str(
        "export default defineBot({\n\
         \x20 onMessage: async (ctx) => {\n",
    );
    for index in 0..files.saturating_sub(1) {
        entry.push_str(&format!(
            "    const v{index}: string = f{index}(ctx.text);\n"
        ));
    }
    entry.push_str("    await reply(ctx.chatId, ctx.text);\n");
    entry.push_str("  },\n});\n");
    std::fs::write(dir.join("src").join("main.ts"), entry)?;
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
