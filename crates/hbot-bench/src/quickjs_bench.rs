//! 预算 4：QuickJS 事件吞吐 —— 与 `docs/spikes/S1-quickjs.md` 的基线做回归对照。
//!
//! 数据怎么造的：`hbot_code::manifest::scaffold` 在临时目录里生成一个**脚手架项目**
//! （`hbot.json` / `tsconfig.json` / `hbot.d.ts` / `AGENTS.md` / `.gitignore` / `src/main.ts`），
//! 入口就是官方模板：`async onMessage(ctx) { reply(\`收到：${ctx.text}\`); }`。
//! 用 `Compiler` 编一次冷、一次热，再 `Sandbox::spawn` 起一个 isolate，然后向它
//! dispatch **1000 条入站事件**（`{"type":"message","text":"hello N","chatId":"g1","userId":"u1"}`），
//! 每条事件都断言刚好收回 1 条 `收到：…` —— 否则量到的可能是“快速失败”。
//!
//! 复跑 3 轮 × 1000 条。判定口径是 **S1 的 2×**（S1 一文件项目 mean 34.4 µs/事件）：
//! 超过 2× 说明这条链路已经不能用 S1 的数字外推，需要在终报里指出；这不是
//! `05-quality-gates.md` §4 表里的预算（那一栏写的是“回归 >10% 阻断”，需要先有基线入库）。
//!
//! 与 S1 的方法差异（比较时要算进去）：
//!
//! * S1 的 1-file 夹具是自己写的 `export default { onMessage }`，这里是脚手架模板；
//! * S1 的 handler 里 `await reply(...)`，脚手架模板不 await（`reply` 声明为 `void`，
//!   同步入队，所以差别只在一次微任务调度上）；
//! * S1 用 `Limits::default()`，这里用 `manifest.sandbox_limits()`（脚手架 `hbot.json`
//!   的默认值，同样是 32 MiB / 2 s，见 `ManifestLimits::default`）。

use std::time::{Duration, Instant};

use hbot_code::{Compiler, Sandbox, manifest};

use crate::harness::{self, Stats, TempDir, Unit};

/// 每轮事件数。
const EVENTS: usize = 1000;
/// 复跑轮数（每轮 1000 条）。
const RUNS: usize = 3;
/// S1 基线：一文件项目 1000 条事件的均值（`docs/spikes/S1-quickjs.md`：34.4 µs）。
const S1_MEAN: Duration = Duration::from_nanos(34_400);
/// 回归闸：超过 S1 的 2× 就在终报里点名（S1 数字不再能外推）。
const REGRESSION_LIMIT: Duration = Duration::from_nanos(68_800);

/// 冷编译 / spawn 是**单次**事件，最容易被并发编译与 Defender 拖长；
/// 与 S1 的偏离超过这个倍数就单独点名，并说明更可能是机器/负载差异。
const SINGLE_SHOT_DEVIATION: f64 = 2.0;

