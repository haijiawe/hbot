//! 搜索耗时**离散**的机制实验（不是预算闸门，不参与 CI）。
//!
//! ```text
//! cargo run -q --release -p hbot-bench --example search_variance *> variance.log
//! ```
//!
//! 要回答的问题只有一个：`search_10k_*` 那条行里偶发的 **> 1000 ms**，到底是
//! **环境/缓存波动**，还是**算法本身**（批间负载不均、末尾小批、自动线程数、排序/截断、
//! 句柄争用、Defender）。做法是把算法侧能被解释的因素逐项用**对照实验**打掉：
//!
//! | 实验 | 打掉哪个假设 | 怎么读 |
//! |---|---|---|
//! | §0.5 首次触碰搜索 | 基准行本身是否已接近 IO 地板 | 第 1 次 vs 后续热运行的差 = 首次读盘/扫描成本 |
//! | §1 裸遍历三种口径（DirEntry 元数据 / 按路径 stat / 加读内容） | 「bench 的 walk_metadata 是不是 search 真付的账」 | 两条元数据路径差一个数量级时，归因段不能互相替代 |
//! | §1 内存内匹配 | 匹配算法是否值得优化 | 占比通常 < 5% |
//! | §2 线程数扫描 1/2/4/8/14/20/32/64/auto | 「自动线程数取错」「句柄争用」 | 若 20 与 8/14 无差异，多开线程只是摊薄同一份 IO；若 64 变慢，是争用 |
//! | §3 同进程连跑 24 次 | 「同配置下离散来自算法」 | 同进程 p95 远小于跨进程离群 ⇒ 离群来自进程外 |
//! | §4 `max_file_size = 1` 只跑阶段 1 | 「阶段 1（遍历+排序）吃大头」 | 阶段 1 单独耗时占比 |
//! | §4 10k 路径排序 | 「排序/截断开销」 | µs 级就不可能解释 1 s |
//! | §5 每批 512 的字节负载 | 「批间负载不均 / 末尾小批」 | 批字节 max/min 与每 worker 切片 max/mean |
//! | §6 CPU 争用 A/B | 「外部编译占 CPU」 | 同一份代码、只加 CPU 占用，看倍数 |
//! | §7 同盘 IO 争用 A/B | 「别的进程在读同一块盘」 | 同一份代码、只加并发读，看倍数 |
//!
//! 纪律（与预算行一致，别为了好看改口径）：
//!
//! * 只跑 release（[`harness::require_release`]）；
//! * 夹具复用 [`hbot_bench::search_bench`] 的同一棵树（同一份 `build_fixture`），
//!   不另造第二份夹具；
//! * 冷/热口径跟预算行一致：夹具由**本进程**写出，`cold_first_call` 是**偏乐观的下界**
//!   （文件已在 Windows 文件缓存里），不是真冷盘 —— 本实验里 `§3` 的第 1 次同样标注为
//!   「冷(乐观)」，第 2 次起才是热；
//! * 争用实验的「during」是**故意制造**的对照，不是环境噪声的证据 —— 反过来用它给出
//!   「要多少外部争用才能把 250 ms 推到 1 s」这个量级刻度。

use std::error::Error;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hbot_bench::harness::{self, TempDir};
use hbot_bench::search_bench::{ABSENT, NEEDLE, build_fixture_for_variance};
use hbot_workspace::{SearchOptions, Workspace};

/// 预算线：与预算行同一条，用于统计「超线次数」，不参与判定。
const BUDGET: Duration = Duration::from_millis(1000);
/// 搜索实现的批大小，必须与 `hbot-workspace/src/search.rs` 的 `SEARCH_BATCH_FILES` 一致。
const BATCH_FILES: usize = 512;
/// 搜索实现的线程上限，必须与 `search.rs` 的 `MAX_SEARCH_THREADS` 一致。
const MAX_THREADS: usize = 64;
/// 线程扫描用的配置（0 = 自动）。
const THREAD_CONFIGS: [usize; 9] = [1, 2, 4, 8, 14, 20, 32, 64, 0];
/// 线程扫描的轮数（每配置 n = 本值）。
const SWEEP_ROUNDS: usize = 5;
/// 同进程连跑次数。
const CONSECUTIVE_RUNS: usize = 24;

