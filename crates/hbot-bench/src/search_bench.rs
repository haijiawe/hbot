//! 预算 1：全局搜索首屏 < 1 s（10,000 文件，固定夹具）。
//!
//! 数据怎么造的（也打在结果里）：
//!
//! * 10 个 `pkg*` × 10 个 `mod*` × 100 个 `file_NNN.txt` = **10,000 个文件**，三层目录；
//! * 每个文件 **200 B – 4 KiB**（按索引确定性分布，中位约 2 KiB，总约 21 MB）的真实 UTF-8
//!   文本，不是随机字节、不是空文件；
//! * 命中词 `HBOT_NEEDLE_9F3A` 只埋在 `index % 100 == 7` 的 **100 个文件（1.0%）**，
//!   每个文件一行 —— 与“约 1% 文件含关键词”的设定一致；
//! * 走 `hbot_workspace::Workspace::search` 的**默认 `SearchOptions`**
//!   （字面量、大小写不敏感、无 glob、`max_results = 1000`、单文件上限 1 MiB）。
//!
//! ## 三条口径，分开报（不要用一个数掩盖三段时间）
//!
//! | 行 | 是什么 | 计入预算 |
//! |---|---|---|
//! | `search_10k_fixture_build` | 造 10,000 个文件 + 100 个目录的 wall time（**建树**） | 否 |
//! | `search_10k_cold_first_call` | 进程内**第 1 次** `search()` | 是 |
//! | `search_10k_warm` | 第 2–4 次的**中位数** | 是 |
//! | `search_10k_screen_100hits` | 第 100 条命中出现的时刻（首屏代理，热缓存） | 是 |
//! | `search_10k_no_hit` | 查不存在的词，必然扫完 10k（最坏情况，热缓存） | 是 |
//!
//! ## 冷/热的诚实说明
//!
//! 夹具由**本进程**写出，做第 1 次搜索时这些文件的内容已经在 Windows 文件缓存里，
//! 所以 `cold_first_call` 是**真冷盘性能的下界（偏乐观）**，不是真冷盘数字。要拿到真冷盘
//! 得清 standby list（`RAMMap -EmptyStandbyList`，管理员）或换刚开机的机器 —— 本次测量
//! 没有管理员权限，做不到，因此**不冒充**冷盘数据，只标注为“乐观下界”。
//!
//! 另外，本机测量期间有其他代理在并发 `cargo` 编译，冷/热两次的差异会被负载噪声淹没
//! （见 S8 的实测：第 1 次反而比第 2、3 次快）。所以判定按两者**都**超预算写成 FAIL，
//! 而不是挑一个好看的数字。
//!
//! ## IO 归因（超预算时必须做）
//!
//! 分三段量，逐段扣：
//!
//! 1. `walk_metadata`：std 递归 `read_dir` + 每个文件一次 `fs::metadata`（只有元数据，**不读内容**）；
//! 2. `walk_read`：在第 1 段基础上再加 `fs::read` 全部内容（**不匹配**）；
//! 3. `search`：真正的 `Workspace::search()`。
//!
//! 于是：`元数据 = 1`、`内容读取 = 2 − 1`、`匹配 = 3 − 2`。若第 1 段就占了大头，瓶颈是
//! 文件系统/元数据路径（Windows 上还有 Defender 的过滤驱动做按访问扫描），**不是**匹配算法。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use hbot_workspace::{SearchOptions, Workspace};

use crate::harness::{self, Stats, TempDir, Unit};

/// 固定命中词：只埋在约 1% 的文件里。
pub const NEEDLE: &str = "HBOT_NEEDLE_9F3A";
/// 任何文件都不含的词：强制走完整扫描（最坏情况）。
pub const ABSENT: &str = "HBOT_ABSENT_TOKEN_ZZZ";
/// 热缓存口径复跑几次（取中位数）。
const WARM_RUNS: usize = 3;
/// 夹具文件数（三层 10×10×100）。
const FILES: usize = 10_000;
/// 单文件下限（任务口径 200 B–4 KiB）。
const MIN_BODY: usize = 200;
/// 单文件上限。
const MAX_BODY: usize = 4096;
/// 预算：全局搜索首屏 < 1 s。
const BUDGET: Duration = Duration::from_millis(1000);