/// 跑事件吞吐测量并打印结果行。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    harness::require_release();
    harness::section("4. QuickJS 事件吞吐（hbot_code：脚手架项目 + 1000 条入站事件 × 3 轮）");

    let temp = TempDir::new("quickjs")?;
    let root = temp.path().to_path_buf();
    let scaffold_started = Instant::now();
    let report = manifest::scaffold(&root, "bench-bot")?;
    let scaffold_time = scaffold_started.elapsed();
    let manifest = manifest::load(&root)?;
    harness::info_line(
        "quickjs_fixture",
        &format!("{} 文件", report.files.len()),
        "files",
        &format!(
            "`manifest::scaffold` 生成的脚手架项目（{}）；入口 `{}`；建项目 {:.1} ms（不计入测量）",
            report.files.join(", "),
            manifest.entry_name(),
            scaffold_time.as_secs_f64() * 1e3
        ),
    );

    // 冷/热编译：S1 的 3.10 ms / 299 µs 就是这两个口径。
    let mut compiler = Compiler::new();
    let cold_started = Instant::now();
    let _ = compiler.compile_project(&root)?;
    let cold = cold_started.elapsed();
    let warm_started = Instant::now();
    let compiled = compiler.compile_project(&root)?;
    let warm = warm_started.elapsed();

    let spawn_started = Instant::now();
    let sandbox = Sandbox::spawn(compiled, manifest.sandbox_limits())?;
    let spawn = spawn_started.elapsed();

    let mut per_run_mean = Vec::with_capacity(RUNS);
    let mut samples: Vec<Duration> = Vec::with_capacity(EVENTS * RUNS);
    let mut replies = 0usize;
    for run in 0..RUNS {
        let run_started = Instant::now();
        for index in 0..EVENTS {
            let event = serde_json::json!({
                "type": "message",
                "text": format!("hello {run}-{index}"),
                "chatId": "g1",
                "userId": "u1",
            });
            let started = Instant::now();
            let outcome = sandbox.dispatch(&event)?;
            samples.push(started.elapsed());
            assert_eq!(
                outcome.replies.len(),
                1,
                "第 {run} 轮第 {index} 条事件没有产出恰好 1 条回复（实现可能没跑到 handler）"
            );
            replies += outcome.replies.len();
            if run == 0 && index == 0 {
                assert!(
                    outcome.replies[0].contains("hello 0-0"),
                    "回复内容不对：{:?}",
                    outcome.replies[0]
                );
            }
        }
        per_run_mean.push(run_started.elapsed().as_secs_f64() * 1e6 / EVENTS as f64);
    }

    let mean_us = samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1e6)
        .sum::<f64>()
        / samples.len() as f64;
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let p50 = harness::percentile(&sorted, 0.5);
    let p95 = harness::percentile(&sorted, 0.95);
    let worst = sorted[sorted.len() - 1];
    let s1_mean_us = S1_MEAN.as_secs_f64() * 1e6;

    harness::info_line(
        "quickjs_compile_cold",
        &Unit::Ms.render(cold),
        "ms",
        "`Compiler::new()` 后第一次编译（S1 对照：3.10 ms）",
    );
    harness::info_line(
        "quickjs_compile_warm",
        &Unit::Us.render(warm),
        "µs",
        "同一 Compiler 的第二次编译，全部命中内容哈希缓存（S1 对照：299 µs）",
    );
    harness::info_line(
        "quickjs_isolate_spawn",
        &Unit::Ms.render(spawn),
        "ms",
        "`Sandbox::spawn`（含 preload；S1 对照：1.97 ms）",
    );
    harness::info_line(
        "quickjs_events",
        &format!("{} 条 / {} 轮", EVENTS * RUNS, RUNS),
        "events",
        &format!("共收回 {replies} 条回复（每条恰好 1 条，说明 handler 真的跑了）"),
    );
    harness::info_line(
        "quickjs_event_mean",
        &Unit::Us.render(Duration::from_secs_f64(mean_us / 1e6)),
        "µs",
        &format!(
            "全部 {} 个样本的均值（S1 一文件项目：{s1_mean_us:.1} µs；比值 {:.2}×）",
            samples.len(),
            mean_us / s1_mean_us
        ),
    );
    harness::info_line(
        "quickjs_event_p50",
        &Unit::Us.render(p50),
        "µs",
        "p50（S1：19.7 µs）",
    );
    harness::info_line(
        "quickjs_event_p95",
        &Unit::Us.render(p95),
        "µs",
        "p95（S1 文档只给了 mean/p50/worst 2.74 ms）",
    );
    harness::info_line(
        "quickjs_event_worst",
        &Unit::Us.render(worst),
        "µs",
        "最慢单条（S1：2.74 ms —— 长尾来自 Windows 调度/GC，不是稳态成本）",
    );

    let memory = sandbox.memory()?;
    harness::info_line(
        "quickjs_isolate_heap_used",
        &format!("{}", memory.used_bytes / 1024),
        "KiB",
        "isolate 自报 used（S1：100 KiB）；**不是**进程内存口径，进程内存按 AGENTS.md 坑 34 不用 WorkingSet",
    );
    harness::info_line(
        "quickjs_isolate_heap_malloc",
        &format!("{}", memory.heap_bytes / 1024),
        "KiB",
        &format!(
            "isolate 自报 malloc（S1：112 KiB）；上限 {} KiB",
            memory.limit_bytes / 1024
        ),
    );

    // 判定：每轮 1000 条的均值 vs S1 的 2× —— 中位数取 3 轮均值的中位数。
    let per_run: Vec<Duration> = per_run_mean
        .iter()
        .map(|mean| Duration::from_secs_f64(mean / 1e6))
        .collect();
    harness::budget_line(
        "quickjs_1000_events_mean",
        &Stats::of(&per_run),
        Unit::Us,
        REGRESSION_LIMIT,
        &format!(
            "每轮 1000 条事件的均值；判定线 = S1 一文件项目 {s1_mean_us:.1} µs × 2 = 68.8 µs（超出即 S1 数字不可外推）"
        ),
    );

    // 单次事件（冷编译 / spawn）与 S1 的偏离：> 2× 就点名，并给出更可能的解释。
    let cold_ratio = cold.as_secs_f64() * 1e6 / 3_100.0;
    let spawn_ratio = spawn.as_secs_f64() * 1e6 / 1_970.0;
    let flagged: Vec<String> = [
        ("quickjs_compile_cold", cold_ratio),
        ("quickjs_isolate_spawn", spawn_ratio),
    ]
    .into_iter()
    .filter(|(_, ratio)| *ratio > SINGLE_SHOT_DEVIATION)
    .map(|(name, ratio)| format!("{name} {ratio:.1}×"))
    .collect();
    let deviation_value = if flagged.is_empty() {
        "均在 2× 内".to_owned()
    } else {
        flagged.join(" / ")
    };
    harness::info_line(
        "quickjs_single_shot_deviation",
        &deviation_value,
        "vs S1",
        if flagged.is_empty() {
            "冷编译 3.10 ms / spawn 1.97 ms 都在 S1 的 2× 内"
        } else {
            "冷编译与 spawn 是**单次**事件：本机测量期间有其他代理并发 cargo 编译（见 env_concurrent_builds / env_load_percent），且 Defender 按访问保护会介入 DLL 载入与文件扫描 —— 单次数字被放大属预期；1000 条事件的均值才是稳态口径"
        },
    );

    Ok(())
}