fn main() -> Result<(), Box<dyn Error>> {
    harness::require_release();
    harness::print_header_once();

    let temp = TempDir::new("searchvar")?;
    let root = temp.path().to_path_buf();

    harness::info_line(
        "variance_repro",
        "cargo run -q --release -p hbot-bench --example search_variance",
        "cmd",
        "复现命令（先设 CARGO_TARGET_DIR，见 docs/spikes/S8-perf-budgets.md）",
    );

    // ---- §0 夹具 ---------------------------------------------------------
    harness::section("0. 夹具（与预算行同一份 build_fixture）");
    let started = Instant::now();
    let facts = build_fixture_for_variance(&root)?;
    let build_time = started.elapsed();
    harness::info_line(
        "variance_fixture",
        &format!(
            "{} 文件 / {} 目录 / {} B 正文 ({:.1} MiB) / 命中文件 {}",
            facts.files,
            facts.dirs,
            facts.total_bytes,
            facts.total_bytes as f64 / (1024.0 * 1024.0),
            facts.hit_files
        ),
        "files",
        "与 search_bench::run 的夹具逐字节相同",
    );
    harness::info_line(
        "variance_fixture_build",
        &format!("{:.1}", as_ms(build_time)),
        "ms",
        "建树墙钟（不计入任何预算）",
    );
    snapshot("start");

    let workspace = Workspace::open(root.clone())?;
    let options = SearchOptions::default();

    // ---- §0.5 首次触碰（A 树） -------------------------------------------
    harness::section("0.5 首次触碰：A 树刚建完、内容一次都没被读过");
    let first_touch = measure(&workspace, NEEDLE, &options, 0)?;
    harness::info_line(
        "variance_first_touch",
        &format!("{:.1}", as_ms(first_touch.elapsed)),
        "ms",
        "A 树第 1 次搜索：文件内容首次被读（含 Defender 按访问扫描/首次缺页）",
    );

    // ---- §1 参照基线：裸遍历（B 树专保冷）+ 内存内匹配 -------------------
    harness::section("1. 参照基线：裸 std 遍历（B 树：内容从没被读过）/ 内存内匹配");
    let pristine = TempDir::new("searchvar-pristine")?;
    let pristine_root = pristine.path().to_path_buf();
    let pristine_started = Instant::now();
    let pristine_facts = build_fixture_for_variance(&pristine_root)?;
    harness::info_line(
        "variance_fixture_pristine",
        &format!(
            "{} 文件 / {} B / 建树 {:.1} ms",
            pristine_facts.files,
            pristine_facts.total_bytes,
            as_ms(pristine_started.elapsed())
        ),
        "files",
        "第二棵树：只用来量「首次触碰」的裸 IO，避免被 A 树的搜索预热污染",
    );
    // ① 只走目录项 + DirEntry 自带元数据（search.rs 阶段 1 的做法）
    let entry_first = walk_entry_metadata(&pristine_root)?;
    harness::info_line(
        "variance_walk_entry_first",
        &format!("{:.1}", as_ms(entry_first)),
        "ms",
        "B 树首次：read_dir + DirEntry::metadata（不读内容、不额外 stat）",
    );
    // ② 每文件一次按路径 stat（bench 的 measure_walk 的做法）
    let stat_first = walk_path_stat(&pristine_root)?;
    harness::info_line(
        "variance_walk_stat_first",
        &format!("{:.1}", as_ms(stat_first)),
        "ms",
        "B 树首次：read_dir + fs::metadata(path)（按路径 stat，要开句柄）",
    );
    // ③ 内容第一次被读
    let read_first = walk(&pristine_root, true)?;
    harness::info_line(
        "variance_walk_read_first",
        &format!("{:.1}", as_ms(read_first)),
        "ms",
        "B 树首次：read_dir + DirEntry::metadata + fs::read（内容首次读）",
    );
    let walk_read = repeat(3, || walk(&pristine_root, true))?;
    print_dist(
        "variance_walk_read_hot",
        &walk_read,
        "B 树热：同上一段，第 2–4 次",
    );
    let walk_meta = repeat(3, || walk_entry_metadata(&pristine_root))?;
    print_dist(
        "variance_walk_entry_hot",
        &walk_meta,
        "B 树热：只读目录项元数据",
    );
    let bodies = read_all(&pristine_root)?;
    let inmem = repeat(3, || {
        let started = Instant::now();
        let hits = bodies
            .iter()
            .filter(|body| body.windows(NEEDLE.len()).any(|w| w == NEEDLE.as_bytes()))
            .count();
        assert_eq!(hits, pristine_facts.hit_files, "内存内命中数与夹具不符");
        Ok(started.elapsed())
    })?;
    print_dist(
        "variance_inmem_match",
        &inmem,
        "已读入内存后子串扫描全树（21 MB）",
    );
    let inmem_all = repeat(3, || {
        let started = Instant::now();
        let utf8 = bodies
            .iter()
            .filter(|b| std::str::from_utf8(b).is_ok())
            .count();
        assert_eq!(utf8, pristine_facts.files, "内存内 UTF-8 校验数不对");
        Ok(started.elapsed())
    })?;
    print_dist(
        "variance_inmem_utf8",
        &inmem_all,
        "已读入内存后 from_utf8 全树",
    );

    // ---- §2 线程数扫描 ---------------------------------------------------
    harness::section("2. 线程数扫描（热缓存，交错取样，防漂移）");
    // 先各跑一次预热，丢掉「进程内第一次」的冷效应。
    let _ = measure(&workspace, NEEDLE, &options, 0)?;
    let _ = measure(&workspace, NEEDLE, &options, 0)?;
    let mut buckets: Vec<Vec<Duration>> = vec![Vec::new(); THREAD_CONFIGS.len()];
    let mut hit_sets: Vec<usize> = Vec::new();
    for _ in 0..SWEEP_ROUNDS {
        for (slot, &threads) in THREAD_CONFIGS.iter().enumerate() {
            let run = measure(&workspace, NEEDLE, &options, threads)?;
            buckets[slot].push(run.elapsed);
            hit_sets.push(run.hits);
        }
    }
    for (slot, &threads) in THREAD_CONFIGS.iter().enumerate() {
        let label = if threads == 0 {
            "auto".to_string()
        } else {
            threads.to_string()
        };
        print_dist(
            &format!("variance_threads_{label}"),
            &buckets[slot],
            &format!("search_with_threads(threads={label})，命中 100"),
        );
    }
    let first_hits = hit_sets.first().copied().unwrap_or(0);
    let all_same = hit_sets.iter().all(|&h| h == first_hits);
    harness::info_line(
        "variance_threads_hits",
        &format!("{first_hits} 条 / 各配置一致={all_same}"),
        "hits",
        "任何线程数都必须找到同一批命中（确定性）",
    );
    assert!(
        all_same,
        "不同线程数的命中数不一致 —— 这是正确性 bug，不是性能问题"
    );

    // ---- §3 同进程连跑 ---------------------------------------------------
    harness::section("3. 同进程连跑（threads=auto，热缓存）");
    let mut consecutive: Vec<Duration> = Vec::with_capacity(CONSECUTIVE_RUNS);
    let mut first_hits: Vec<Duration> = Vec::with_capacity(CONSECUTIVE_RUNS);
    let mut scanned: Vec<u64> = Vec::with_capacity(CONSECUTIVE_RUNS);
    for index in 0..CONSECUTIVE_RUNS {
        let run = measure(&workspace, NEEDLE, &options, 0)?;
        assert_eq!(run.hits, facts.hit_files, "命中数必须等于夹具埋点数");
        assert!(
            !run.limit_reached,
            "max_results=1000 且只有 100 命中，不该截断"
        );
        if index == 0 {
            harness::info_line(
                "variance_consecutive_first",
                &format!("{:.1}", as_ms(run.elapsed)),
                "ms",
                "本进程第 1 次搜索（缓存已被建树捂热，标注为冷(乐观)下界）",
            );
        }
        if let Some(first) = run.first_hit {
            first_hits.push(first);
        }
        scanned.push(run.files_scanned);
        consecutive.push(run.elapsed);
    }
    print_dist("variance_consecutive", &consecutive, "连续 24 次，热缓存");
    print_dist(
        "variance_first_hit",
        &first_hits,
        "首条命中出现的时刻（首屏指标；命中在第 8 个文件，但批次 512 会先扫完）",
    );
    harness::info_line(
        "variance_files_scanned",
        &format!(
            "min {} / max {}",
            scanned.iter().min().copied().unwrap_or(0),
            scanned.iter().max().copied().unwrap_or(0)
        ),
        "files",
        "每次实际扫描的文件数（应恒为 10000 或直到 512 批边界）",
    );
    harness::info_line(
        "variance_consecutive_series",
        &consecutive
            .iter()
            .map(|d| format!("{:.0}", as_ms(*d)))
            .collect::<Vec<_>>()
            .join(" "),
        "ms",
        "逐次序列（找成簇的超线，而不是均匀分布）",
    );
    let over: Vec<usize> = consecutive
        .iter()
        .enumerate()
        .filter(|(_, d)| **d > BUDGET)
        .map(|(i, _)| i)
        .collect();
    harness::info_line(
        "variance_consecutive_over",
        &format!("{} / {CONSECUTIVE_RUNS}（位次 {:?}）", over.len(), over),
        "runs",
        "同进程内超线次数与位次",
    );
    let no_hit = repeat(5, || Ok(measure(&workspace, ABSENT, &options, 0)?.elapsed))?;
    print_dist("variance_no_hit", &no_hit, "查不存在的词（必然扫完 10k）");

    // ---- §4 阶段拆分与排序开销 -------------------------------------------
    harness::section("4. 阶段拆分：只跑阶段 1 / 排序开销");
    let tiny = SearchOptions {
        max_file_size: 1,
        ..SearchOptions::default()
    };
    let phase1 = repeat(5, || Ok(measure(&workspace, NEEDLE, &tiny, 0)?.elapsed))?;
    print_dist(
        "variance_phase1_only",
        &phase1,
        "max_file_size=1 ⇒ 候选全被预筛掉，只剩阶段 1（遍历+排序空表）",
    );
    let serial_one = repeat(5, || Ok(measure(&workspace, NEEDLE, &options, 1)?.elapsed))?;
    print_dist(
        "variance_threads_one",
        &serial_one,
        "1 线程全流程（同口径对照）",
    );
    // 排序开销：用真实的 10k 相对路径（与 search.rs 的候选 label 同源数据）量排序本身。
    let paths = all_relative_paths(&root)?;
    let sorting = repeat(5, || {
        let mut copy = paths.clone();
        let started = Instant::now();
        copy.sort_unstable();
        Ok(started.elapsed())
    })?;
    print_dist(
        "variance_sort_10k_paths",
        &sorting,
        "10k 相对路径 sort_unstable（含 Vec<String> 克隆）",
    );
    let hits_sort = repeat(5, || {
        let mut copy: Vec<(String, u64)> = (0..100u64)
            .map(|i| {
                (
                    format!(
                        "pkg{:02}/mod{:02}/file_{:03}.txt",
                        i / 1000,
                        (i / 100) % 10,
                        i % 100
                    ),
                    i,
                )
            })
            .collect();
        let started = Instant::now();
        copy.sort_unstable();
        Ok(started.elapsed())
    })?;
    print_dist(
        "variance_sort_100hits",
        &hits_sort,
        "100 条命中按 (path,line) 排序",
    );

    // ---- §5 批间负载 -----------------------------------------------------
    harness::section("5. 批间负载（SEARCH_BATCH_FILES=512）");
    batch_balance(&root, &paths)?;
    snapshot("after-quiet");

    // ---- §6 CPU 争用 A/B -------------------------------------------------
    //
    // 8 个忙等线程在 20 逻辑核上只占 40%，几乎测不出退化；batch G 当时是
    // `concurrent_builds=12` 的真编译，所以这里补 20（1:1 超订）与 40（2:1 超订）两档。
    harness::section("6. 对照：故意制造 CPU 争用（同一份代码，只加忙等线程）");
    for level in [8usize, 20, 40] {
        let before = repeat(3, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_cpu{level}_before"),
            &before,
            "加负载前基线",
        );
        let hogs = Hogs::cpu(level);
        std::thread::sleep(Duration::from_millis(500));
        snapshot(&format!("during-cpu{level}"));
        let during = repeat(5, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_cpu{level}_during"),
            &during,
            &format!("{level} 个忙等线程（逻辑核 20）"),
        );
        hogs.stop();
        std::thread::sleep(Duration::from_millis(400));
        let after = repeat(2, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_cpu{level}_after"),
            &after,
            "撤掉争用后（恢复对照）",
        );
        print_ratio(&format!("variance_cpu{level}_ratio"), &before, &during);
    }

    // ---- §6b 超订下的线程数扫描 ------------------------------------------
    //
    // 官方基准（`search()` 自动取 20 线程）在 20 个忙等线程下常常比**1 线程还慢**，
    // 而同一窗口里单线程裸遍历几乎不受影响。这里把「线程数」当自变量扫一遍：
    // 若每条都比 §2 的安静值差得多、且 1 线程最好，那就不是「某个线程数不巧」，
    // 而是「并行阶段在超订下净亏」，需要看是不是每批 spawn+join 的栅栏在付调度账。
    harness::section("6b. 对照：1:1 超订下的线程数扫描（并行是否净亏）");
    {
        let hogs = Hogs::cpu(20);
        std::thread::sleep(Duration::from_millis(500));
        snapshot("during-cpu20-sweep");
        for k in [1usize, 2, 4, 8, 14, 20, 32, 64, 0] {
            let samples = repeat(2, || Ok(measure(&workspace, NEEDLE, &options, k)?.elapsed))?;
            let label = if k == 0 {
                "auto".to_string()
            } else {
                k.to_string()
            };
            print_dist(
                &format!("variance_overload_threads_{label}"),
                &samples,
                "20 个忙等线程同时在跑（1:1 超订；安静值见 §2 同名档位）",
            );
        }
        hogs.stop();
        std::thread::sleep(Duration::from_millis(300));
        snapshot("after-cpu20-sweep");
    }

    // ---- §7 IO 争用 A/B --------------------------------------------------
    harness::section("7. 对照：故意制造同盘 IO 争用（N 个线程循环读同一棵树）");
    for level in [3usize, 8] {
        let before = repeat(3, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_io{level}_before"),
            &before,
            "加负载前基线",
        );
        let hogs = Hogs::io(root.clone(), level);
        std::thread::sleep(Duration::from_millis(500));
        snapshot(&format!("during-io{level}"));
        let during = repeat(5, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_io{level}_during"),
            &during,
            &format!("{level} 个线程循环 read_dir+read 全树（与 search 抢同一份文件）"),
        );
        hogs.stop();
        std::thread::sleep(Duration::from_millis(400));
        let after = repeat(2, || Ok(measure(&workspace, NEEDLE, &options, 0)?.elapsed))?;
        print_dist(
            &format!("variance_io{level}_after"),
            &after,
            "撤掉争用后（恢复对照）",
        );
        print_ratio(&format!("variance_io{level}_ratio"), &before, &during);
    }

    // ---- §8 派生量 -------------------------------------------------------
    harness::section("8. 派生量（把三段时间从同一批数字里扣出来）");
    let phase1_med = median_ms(&phase1);
    let one_med = median_ms(&serial_one);
    let auto_med = median_ms(&buckets[THREAD_CONFIGS.len() - 1]);
    let walk_read_med = median_ms(&walk_read);
    harness::info_line(
        "variance_phase1_share",
        &format!(
            "{:.1} / {:.1} = {:.0}%",
            phase1_med,
            one_med,
            100.0 * phase1_med / one_med
        ),
        "ms",
        "阶段 1 占 1 线程全流程的比例",
    );
    harness::info_line(
        "variance_phase2_serial",
        &format!("{:.1}", (one_med - phase1_med).max(0.0)),
        "ms",
        "1 线程下阶段 2（读内容+匹配）的估值 = 1 线程全流程 − 阶段 1",
    );
    harness::info_line(
        "variance_per_file_us",
        &format!(
            "{:.1}",
            1000.0 * (one_med - phase1_med).max(0.0) / facts.files as f64
        ),
        "us",
        "单文件串行读+匹配成本（阶段 2 / 文件数）",
    );
    harness::info_line(
        "variance_search_minus_walk_read",
        &format!("{:.1}", auto_med - walk_read_med),
        "ms",
        "并行 search（auto）− 单线程裸遍历+读内容（负数=并行已比串行裸读快这么多）",
    );
    harness::info_line(
        "variance_io_parallel_speedup",
        &format!("{:.2}×", walk_read_med / auto_med.max(0.001)),
        "x",
        "单线程裸遍历+读 / 并行 search：并行把同一份 IO 摊薄了几倍（这就是算法的全部收益来源）",
    );
    harness::info_line(
        "variance_auto_vs_one",
        &format!("{:.2}×", one_med / auto_med.max(0.001)),
        "x",
        "1 线程 / auto：并行化带来的加速比（>1 才说明并行真的在摊薄 IO）",
    );
    harness::info_line(
        "variance_cpu_headroom",
        &format!("线程数上限常量 = {MAX_THREADS}"),
        "-",
        "若 auto(20) 已到收益拐点，再抬这个常量没有意义",
    );
    snapshot("end");

    harness::section("9. 判读提示（本实验不下结论，结论写进 S8）");
    println!(
        "1) 若 variance_threads_auto 与 8/14 的差异在噪声内、且 >1 线程远快于 1 线程，\n   \
         则并行化摊薄的是同一份 IO，不是 CPU —— 抬线程数上限不会变快。\n\
         2) 若 variance_consecutive 的 p95 明显低于跨进程离群值，说明离群来自进程外\n   \
         （负载/缓存/Defender），不是算法；反过来若同进程也能复现 >1s，才值得改 search.rs。\n\
         3) 若 variance_phase1_share 很大而 variance_sort_10k_paths 只在 µs 级，\n   \
         则阶段 1 的耗时在遍历/metadata，不在排序。\n\
         4) variance_*_ratio 给出「要让 250ms 变 1s，需要多少倍外部争用」的量级刻度：\n   \
         倍数在 1.0–1.3 之间时，不该用争用去解释 4× 的离群。"
    );
    Ok(())
}