/// 夹具的规模事实，用于结果行的“数据规模说明”。
struct Fixture {
    files: usize,
    dirs: usize,
    hit_files: usize,
    min_bytes: usize,
    max_bytes: usize,
    total_bytes: usize,
}

/// 跑 10k 文件搜索测量并打印结果行。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    harness::require_release();
    harness::section("1. 全局搜索：10,000 文件（hbot_workspace::search，默认 SearchOptions）");

    let temp = TempDir::new("search")?;
    let root = temp.path().to_path_buf();
    let build_started = Instant::now();
    let fixture = build_fixture(&root)?;
    let build_time = build_started.elapsed();

    harness::info_line(
        "search_fixture",
        &format!("{} 文件 / {} 目录", fixture.files, fixture.dirs),
        "files",
        &format!(
            "三层目录 10×10×100；单文件 {}–{} B 真实文本（合计 {:.1} MB）；{} 个文件（{:.1}%）埋 `{NEEDLE}`",
            fixture.min_bytes,
            fixture.max_bytes,
            fixture.total_bytes as f64 / 1_048_576.0,
            fixture.hit_files,
            100.0 * fixture.hit_files as f64 / fixture.files as f64
        ),
    );
    harness::info_line(
        "search_10k_fixture_build",
        &Unit::Ms.render(build_time),
        "ms",
        "**建树**（造 10,000 个文件 + 100 个目录）的 wall time —— 不计入搜索预算，但必须报，否则读者会以为搜索本身要十几秒",
    );

    let workspace = Workspace::open(root.clone())?;
    let options = SearchOptions::default();
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    harness::info_line(
        "search_10k_threads",
        &format!("{cores}"),
        "threads",
        "并行搜索的工作线程数（`available_parallelism()`，`search()` 默认走它；上限 64）。下面所有搜索数字都在这条线程数下测得",
    );

    // 1 次冷（进程内第一次）+ WARM_RUNS 次热。
    let mut scans = Vec::with_capacity(1 + WARM_RUNS);
    let mut screen = Vec::with_capacity(1 + WARM_RUNS);
    let mut first_hit = Vec::with_capacity(1 + WARM_RUNS);
    let mut hits_per_run = Vec::with_capacity(1 + WARM_RUNS);
    let mut scanned_per_run = Vec::with_capacity(1 + WARM_RUNS);
    let mut truncated = false;

    for _ in 0..(1 + WARM_RUNS) {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut hits = 0usize;
        let mut first = Duration::ZERO;
        let mut hundredth = Duration::ZERO;
        let started = Instant::now();
        let summary = workspace.search(NEEDLE, &options, cancel, |_hit| {
            hits += 1;
            if hits == 1 {
                first = started.elapsed();
            }
            if hits == 100 {
                hundredth = started.elapsed();
            }
        })?;
        scans.push(started.elapsed());
        first_hit.push(first);
        screen.push(hundredth);
        hits_per_run.push(summary.hits);
        scanned_per_run.push(summary.files_scanned);
        truncated |= summary.limit_reached;
    }

    let mut no_hit = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let summary = workspace.search(ABSENT, &options, cancel, |_hit| {})?;
        no_hit.push(started.elapsed());
        if summary.hits != 0 {
            return Err(format!("无命中查询居然命中了 {} 条", summary.hits).into());
        }
    }

    // 串行对照：同一夹具、同一查询，强制 1 个工作线程。
    // 作用是把「并行收益」与「10k 小文件读盘的地板价」分开报 —— 否则读者无法判断剩下的
    // 时间到底是并行没生效，还是 IO 本身就要这么久。同时顺手钉住「并行与串行命中数相同」。
    let mut serial = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut hits = 0usize;
        let started = Instant::now();
        let summary = workspace.search_with_threads(NEEDLE, &options, cancel, 1, |_hit| {
            hits += 1;
        })?;
        serial.push(started.elapsed());
        if hits != summary.hits {
            return Err(format!(
                "串行搜索回调命中数 {hits} 与 summary.hits {} 不一致",
                summary.hits
            )
            .into());
        }
        if hits != hits_per_run[1] {
            return Err(format!(
                "串行命中 {hits} 条 ≠ 并行命中 {} 条 —— 并行版漏了或多算了文件",
                hits_per_run[1]
            )
            .into());
        }
    }
    let serial_stats = Stats::of(&serial);
    harness::info_line(
        "search_10k_serial_1thread",
        &Unit::Ms.render(serial_stats.median),
        "ms",
        &format!(
            "同一查询强制 1 线程的中位（热，{} 次）—— 与 `search_10k_warm` 相减即并行的净收益；命中数已断言与并行一致",
            WARM_RUNS
        ),
    );

    for (index, elapsed) in scans.iter().enumerate() {
        harness::info_line(
            &format!("search_10k.run{}", index + 1),
            &Unit::Ms.render(*elapsed),
            "ms",
            &format!(
                "第 {} 次{}：命中 {} 条 / 扫过 {} 文件",
                index + 1,
                if index == 0 {
                    "（冷：进程内首次调用）"
                } else {
                    "（热）"
                },
                hits_per_run[index],
                scanned_per_run[index]
            ),
        );
    }

    harness::info_line(
        "search_10k_hits",
        &format!("{}", hits_per_run[0]),
        "hits",
        &format!(
            "期望 {} 条（每个命中文件各 1 行）；默认 max_results=1000 {}",
            fixture.hit_files,
            if truncated {
                "已触发截断 —— 结果不完整"
            } else {
                "未触发截断（结果完整）"
            }
        ),
    );
    harness::info_line(
        "search_10k_first_hit",
        &Unit::Ms.render(Stats::of(&first_hit).median),
        "ms",
        "流式首屏的最早形态：第 1 条命中出现的时刻（不参与判定）",
    );

    let cold = Stats::of(&scans[..1]);
    let warm = Stats::of(&scans[1..]);
    harness::budget_line(
        "search_10k_cold_first_call",
        &cold,
        Unit::Ms,
        BUDGET,
        "冷缓存口径 = 进程内第 1 次 search()。夹具刚由本进程写出 ⇒ 文件缓存已命中，这是真冷盘的**乐观下界**（真冷盘只会更慢）",
    );
    harness::budget_line(
        "search_10k_warm",
        &warm,
        Unit::Ms,
        BUDGET,
        "热缓存口径 = 第 2–4 次的中位数；跑完整个 10,000 文件（search API 是同步的，没有“只跑首屏”的入口）",
    );
    harness::budget_line(
        "search_10k_screen_100hits",
        &Stats::of(&screen[1..]),
        Unit::Ms,
        BUDGET,
        "首屏代理（热）：第 100 条命中的时刻；命中词均匀分布在 10 个 pkg 里 ⇒ 约等于全量扫描，是**对首屏最不利**的夹具（换“命中集中在前几个目录”的夹具会好看，那是把夹具改绿）",
    );
    harness::budget_line(
        "search_10k_no_hit",
        &Stats::of(&no_hit),
        Unit::Ms,
        BUDGET,
        "查询不存在的词：必然扫完全部 10,000 个文件，最坏情况下界（热缓存）",
    );

    // ---- IO 归因：元数据 / 内容 / 匹配 三段分开量 ----------------------
    let walk_meta = measure_walk(&root, false, WARM_RUNS)?;
    let walk_read = measure_walk(&root, true, WARM_RUNS)?;
    let meta_ms = Stats::of(&walk_meta).median.as_secs_f64() * 1e3;
    let read_ms = Stats::of(&walk_read).median.as_secs_f64() * 1e3;
    let scan_ms = warm.median.as_secs_f64() * 1e3;

    harness::info_line(
        "search_10k_walk_metadata",
        &format!("{meta_ms:.1}"),
        "ms",
        "归因段 1（不判定）：std 递归 read_dir + 每文件一次 fs::metadata，只碰元数据、不读内容",
    );
    harness::info_line(
        "search_10k_walk_read",
        &format!("{read_ms:.1}"),
        "ms",
        "归因段 2（不判定）：段 1 + fs::read 全部内容，不做任何匹配（无 ignore 的 gitignore 处理）",
    );

    let content_ms = (read_ms - meta_ms).max(0.0);
    let meta_pct = meta_ms / scan_ms * 100.0;
    let content_pct = content_ms / scan_ms * 100.0;
    // 并行搜索之后，单线程的 std::fs 参照可能比搜索本身还慢（参照没有并行）。
    // 那时「search − 参照」是负数，占比算出来会 >100%，纯属分母倒挂 —— 只报绝对值。
    if scan_ms < read_ms {
        harness::info_line(
            "search_attribution",
            &format!(
                "热搜索 {scan_ms:.1} ms < 单线程参照 {read_ms:.1} ms（元数据 {meta_ms:.1} + 内容读取 {content_ms:.1}）= {:.1}×；单文件 {:.0} µs",
                read_ms / scan_ms,
                scan_ms * 1e3 / fixture.files as f64
            ),
            "-",
            "此时不能再按「search − 参照」算占比（分母已小于被减数）；参照是单线程 std::fs，而搜索已并行，只报绝对值",
        );
        harness::info_line(
            "search_attribution_verdict",
            &format!("并行搜索比单线程裸遍历+整读快 {:.1}×", read_ms / scan_ms),
            "-",
            "逐文件 IO 仍是主要成本（并行只是把它摊到多个核心上）；剩余时间就是 10,000 次 open/read/close 本身，不是匹配算法",
        );
    } else {
        let match_ms = (scan_ms - read_ms).max(0.0);
        let match_pct = match_ms / scan_ms * 100.0;
        harness::info_line(
            "search_attribution",
            &format!(
                "热搜索 {scan_ms:.1} ms = 元数据 {meta_ms:.1} ms（{meta_pct:.0}%）+ 内容读取 {content_ms:.1} ms（{content_pct:.0}%）+ 匹配 {match_ms:.1} ms（{match_pct:.0}%）；单文件 {:.0} µs",
                scan_ms * 1e3 / fixture.files as f64
            ),
            "-",
            "三段是逐段相减得到的（search − walk_read − walk_metadata），所以是归因不是精确拆分",
        );

        let (dominant, dominant_pct, conclusion) = if meta_pct >= content_pct
            && meta_pct >= match_pct
        {
            (
                "文件系统元数据（10,000 次 read_dir 条目 + stat）",
                meta_pct,
                "瓶颈在文件系统层：Windows 的 Defender 过滤驱动会对每次 open/stat 做按访问扫描，纯遍历也不便宜。要压这个数，方向是减少每文件系统调用（并发/批量/索引），不是优化正则",
            )
        } else if content_pct >= match_pct {
            (
                "文件内容读取（10,000 次 open/read/close）",
                content_pct,
                "瓶颈在逐个打开读文件；search API 每次 fs::read 整文件，没有“只读前 N KB”的入口",
            )
        } else {
            (
                "匹配（逐文件 searcher + 正则）",
                match_pct,
                "瓶颈在匹配路径本身；看下面 inmem_match 参照物区分“算法慢”与“每文件固定开销”",
            )
        };
        harness::info_line(
            "search_attribution_verdict",
            &format!("{dominant}占 {dominant_pct:.0}%"),
            "-",
            conclusion,
        );
    }

    // ---- 归因（更细）：同样的内容已在内存里，纯匹配 / 纯 UTF-8 校验各要多久 ------
    // 上面的“匹配”桶里含多一次 stat、NUL 扫描、UTF-8 校验、grep-searcher 每文件 setup，
    // 所以它不等于“算法慢”。这里把纯匹配抽出来当参照物（不参与判定）。
    let bodies = read_all_bodies(&root)?;
    let texts: Vec<&str> = bodies
        .iter()
        .map(|body| std::str::from_utf8(body).expect("夹具必须是合法 UTF-8"))
        .collect();
    let (inmem_match, inmem_utf8, inmem_hits) = measure_inmemory(&bodies, &texts, WARM_RUNS);
    let inmem_match_ms = Stats::of(&inmem_match).median.as_secs_f64() * 1e3;
    let inmem_utf8_ms = Stats::of(&inmem_utf8).median.as_secs_f64() * 1e3;
    assert_eq!(
        inmem_hits, fixture.hit_files,
        "内存里纯匹配的命中文件数与夹具不符"
    );
    harness::info_line(
        "search_10k_inmem_match",
        &format!("{inmem_match_ms:.2}"),
        "ms",
        "参考（不判定）：10,000 份内容已读入内存，只做 str::contains 纯匹配（不含 IO / stat / NUL 扫描 / UTF-8 校验 / 每文件 searcher setup）",
    );
    harness::info_line(
        "search_10k_inmem_utf8",
        &format!("{inmem_utf8_ms:.2}"),
        "ms",
        "参考（不判定）：只做 std::str::from_utf8 全量校验（10,000 份内容，内存里）",
    );
    harness::info_line(
        "search_10k_inmem_verdict",
        &format!(
            "纯匹配 {:.2} ms（{:.1}%）+ UTF-8 {:.2} ms（{:.1}%）；其余 {:.1} ms 是逐文件固定开销",
            inmem_match_ms,
            inmem_match_ms / scan_ms * 100.0,
            inmem_utf8_ms,
            inmem_utf8_ms / scan_ms * 100.0,
            (scan_ms - read_ms - inmem_match_ms - inmem_utf8_ms).max(0.0)
        ),
        "-",
        "归因：纯匹配算法本身只占全量扫描的极小部分；剩下的“匹配桶”是每文件固定开销（stat / NUL / 行解码 / searcher setup）与 Windows 文件系统过滤驱动",
    );

    Ok(())
}

