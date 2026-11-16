//! 预算 5：内存（口径 = `PrivateMemorySize64` 私有提交，**不用** `WorkingSet64`）。
//!
//! 口径的来源是 `docs/adr/0002-agent-in-rust.md` §4：WorkingSet 里含可被系统回收的
//! 共享页 / standby 页（实测 WS 382 MB vs private 206 MB，差 176 MB），拿它判预算
//! 会把“文件缓存与共享 DLL”算成产品内存。所以这里按**私有提交**量。
//!
//! ## 判定的那一行
//!
//! `memory_per_isolate` = 每个 QuickJS isolate 的私有提交增量，预算 **< 8 MB**
//! （`docs/05-quality-gates.md` §4；ADR-0002 §4 实测 0.30 MB）。
//! 算法：预热（编译 1 次 + spawn/drop 1 个 isolate，把 rquickjs 的模块级懒初始化
//! 摘出去）→ 采基线 → 同时存活 8 个 isolate → 再采 → `(存活 − 基线) / 8` → 全 drop → 残差。
//!
//! ## 为什么“进程树求和”在这里 = 本进程
//!
//! `hbot-code` 的 isolate 是**线程**（`Sandbox::spawn` 把 rquickjs `!Send` 的 isolate
//! 放到自己的线程上），不是子进程 ⇒ 本进程没有子进程，进程树求和 = 本进程的私有提交。
//! 因此采样时只查**本进程 PID**。
//!
//! 注意**不要**在这里用 `scripts/mem-probe.ps1 -IncludeChildren`：它按“根 PID 的全体后代”
//! 求和，而采样用的 PowerShell 自己就是本进程的后代 —— 会把约 70 MB 的 PowerShell
//! 算进被测进程的树里，直接污染 8 MB 级别的判定。这是个真会踩的坑，写在这里备查。
//!
//! ## 没测的那几行（写进 S8 的「未测与原因」）
//!
//! * 桌面整机 < 260 MB：需要 Tauri 壳的 release 产物 + 真机窗口，本 crate 不启动 GUI；
//! * 核心空闲 < 60 MB / 每多一个 bot < 6 MB：需要 `hbot serve` 二进制与真实平台连接。
//!
//! 这三行在 ADR-0002 §4 与 S0-memory.md 里已有实测（206 MB / 3.4 MB / 0.10 MB），
//! 本 crate 不重测，也不冒充。

use std::time::Instant;

use hbot_code::{Compiler, Sandbox, manifest};

use crate::harness::{self, TempDir};

/// 同时存活的 isolate 数（8 个足够把单 isolate 增量从页粒度噪声里拉出来）。
const ISOLATES: usize = 8;
/// 每次采样点数（取中位数，避开单次采样撞上 GC/分配抖动）。
const SAMPLES: usize = 5;
/// 采样间隔。
const SAMPLE_INTERVAL_MS: u64 = 200;
/// 预算：每个 isolate < 8 MB。
const BUDGET_MB: f64 = 8.0;