/// 一次搜索的原始观测。
struct Run {
    elapsed: Duration,
    hits: usize,
    first_hit: Option<Duration>,
    files_scanned: u64,
    limit_reached: bool,
}

/// 跑一次搜索，记录墙钟、命中数、首条命中时刻。
fn measure(
    workspace: &Workspace,
    query: &str,
    options: &SearchOptions,
    threads: usize,
) -> Result<Run, Box<dyn Error>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut hits = 0usize;
    let mut first_hit: Option<Duration> = None;
    let started = Instant::now();
    let summary = workspace.search_with_threads(query, options, cancel, threads, |_hit| {
        if first_hit.is_none() {
            first_hit = Some(started.elapsed());
        }
        hits += 1;
    })?;
    Ok(Run {
        elapsed: started.elapsed(),
        hits,
        first_hit,
        files_scanned: summary.files_scanned,
        limit_reached: summary.limit_reached,
    })
}

/// 把一件事重复 `times` 次，收集每次的墙钟。
fn repeat<F>(times: usize, mut body: F) -> Result<Vec<Duration>, Box<dyn Error>>
where
    F: FnMut() -> Result<Duration, Box<dyn Error>>,
{
    let mut out = Vec::with_capacity(times);
    for _ in 0..times {
        out.push(body()?);
    }
    Ok(out)
}