/// 把夹具内容再读一遍进内存（一次性，不计入测量）。
fn read_all_bodies(root: &Path) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let mut bodies = Vec::with_capacity(FILES);
    collect_bodies(root, &mut bodies)?;
    assert_eq!(bodies.len(), FILES, "读回的文件数不对");
    Ok(bodies)
}

fn collect_bodies(dir: &Path, bodies: &mut Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_bodies(&path, bodies)?;
        } else {
            bodies.push(std::fs::read(&path)?);
        }
    }
    Ok(())
}

/// 参考（不判定）：纯匹配与纯 UTF-8 校验的耗时，返回 (匹配, 校验, 命中文件数)。
fn measure_inmemory(
    bodies: &[Vec<u8>],
    texts: &[&str],
    runs: usize,
) -> (Vec<Duration>, Vec<Duration>, usize) {
    let mut match_samples = Vec::with_capacity(runs);
    let mut utf8_samples = Vec::with_capacity(runs);
    let mut hits = 0usize;

    for _ in 0..runs {
        let started = Instant::now();
        hits = texts.iter().filter(|text| text.contains(NEEDLE)).count();
        match_samples.push(started.elapsed());
    }
    for _ in 0..runs {
        let started = Instant::now();
        let mut bytes = 0usize;
        for body in bodies {
            bytes += std::str::from_utf8(body)
                .expect("夹具必须是合法 UTF-8")
                .len();
        }
        utf8_samples.push(started.elapsed());
        assert!(bytes > 0, "UTF-8 校验没读到字节");
    }
    (match_samples, utf8_samples, hits)
}