/// 跑内存测量并打印结果行。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    harness::require_release();
    harness::section("5. 内存：PrivateMemorySize64（私有提交，ADR-0002 §4 口径）");

    let pid = std::process::id();
    harness::info_line(
        "memory_metric",
        "PrivateMemorySize64",
        "-",
        "ADR-0002 §4：不用 WorkingSet64（含可回收的共享/standby 页，WS 382 MB vs private 206 MB）；采样 `(Get-Process -Id <本进程>).PrivateMemorySize64`",
    );
    harness::info_line(
        "memory_process_tree",
        "1 进程 / 0 子进程",
        "-",
        "hbot-code 的 isolate 是**线程**（rquickjs !Send 放自己的线程），不是子进程 ⇒ 进程树求和 = 本进程；采样只查本进程 PID，避免把采样用的 PowerShell 自己算进后代合计",
    );

    let temp = TempDir::new("memory")?;
    let root = temp.path().to_path_buf();
    manifest::scaffold(&root, "bench-bot")?;
    let manifest = manifest::load(&root)?;
    let mut compiler = Compiler::new();
    let compiled = compiler.compile_project(&root)?;

    // 预热：先起一个再丢掉，把 rquickjs 的模块级懒初始化算到基线里。
    let warm = Sandbox::spawn(compiled.clone(), manifest.sandbox_limits())?;
    let warm_memory = warm.memory()?;
    drop(warm);

    let baseline = sample_median_mb(pid)?;
    harness::info_line(
        "memory_baseline",
        &format!("{baseline:.2}"),
        "MB",
        "预热后（编译 1 次 + spawn/drop 1 个 isolate）的进程私有提交 —— 单个 isolate 的增量以此为起点",
    );
    harness::info_line(
        "memory_isolate_heap",
        &format!("{}", warm_memory.used_bytes / 1024),
        "KiB",
        &format!(
            "isolate 自报 used / malloc {} KiB / 上限 {} KiB（这是堆口径，与下面的进程口径互补，不可混用）",
            warm_memory.heap_bytes / 1024,
            warm_memory.limit_bytes / 1024
        ),
    );

    let spawned = Instant::now();
    let sandboxes: Vec<Sandbox> = (0..ISOLATES)
        .map(|_| Sandbox::spawn(compiled.clone(), manifest.sandbox_limits()))
        .collect::<Result<_, _>>()?;
    let spawn_all = spawned.elapsed();
    let loaded = sample_median_mb(pid)?;

    let per_isolate_mb = (loaded - baseline) / ISOLATES as f64;
    let pass = harness::budget_line_scalar(
        "memory_per_isolate",
        per_isolate_mb,
        "MB",
        BUDGET_MB,
        &format!(
            "({ISOLATES} 个存活 {loaded:.2} MB − 基线 {baseline:.2} MB) / {ISOLATES}；预算 < {BUDGET_MB} MB/isolate（堆上限 32 MB 不是常驻内存）"
        ),
    );

    drop(sandboxes);
    let residual = sample_median_mb(pid)?;
    harness::info_line(
        "memory_after_drop",
        &format!("{residual:.2}"),
        "MB",
        &format!(
            "全部 drop 后：相对基线 {}（归还给分配器但未还系统属正常，Windows 进程私有提交通常不会立刻回落）",
            if residual >= baseline {
                format!("+{:.2} MB", residual - baseline)
            } else {
                format!("{:.2} MB", residual - baseline)
            }
        ),
    );
    harness::info_line(
        "memory_spawn_all",
        &format!("{:.1}", spawn_all.as_secs_f64() * 1e3),
        "ms",
        &format!("{ISOLATES} 个 isolate 的 spawn 总时间（不判定；单次事件，受负载影响）"),
    );

    if !pass {
        harness::info_line(
            "memory_verdict",
            "超预算",
            "-",
            "单个 isolate 的私有提交超过 8 MB：先看 heap limit（32 MiB）与实际 used 的差，再看是不是 isolate 之外的东西（线程栈 / preload 快照 / 编译产物）被算进来了",
        );
    }

    Ok(())
}

/// 采 `SAMPLES` 次本进程私有提交，取中位数（MB）。
///
/// 一次 PowerShell 进程里连续采完再统一解析：比起 5 次 PowerShell 少 4 次进程启动，
/// 更重要的是 5 个采样点在时间上均匀，不受每次启动延迟影响。
fn sample_median_mb(pid: u32) -> Result<f64, Box<dyn std::error::Error>> {
    let script = format!(
        "for ($i = 0; $i -lt {SAMPLES}; $i++) {{ \
           (Get-Process -Id {pid} -ErrorAction Stop).PrivateMemorySize64; \
           Start-Sleep -Milliseconds {SAMPLE_INTERVAL_MS} \
         }}"
    );
    let lines = harness::run_powershell(&script)?;
    let mut bytes: Vec<u64> = Vec::with_capacity(lines.len());
    for line in &lines {
        let value: u64 = line
            .trim()
            .parse()
            .map_err(|_| format!("PowerShell 采样输出不是整数：{line:?}"))?;
        bytes.push(value);
    }
    if bytes.is_empty() {
        return Err("PowerShell 没有返回任何内存采样".into());
    }
    bytes.sort_unstable();
    let median = bytes[bytes.len() / 2];
    Ok(median as f64 / 1_048_576.0)
}