/// 打印一组数字的 min/median/p95/max 与超线次数。
fn print_dist(name: &str, samples: &[Duration], note: &str) {
    if samples.is_empty() {
        harness::info_line(name, "无样本", "ms", note);
        return;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let min = sorted[0];
    let median = harness::percentile(&sorted, 50.0);
    let p95 = harness::percentile(&sorted, 95.0);
    let max = sorted[sorted.len() - 1];
    let over = sorted.iter().filter(|d| **d > BUDGET).count();
    harness::info_line(
        name,
        &format!(
            "med {:.1} / p95 {:.1} / max {:.1} / min {:.1} / n={} / >1s {}",
            as_ms(median),
            as_ms(p95),
            as_ms(max),
            as_ms(min),
            samples.len(),
            over
        ),
        "ms",
        note,
    );
}

/// 打印「during / before」的倍数（用各自中位数）。
fn print_ratio(name: &str, before: &[Duration], during: &[Duration]) {
    let ratio = median_ms(during) / median_ms(before).max(0.001);
    harness::info_line(
        name,
        &format!("{ratio:.2}×"),
        "x",
        "争用态中位数 / 安静态中位数",
    );
}

/// 中位数（ms）。
fn median_ms(samples: &[Duration]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    as_ms(harness::percentile(&sorted, 50.0))
}

/// `Duration` → ms 浮点。
fn as_ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

/// 裸 std 递归遍历：返回 `(文件数, 字节数)`；`read` 为真时连内容一起读。
fn walk(root: &Path, read: bool) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                let mut bytes = metadata.len();
                if read {
                    bytes = std::fs::read(entry.path())?.len() as u64;
                }
                black_box(bytes);
                files += 1;
            }
        }
    }
    black_box(files);
    Ok(started.elapsed())
}