/// 归因：std 递归遍历全部文件；`read_contents` 决定要不要把内容也读出来。
fn measure_walk(
    root: &Path,
    read_contents: bool,
    runs: usize,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let mut files = 0usize;
        let mut bytes = 0usize;
        walk(root, read_contents, &mut files, &mut bytes)?;
        samples.push(started.elapsed());
        assert_eq!(files, FILES, "参考遍历的文件数不对");
        if read_contents {
            assert!(bytes > 0, "参考遍历没读到字节");
        }
    }
    Ok(samples)
}

/// 递归读目录；每个文件都 stat 一次（`fs::metadata` 是真实的系统调用，
/// 会被 Defender 的过滤驱动看到 —— 这正是要量出来的那部分开销）。
fn walk(
    dir: &Path,
    read_contents: bool,
    files: &mut usize,
    bytes: &mut usize,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, read_contents, files, bytes)?;
        } else {
            let _metadata = std::fs::metadata(&path)?;
            if read_contents {
                *bytes += std::fs::read(&path)?.len();
            }
            *files += 1;
        }
    }
    Ok(())
}

/// 造出 10,000 个文件并返回规模事实。
fn build_fixture(root: &Path) -> Result<Fixture, Box<dyn std::error::Error>> {
    let mut fixture = Fixture {
        files: 0,
        dirs: 0,
        hit_files: 0,
        min_bytes: usize::MAX,
        max_bytes: 0,
        total_bytes: 0,
    };
    for package in 0..10usize {
        for module in 0..10usize {
            let dir = root
                .join(format!("pkg{package}"))
                .join(format!("mod{module}"));
            std::fs::create_dir_all(&dir)?;
            fixture.dirs += 1;
            for file in 0..100usize {
                let index = package * 1_000 + module * 100 + file;
                let body = file_body(package, module, file, index);
                assert!(
                    (MIN_BODY..=MAX_BODY).contains(&body.len()),
                    "夹具单文件必须是 {MIN_BODY}–{MAX_BODY} B，实际 {} B（pkg{package}/mod{module}/file_{file:03}）",
                    body.len()
                );
                std::fs::write(dir.join(format!("file_{file:03}.txt")), &body)?;
                fixture.files += 1;
                fixture.min_bytes = fixture.min_bytes.min(body.len());
                fixture.max_bytes = fixture.max_bytes.max(body.len());
                fixture.total_bytes += body.len();
                if index % 100 == 7 {
                    fixture.hit_files += 1;
                }
            }
        }
    }
    assert_eq!(fixture.files, FILES, "夹具文件数不对");
    Ok(fixture)
}

