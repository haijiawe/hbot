//! 建一个项目骨架，然后立刻按清单编一遍——`hbot new` 的核心两步。
//!
//! 用法：
//!
//! ```text
//! cargo run -p hbot-code --example scaffold_demo -- <dir> [name]
//! ```
//!
//! 打印骨架写了哪些文件、清单解析结果、编译统计。测试 `manifest::scaffold_writes_a_project_that_compiles`
//! 断言的是同一件事；这个例子把结果打在屏幕上，方便人工核对，也方便在 CI 日志里
//! 看到"骨架确实编得过"。

use std::path::PathBuf;

use hbot_code::manifest;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("用法：scaffold_demo <dir> [name]");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let name = args.next().unwrap_or_else(|| "demo".to_owned());

    let report = manifest::scaffold(&dir, &name)?;
    println!(
        "scaffolded {} ({} files)",
        report.dir.display(),
        report.files.len()
    );
    for file in &report.files {
        println!("  {file}");
    }

    let loaded = manifest::load(&dir)?;
    println!(
        "manifest: name={} entry={} platform={} ws={} handlerMs={} heapMb={} http={:?}",
        loaded.name,
        loaded.entry_name(),
        loaded.platform.kind,
        loaded.platform.ws.as_deref().unwrap_or("-"),
        loaded.limits.handler_ms,
        loaded.limits.heap_mb,
        loaded.permissions.http,
    );

    let limits = loaded.sandbox_limits();
    println!(
        "sandbox limits: handler={}ms heap={}MiB stack={}KiB",
        limits.handler_timeout_ms(),
        limits.memory_bytes / (1024 * 1024),
        limits.stack_bytes / 1024,
    );

    let project = hbot_code::compile_project_with_entry(&dir, &loaded.entry_name())?;
    let stats = project.stats();
    println!(
        "compiled: entry={} files={} source={}B js={}B misses={} in {:?}",
        project.entry(),
        stats.files,
        stats.source_bytes,
        stats.emitted_bytes,
        stats.cache_misses,
        stats.elapsed,
    );

    Ok(())
}