/// 裸遍历 `①`：只走目录项，用 `DirEntry::metadata`（search.rs 阶段 1 的做法）。
fn walk_entry_metadata(root: &Path) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                black_box(entry.metadata()?.len());
                files += 1;
            }
        }
    }
    black_box(files);
    Ok(started.elapsed())
}

/// 裸遍历 `②`：每文件一次 `fs::metadata(path)`（bench 的 `measure_walk` 的做法）。
fn walk_path_stat(root: &Path) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                black_box(std::fs::metadata(entry.path())?.len());
                files += 1;
            }
        }
    }
    black_box(files);
    Ok(started.elapsed())
}

/// 把全树正文读进内存（只做一次，供内存内匹配用时）。
fn read_all(root: &Path) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    let mut bodies = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            } else if entry.file_type()?.is_file() {
                bodies.push(std::fs::read(entry.path())?);
            }
        }
    }
    Ok(bodies)
}

/// 全树相对路径（与 `search.rs` 的候选 label 同源：`/` 分隔、字典序）。
fn all_relative_paths(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            } else if entry.file_type()?.is_file() {
                let path = entry.path();
                let relative = path.strip_prefix(root)?;
                paths.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    paths.sort_unstable();
    Ok(paths)
}

/// 按搜索实现的方式切批（每批 512、批内再按 `div_ceil` 切片），看负载是否真的不均。
fn batch_balance(root: &Path, sorted_paths: &[String]) -> Result<(), Box<dyn Error>> {
    let mut items: Vec<(String, u64)> = Vec::with_capacity(sorted_paths.len());
    for relative in sorted_paths {
        let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        items.push((relative.clone(), std::fs::metadata(&path)?.len()));
    }
    items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let batches = items.len().div_ceil(BATCH_FILES);
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, MAX_THREADS);
    let mut bytes_per_batch = Vec::with_capacity(batches);
    let mut worst_worker_ratio = 0.0_f64;
    let mut worst_batch = (0usize, 0.0_f64);
    for (index, batch) in items.chunks(BATCH_FILES).enumerate() {
        let batch_bytes: u64 = batch.iter().map(|(_, len)| *len).sum();
        bytes_per_batch.push(batch_bytes);
        let per_worker = batch.len().div_ceil(workers).max(1);
        let mut slices: Vec<u64> = batch
            .chunks(per_worker)
            .map(|slice| slice.iter().map(|(_, len)| *len).sum())
            .collect();
        slices.sort_unstable();
        let mean = batch_bytes as f64 / slices.len() as f64;
        let max = *slices.last().unwrap_or(&0) as f64;
        let ratio = max / mean.max(1.0);
        if ratio > worst_worker_ratio {
            worst_worker_ratio = ratio;
            worst_batch = (index, ratio);
        }
    }
    let min = *bytes_per_batch.iter().min().unwrap_or(&0);
    let max = *bytes_per_batch.iter().max().unwrap_or(&0);
    harness::info_line(
        "variance_batch_bytes",
        &format!(
            "{batches} 批 / 每批字节 min {min} / max {max} / max÷min {:.3}",
            max as f64 / min.max(1) as f64
        ),
        "B",
        "批间负载（512 文件一批，按候选排序切）",
    );
    harness::info_line(
        "variance_batch_last",
        &format!("{} 文件", items.len() % BATCH_FILES),
        "files",
        "末尾批文件数（10000 mod 512 = 272；越接近 512 越说明几乎没有尾批损耗）",
    );
    harness::info_line(
        "variance_batch_worker_ratio",
        &format!(
            "最坏 {:.3}×（第 {} 批，{} workers）",
            worst_worker_ratio, worst_batch.0, workers
        ),
        "x",
        "批内按自动线程数均分后的最大切片字节 / 平均切片字节",
    );
    Ok(())
}