/// 夹具规模事实，供机制实验（`examples/search_variance.rs`）复用同一棵树。
#[derive(Debug, Clone, Copy)]
pub struct FixtureFacts {
    /// 文件数。
    pub files: usize,
    /// 目录数。
    pub dirs: usize,
    /// 含 [`NEEDLE`] 的文件数。
    pub hit_files: usize,
    /// 单文件最小字节数。
    pub min_bytes: usize,
    /// 单文件最大字节数。
    pub max_bytes: usize,
    /// 全部正文的总字节数。
    pub total_bytes: usize,
}

/// 造一份与 [`run`] **完全相同**的 10k 夹具，返回规模事实。
///
/// 存在的理由只有一个：耗时离散的机制实验必须量在**同一棵树**上，否则它的数字
/// 与预算行没法互相解释。夹具逻辑只有 [`build_fixture`] 一份，别在这里抄第二份。
pub fn build_fixture_for_variance(root: &Path) -> Result<FixtureFacts, Box<dyn std::error::Error>> {
    let fixture = build_fixture(root)?;
    Ok(FixtureFacts {
        files: fixture.files,
        dirs: fixture.dirs,
        hit_files: fixture.hit_files,
        min_bytes: fixture.min_bytes,
        max_bytes: fixture.max_bytes,
        total_bytes: fixture.total_bytes,
    })
}

/// 一个夹具文件的正文：两行头 + 填充常量 + （1% 的文件）一行命中词，
/// 长度按索引确定性落在 `MIN_BODY..=MAX_BODY` 内（末尾用空格补齐到目标长度）。
fn file_body(package: usize, module: usize, file: usize, index: usize) -> String {
    let target = (MIN_BODY + (index * 397) % 3901).min(MAX_BODY);
    let mut body = String::with_capacity(target);
    body.push_str(&format!("// pkg{package}/mod{module}/file_{file:03}.txt\n"));
    body.push_str(&format!(
        "export const id = \"pkg{package}_mod{module}_file{file:03}\";\n"
    ));
    if index % 100 == 7 {
        body.push_str(&format!("const needle = \"{NEEDLE}\";\n"));
    }
    let mut line = 0usize;
    while body.len() + 52 < target {
        body.push_str(&format!(
            "const f{line} = \"lorem ipsum dolor sit amet {index:04}\";\n"
        ));
        line += 1;
    }
    // 用空格把最后一行补齐到目标长度（仍是合法 UTF-8 文本，不影响匹配）。
    let remainder = target.saturating_sub(body.len());
    if remainder > 1 {
        body.push_str(&" ".repeat(remainder - 1));
    }
    if remainder > 0 {
        body.push('\n');
    }
    body
}