/// 采样一次机器负载（PowerShell；拿不到就写“未测”，不猜）。
fn snapshot(label: &str) {
    let script = r#"$cpu = (Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
$builds = @(Get-Process -Name cargo,rustc,link,cc1plus -ErrorAction SilentlyContinue).Count
$os = Get-CimInstance Win32_OperatingSystem
"load={0:N0}% builds={1} freeMB={2:N0}" -f $cpu, $builds, ($os.FreePhysicalMemory/1KB)"#;
    match harness::run_powershell(script) {
        Ok(lines) => harness::info_line(
            &format!("variance_env_{label}"),
            lines.last().map(String::as_str).unwrap_or("空输出"),
            "snapshot",
            "Win32_Processor 平均负载 / 编译进程数 / 空闲物理内存",
        ),
        Err(error) => harness::info_line(
            &format!("variance_env_{label}"),
            "未测",
            "-",
            &format!("PowerShell 采样失败：{error}"),
        ),
    }
}

/// 可以随时喊停的负载线程。
struct Hogs {
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Hogs {
    /// CPU 忙等线程。
    fn cpu(count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(count);
        for _ in 0..count {
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut acc: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..200_000u64 {
                        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
                        acc ^= acc >> 17;
                    }
                    black_box(acc);
                }
            }));
        }
        Self { stop, handles }
    }

    /// 同盘 IO 线程：循环把整棵树读一遍。
    fn io(root: PathBuf, count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(count);
        for _ in 0..count {
            let stop = Arc::clone(&stop);
            let root = root.clone();
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Err(error) = walk(&root, true) {
                        black_box(&error);
                    }
                }
            }));
        }
        Self { stop, handles }
    }

    /// 停掉并等线程退出（否则「after」那一档还在争用）。
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}
