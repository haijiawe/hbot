//! 项目内文本搜索。
//!
//! 遍历用 [`ignore::WalkBuilder`]（遵守 `.gitignore` / `.ignore`、跳过隐藏文件），
//! 匹配用 `grep-searcher` + `grep-regex`，命中逐条经回调交给调用方（不在本模块累积
//! 全部结果，内存上界是 `max_results` 与单文件 `max_file_size`）。
//!
//! 编码与二进制处理（不静默，全部计入 [`SearchSummary`]）：
//!
//! - 含 NUL 字节的文件视为二进制，跳过并计入 `files_skipped_binary`；
//! - 非 UTF-8 文件跳过并计入 `files_skipped_non_utf8`；
//! - 超过 `max_file_size` 的文件跳过并计入 `files_skipped_too_large`；
//! - 指向根外的符号链接跳过并计入 `files_skipped_outside`。
//!
//! 为了让上面三类计数**准确**，每个文件先按大小上限整块读入再分类、再搜索；
//! 真正的流式是“逐命中回调”，不是“逐字节读盘”。
//!
//! # 两阶段执行与确定性
//!
//! 1. **遍历阶段**（单线程，只 `read_dir`，不读内容）：把候选文件收成一张清单，按
//!    相对路径排序。`DirEntry` 自带的元数据用来做 `max_file_size` 预筛（不额外 stat）。
//! 2. **扫描阶段**（`std::thread::scope` 并行）：候选清单按固定大小分批，批内按
//!    `ceil(批内文件数 / 线程数)` 静态切片分给各线程（同一批内谁先跑完不影响结果，
//!    因为批内命中随后统一排序）；每批结束后排序并回调，凑够 `max_results` 就停。
//!
//! 路径升序的候选清单是“确定性”的来源：批边界由清单决定，不由线程调度决定；批与批
//! 之间的路径严格递增，所以“批顺序 + 批内排序”就是全局 `(path, line_number)` 升序。
//! 因此**并行版与 `threads = 1` 的串行版逐条相同**，只有快慢差别。
//!
//! 内存上界（都是硬上界，不随文件数增长成无界命中收集）：
//!
//! - 候选清单：`O(文件数)`（10k 文件约 1 MiB，见 `docs/spikes/S8-perf-budgets.md`）；
//! - 命中缓冲：每批至多 `线程数 × max_results` 条，回调后立即释放。

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use hbot_core::HbotError;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::project::Workspace;

/// 单文件搜索的硬上限：8 MiB（超过则不算“文本搜索”了）。
pub const MAX_SEARCH_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// 默认结果上限。
pub const DEFAULT_MAX_RESULTS: usize = 1000;

/// 默认单文件上限：1 MiB。
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1024 * 1024;

/// 一条命中。行号从 1 开始，`line` 已去掉行尾换行符。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// 相对项目根的路径，`/` 分隔。
    pub path: String,
    /// 行号（1 起）。
    pub line_number: u64,
    /// 命中的整行（已去掉行尾 `\r`/`\n`）。
    pub line: String,
}

/// 搜索选项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    /// `true` 时把 `query` 当正则；`false` 时按字面量（自动转义）。
    pub regex: bool,
    /// 区分大小写。默认 `false`（与常见 IDE 的 “Match Case” 关闭一致）。
    pub case_sensitive: bool,
    /// 包含/排除 glob，gitignore 语义：`*.ts` 只搜 TS，`!vendor/**` 排除目录。
    pub globs: Vec<String>,
    /// 命中条数上限，必须大于 0。
    pub max_results: usize,
    /// 单文件字节上限（另受 [`MAX_SEARCH_FILE_BYTES`] 约束）。
    pub max_file_size: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            regex: false,
            case_sensitive: false,
            globs: Vec::new(),
            max_results: DEFAULT_MAX_RESULTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// 一次搜索的统计。字段刻意全部显式，避免“跳过”被当成“没命中”。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    /// 返回的命中条数。
    pub hits: usize,
    /// 实际执行过搜索的文件数（不含被跳过者）。
    pub files_scanned: u64,
    /// 被跳过：含 NUL 的二进制文件。
    pub files_skipped_binary: u64,
    /// 被跳过：非 UTF-8 文件。
    pub files_skipped_non_utf8: u64,
    /// 被跳过：超过 `max_file_size`。
    pub files_skipped_too_large: u64,
    /// 被跳过：读取失败（权限、消失等）。
    pub files_skipped_unreadable: u64,
    /// 被跳过：符号链接指向根外。
    pub files_skipped_outside: u64,
    /// 是否因取消标志提前返回。
    pub cancelled: bool,
    /// 是否因 `max_results` 提前返回。
    pub limit_reached: bool,
}

impl Workspace {
    /// 搜索项目内的文本文件；命中通过 `sink` 逐条回调。
    ///
    /// 命中顺序**稳定**：按 `path` 升序、同文件内按行号升序。`SearchHit` 没有列号，
    /// 同一行最多产出一条命中，所以 `(path, line_number)` 就是全序 —— 并行化不会让
    /// 结果顺序随线程调度抖动。
    ///
    /// 并行度取 [`std::thread::available_parallelism`]；要固定线程数（基准对比、CI
    /// 复现）用 [`Workspace::search_with_threads`]。
    ///
    /// `cancel` 置位后在**下一个检查点**停下（遍历目录项、批次边界、单个文件读之前、
    /// 单个命中产出、回调返回之后）。返回的 [`SearchSummary::cancelled`] 为 `true`
    /// （已经交给 `sink` 的命中不撤回）。
    pub fn search<F>(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Arc<AtomicBool>,
        sink: F,
    ) -> Result<SearchSummary, HbotError>
    where
        F: FnMut(SearchHit),
    {
        self.search_with_threads(query, options, cancel, 0, sink)
    }

    /// 同 [`Workspace::search`]，但显式指定读盘/匹配的线程数。
    ///
    /// - `threads == 0`：自动，取 [`std::thread::available_parallelism`]；
    /// - `threads == 1`：退化为串行（命中与并行版逐条相同，只是慢）——基准里用它把
    ///   「去掉重复 stat」与「并行化」两项收益分开量。
    ///
    /// 线程数会被夹到 `1..=64`，实际不会超过候选文件数。
    pub fn search_with_threads<F>(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Arc<AtomicBool>,
        threads: usize,
        mut sink: F,
    ) -> Result<SearchSummary, HbotError>
    where
        F: FnMut(SearchHit),
    {
        if query.is_empty() {
            return Err(HbotError::invalid("search query is empty"));
        }
        if options.max_results == 0 {
            return Err(HbotError::invalid("max_results must be greater than 0"));
        }
        let matcher = build_matcher(query, options)?;
        let file_limit = options.max_file_size.clamp(1, MAX_SEARCH_FILE_BYTES);

        let mut summary = SearchSummary::default();
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            return Ok(summary);
        }

        // ---- 阶段 1：只走目录项，不读内容 -------------------------------
        //
        // 候选清单必须完整且按路径排序：这是「并行 + 提前终止」还能满足“先按
        // (path, line) 排序再截断”的唯一便宜办法 —— 排好序的路径前缀是确定的，
        // 于是“前 k 个候选里已有 ≥ max_results 条命中”是与线程调度无关的判据。
        let walker = self.build_walker(options)?;
        let mut candidates: Vec<Candidate> = Vec::new();
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(error = %err, "skipping unreadable walk entry");
                    summary.files_skipped_unreadable += 1;
                    continue;
                }
            };
            let path = entry.path();
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                continue;
            }
            // 用 walker 已缓存的元数据，不再 `fs::metadata(path)` 重打一次 stat。
            // Windows 上 `walkdir::DirEntry` 在 `read_dir` 时就把元数据存进了结构体
            // （walkdir-2.5.0/src/dent.rs：“comes for free while reading a directory”），
            // 所以这是 10k 文件省下 10k 次 stat 的那一处。非 Windows 上它退化成一次
            // `symlink_metadata`，与改前那次 `fs::metadata` 等价（`follow_links(false)`）。
            let (real, len) = if file_type.is_symlink() {
                let Ok(real) = fs::canonicalize(path) else {
                    summary.files_skipped_unreadable += 1;
                    continue;
                };
                if !self.is_within(&real) {
                    tracing::debug!(path = %path.display(), "skipping symlink escaping workspace root");
                    summary.files_skipped_outside += 1;
                    continue;
                }
                // 链接本身的元数据不含目标大小，链接是少数派，单独 stat 一次。
                match fs::metadata(&real) {
                    Ok(meta) if meta.is_file() => (real, meta.len()),
                    Ok(_) => continue,
                    Err(err) => {
                        tracing::debug!(path = %real.display(), error = %err, "skipping file without metadata");
                        summary.files_skipped_unreadable += 1;
                        continue;
                    }
                }
            } else {
                match entry.metadata() {
                    Ok(meta) if meta.is_file() => (path.to_path_buf(), meta.len()),
                    // FIFO/设备等：不是文本文件，不算跳过也不算扫描。
                    Ok(_) => continue,
                    Err(err) => {
                        tracing::debug!(path = %path.display(), error = %err, "skipping file without metadata");
                        summary.files_skipped_unreadable += 1;
                        continue;
                    }
                }
            };
            // 大小预筛在**读之前**，用的就是上面那份元数据：不再多打一次 stat，
            // 也绝不“先读后判”（那等于白读一遍大文件）。
            if len > file_limit {
                summary.files_skipped_too_large += 1;
                continue;
            }
            let Ok(label) = self.relative_path(path) else {
                summary.files_skipped_outside += 1;
                continue;
            };
            candidates.push(Candidate { label, real });
        }
        if summary.cancelled {
            return Ok(summary);
        }
        candidates.sort_unstable_by(|a, b| a.label.cmp(&b.label));

        // ---- 阶段 2：并行读 + 匹配 --------------------------------------
        let workers = resolve_threads(threads);
        let matcher = &matcher;
        let cancel = &cancel;
        let mut emitted = 0usize;
        for batch in candidates.chunks(SEARCH_BATCH_FILES) {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            let per_worker = batch.len().div_ceil(workers).max(1);
            let partials = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(batch.len().div_ceil(per_worker));
                for slice in batch.chunks(per_worker) {
                    handles
                        .push(scope.spawn(move || {
                            scan_files(slice, matcher, cancel, options.max_results)
                        }));
                }
                let mut joined = Vec::with_capacity(handles.len());
                for handle in handles {
                    // 工作线程内不 panic；真 panic 了也降级成“这一片没结果”，
                    // 不让整个搜索被一个坏文件带崩。
                    joined.push(handle.join().unwrap_or_default());
                }
                joined
            });

            let mut batch_hits: Vec<SearchHit> = Vec::new();
            for mut partial in partials {
                summary.files_scanned += partial.scanned;
                summary.files_skipped_binary += partial.binary;
                summary.files_skipped_non_utf8 += partial.non_utf8;
                summary.files_skipped_unreadable += partial.unreadable;
                batch_hits.append(&mut partial.hits);
            }
            // 批内排序；批与批之间的路径严格递增 ⇒ 整体是 (path, line) 升序。
            batch_hits.sort_by(|a, b| {
                (a.path.as_str(), a.line_number).cmp(&(b.path.as_str(), b.line_number))
            });

            let mut stop = false;
            for hit in batch_hits {
                if cancel.load(Ordering::Relaxed) {
                    summary.cancelled = true;
                    stop = true;
                    break;
                }
                sink(hit);
                emitted += 1;
                if emitted >= options.max_results {
                    summary.limit_reached = true;
                    stop = true;
                    break;
                }
            }
            if stop {
                break;
            }
        }

        summary.hits = emitted;
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
        }
        Ok(summary)
    }

    fn build_walker(&self, options: &SearchOptions) -> Result<ignore::Walk, HbotError> {
        let mut overrides = OverrideBuilder::new(self.root());
        for glob in &options.globs {
            overrides.add(glob).map_err(|err| {
                HbotError::invalid(format!("invalid search glob `{glob}`: {err}"))
            })?;
        }
        let overrides = overrides
            .build()
            .map_err(|err| HbotError::invalid(format!("invalid search globs: {err}")))?;

        // 显式跳过运行数据与内部临时文件；隐藏文件交给 `.hidden(true)`。
        let mut skip_names = self.config().skip_names.clone();
        skip_names.push(".git".to_owned());
        skip_names.push(".hbot".to_owned());

        let mut builder = WalkBuilder::new(self.root());
        builder
            .hidden(true)
            .follow_links(false)
            .parents(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            // 项目根不要求是 git 仓库：`.gitignore` 在没有 `.git/` 时也生效。
            .require_git(false)
            .overrides(overrides)
            .filter_entry(move |entry| {
                let Some(name) = entry.file_name().to_str() else {
                    return false;
                };
                if name.starts_with(".hbot-tmp-") {
                    return false;
                }
                !skip_names
                    .iter()
                    .any(|skip| name.eq_ignore_ascii_case(skip))
            });
        Ok(builder.build())
    }
}

/// 每批处理的候选文件数。
///
/// 批大小决定两件事：① 提前终止的粒度（一批跑完才看是否凑够 `max_results`）；
/// ② 并行片的大小（`ceil(批内文件数 / 线程数)`）。512 在 20 线程机器上约等于每线程
/// 26 个文件，批边界的 join 开销相对单文件 ~100 µs 的读盘可以忽略。
const SEARCH_BATCH_FILES: usize = 512;

/// 线程数硬上限：避免 `available_parallelism` 在异常环境里给出离谱的值。
const MAX_SEARCH_THREADS: usize = 64;

/// 一个候选文件：`label` 是回给调用方的相对路径，`real` 是要读的路径
/// （符号链接已解析到目标）。
struct Candidate {
    label: String,
    real: PathBuf,
}

/// 一个工作线程处理一片候选文件后的产出。
#[derive(Default)]
struct BatchPartial {
    hits: Vec<SearchHit>,
    scanned: u64,
    binary: u64,
    non_utf8: u64,
    unreadable: u64,
}

/// 解析线程数：`0` = 自动（[`std::thread::available_parallelism`]）。
fn resolve_threads(requested: usize) -> usize {
    if requested == 0 {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, MAX_SEARCH_THREADS)
    } else {
        requested.clamp(1, MAX_SEARCH_THREADS)
    }
}

/// 在**一个**工作线程里串行处理一片候选文件。
///
/// 每片至多收集 `hit_cap` 条命中：单线程内是精确上界，所有线程加起来仍然有界
/// （`线程数 × hit_cap`），不会在 10k 文件上无限收集。
fn scan_files(
    files: &[Candidate],
    matcher: &RegexMatcher,
    cancel: &AtomicBool,
    hit_cap: usize,
) -> BatchPartial {
    let mut out = BatchPartial::default();
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .multi_line(false)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    for candidate in files {
        if out.hits.len() >= hit_cap || cancel.load(Ordering::Relaxed) {
            break;
        }
        #[cfg(test)]
        crate::search::test_hooks::before_read(&candidate.real);
        let bytes = match fs::read(&candidate.real) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(path = %candidate.real.display(), error = %err, "skipping unreadable file");
                out.unreadable += 1;
                continue;
            }
        };
        if memchr::memchr(0, &bytes).is_some() {
            out.binary += 1;
            continue;
        }
        if std::str::from_utf8(&bytes).is_err() {
            out.non_utf8 += 1;
            continue;
        }
        let cap = hit_cap - out.hits.len();
        let label = &candidate.label;
        let mut produced: Vec<SearchHit> = Vec::new();
        let result = searcher.search_slice(
            matcher,
            &bytes,
            UTF8(|line_number, line| {
                if produced.len() >= cap || cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                produced.push(SearchHit {
                    path: label.clone(),
                    line_number,
                    line: line.trim_end_matches(['\r', '\n']).to_owned(),
                });
                Ok(true)
            }),
        );
        match result {
            Ok(()) => {
                out.scanned += 1;
                out.hits.append(&mut produced);
            }
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                out.non_utf8 += 1;
            }
            Err(err) => {
                tracing::debug!(path = %candidate.real.display(), error = %err, "search failed");
                out.unreadable += 1;
            }
        }
    }
    out
}

/// 测试钩子：只在 `cfg(test)` 下存在，用来证明「大文件先筛后读」与「读之前文件消失
/// 不会带崩搜索」。release 构建里没有这段代码。
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// 在每次 `fs::read` 之前被调用的闭包。
    pub(crate) type Hook = Box<dyn Fn(&Path) + Send + Sync + 'static>;

    static SLOT: Mutex<Option<Hook>> = Mutex::new(None);
    static SERIAL: Mutex<()> = Mutex::new(());

    /// 装钩子的作用域守卫：同 crate 里多个用钩子的测试被 `SERIAL` 串行化，
    /// 守卫释放时自动卸载。
    pub(crate) struct HookGuard {
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            *lock(&SLOT) = None;
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        // 钩子内部不 panic；真被毒化了也照旧取回数据，测试才有机会断言。
        mutex.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// 安装钩子，返回的守卫活到作用域结束。
    pub(crate) fn install(hook: Hook) -> HookGuard {
        let serial = lock(&SERIAL);
        *lock(&SLOT) = Some(hook);
        HookGuard { _serial: serial }
    }

    /// 每次 `fs::read` 之前调用。
    pub(crate) fn before_read(path: &Path) {
        // 只在锁内取一次引用：钩子不会重入 `before_read`（它只会删文件）。
        let slot = lock(&SLOT);
        if let Some(hook) = slot.as_ref() {
            hook(path);
        }
    }
}

fn build_matcher(query: &str, options: &SearchOptions) -> Result<RegexMatcher, HbotError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(!options.regex)
        .case_insensitive(!options.case_sensitive);
    builder
        .build(query)
        .map_err(|err| HbotError::invalid(format!("invalid search pattern: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn run(
        workspace: &Workspace,
        query: &str,
        options: &SearchOptions,
    ) -> (Vec<SearchHit>, SearchSummary) {
        let mut hits = Vec::new();
        let summary = workspace
            .search(query, options, Arc::new(AtomicBool::new(false)), |hit| {
                hits.push(hit)
            })
            .expect("search");
        (hits, summary)
    }

    fn paths(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.path.as_str()).collect()
    }

    #[test]
    fn search_finds_literal_matches_with_line_numbers() {
        let temp = TestDir::new("search-literal");
        let workspace = temp.workspace();
        temp.write("a.txt", "alpha\nneedle here\nomega\n");
        temp.write("b.txt", "nothing\n");
        let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.txt");
        assert_eq!(hits[0].line_number, 2);
        assert_eq!(hits[0].line, "needle here");
        assert_eq!(summary.hits, 1);
        assert_eq!(summary.files_scanned, 2);
        assert!(!summary.cancelled && !summary.limit_reached);
    }

    #[test]
    fn search_respects_gitignore() {
        let temp = TestDir::new("search-gitignore");
        let workspace = temp.workspace();
        temp.write(".gitignore", "ignored.txt\ndist/\n");
        temp.write("kept.txt", "needle\n");
        temp.write("ignored.txt", "needle\n");
        temp.write("dist/bundle.js", "needle\n");
        let (hits, _) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["kept.txt"]);
    }

    #[test]
    fn search_regex_and_case_sensitivity() {
        let temp = TestDir::new("search-regex");
        let workspace = temp.workspace();
        temp.write("a.ts", "export function main() {}\nconst HELLO = 1;\n");

        let mut options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let (hits, _) = run(&workspace, r"function\s+main", &options);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_number, 1);

        options.regex = false;
        let (hits, _) = run(&workspace, r"function\s+main", &options);
        assert!(hits.is_empty(), "literal search must not interpret `\\s`");

        let case_insensitive = SearchOptions {
            case_sensitive: false,
            ..SearchOptions::default()
        };
        let (hits, _) = run(&workspace, "hello", &case_insensitive);
        assert_eq!(hits.len(), 1);

        let case_sensitive = SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        };
        let (hits, _) = run(&workspace, "hello", &case_sensitive);
        assert!(
            hits.is_empty(),
            "case-sensitive search must not match HELLO"
        );
    }

    #[test]
    fn search_globs_include_and_exclude() {
        let temp = TestDir::new("search-globs");
        let workspace = temp.workspace();
        temp.write("a.ts", "needle\n");
        temp.write("b.rs", "needle\n");
        temp.write("vendor/c.ts", "needle\n");

        let include_ts = SearchOptions {
            globs: vec!["*.ts".to_owned()],
            ..SearchOptions::default()
        };
        let (hits, _) = run(&workspace, "needle", &include_ts);
        let mut found = paths(&hits);
        found.sort_unstable();
        assert_eq!(found, vec!["a.ts", "vendor/c.ts"]);

        let exclude_vendor = SearchOptions {
            globs: vec!["*.ts".to_owned(), "!vendor/**".to_owned()],
            ..SearchOptions::default()
        };
        let (hits, _) = run(&workspace, "needle", &exclude_vendor);
        assert_eq!(paths(&hits), vec!["a.ts"]);
    }

    #[test]
    fn search_skips_binary_and_counts_it() {
        let temp = TestDir::new("search-binary");
        let workspace = temp.workspace();
        temp.write("text.txt", "needle\n");
        let binary: PathBuf = temp.path().join("blob.bin");
        fs::write(&binary, b"needle\0needle").expect("write binary");
        let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["text.txt"]);
        assert_eq!(summary.files_skipped_binary, 1);
        assert_eq!(summary.files_scanned, 1);
    }

    #[test]
    fn search_skips_non_utf8_and_counts_it() {
        let temp = TestDir::new("search-non-utf8");
        let workspace = temp.workspace();
        fs::write(
            temp.path().join("latin1.txt"),
            [0x6E, 0x65, 0x65, 0xFF, 0x0A],
        )
        .expect("write latin1");
        let (hits, summary) = run(&workspace, "nee", &SearchOptions::default());
        assert!(hits.is_empty());
        assert_eq!(summary.files_skipped_non_utf8, 1);
        assert_eq!(summary.files_scanned, 0);
    }

    #[test]
    fn search_skips_files_over_size_limit() {
        let temp = TestDir::new("search-size");
        let workspace = temp.workspace();
        temp.write("big.txt", "needle\nneedle\nneedle\n");
        let options = SearchOptions {
            max_file_size: 4,
            ..SearchOptions::default()
        };
        let (hits, summary) = run(&workspace, "needle", &options);
        assert!(hits.is_empty());
        assert_eq!(summary.files_skipped_too_large, 1);
    }

    #[test]
    fn search_skips_hidden_and_hbot_directories() {
        let temp = TestDir::new("search-hidden");
        let workspace = temp.workspace();
        temp.write("visible.txt", "needle\n");
        temp.write(".hidden.txt", "needle\n");
        temp.write(".hbot/should-not-appear.txt", "needle\n");
        temp.write(".git/config", "needle\n");
        let (hits, _) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["visible.txt"]);
    }

    #[test]
    fn search_stops_when_cancelled_before_start() {
        let temp = TestDir::new("search-cancel-pre");
        let workspace = temp.workspace();
        temp.write("a.txt", "needle\n");
        let mut hits = Vec::new();
        let summary = workspace
            .search(
                "needle",
                &SearchOptions::default(),
                Arc::new(AtomicBool::new(true)),
                |hit| hits.push(hit),
            )
            .expect("search");
        assert!(hits.is_empty());
        assert!(summary.cancelled);
    }

    #[test]
    fn search_stops_early_when_cancelled_from_sink() {
        let temp = TestDir::new("search-cancel-mid");
        let workspace = temp.workspace();
        for index in 0..20 {
            temp.write(&format!("f{index:02}.txt"), "needle\n");
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut hits = Vec::new();
        let flag = Arc::clone(&cancel);
        let summary = workspace
            .search(
                "needle",
                &SearchOptions::default(),
                Arc::clone(&cancel),
                |hit| {
                    hits.push(hit);
                    flag.store(true, Ordering::Relaxed);
                },
            )
            .expect("search");
        assert!(summary.cancelled, "summary must report cancellation");
        assert!(
            hits.len() < 20,
            "cancellation must stop before scanning every file, got {}",
            hits.len()
        );
    }

    #[test]
    fn search_honours_max_results() {
        let temp = TestDir::new("search-limit");
        let workspace = temp.workspace();
        temp.write("a.txt", "needle\nneedle\nneedle\n");
        let options = SearchOptions {
            max_results: 2,
            ..SearchOptions::default()
        };
        let (hits, summary) = run(&workspace, "needle", &options);
        assert_eq!(hits.len(), 2);
        assert_eq!(summary.hits, 2);
        assert!(summary.limit_reached);
    }

    #[test]
    fn search_rejects_bad_input() {
        let temp = TestDir::new("search-bad");
        let workspace = temp.workspace();
        let err = workspace
            .search(
                "",
                &SearchOptions::default(),
                Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .expect_err("empty query");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");

        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let err = workspace
            .search(
                "(unclosed",
                &options,
                Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .expect_err("bad regex");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");

        let options = SearchOptions {
            globs: vec!["[".to_owned()],
            ..SearchOptions::default()
        };
        let err = workspace
            .search("x", &options, Arc::new(AtomicBool::new(false)), |_| {})
            .expect_err("bad glob");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");

        let options = SearchOptions {
            max_results: 0,
            ..SearchOptions::default()
        };
        let err = workspace
            .search("x", &options, Arc::new(AtomicBool::new(false)), |_| {})
            .expect_err("zero max_results");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn search_skips_symlink_escaping_root() {
        let temp = TestDir::new("search-symlink");
        let workspace = temp.workspace();
        let outside = TestDir::new("search-symlink-outside");
        outside.write("secret.txt", "needle\n");
        temp.write("inside.txt", "needle\n");

        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(
            outside.path().join("secret.txt"),
            temp.path().join("leak.txt"),
        );
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            temp.path().join("leak.txt"),
        );
        #[cfg(not(any(windows, unix)))]
        let created: std::io::Result<()> = Err(std::io::Error::other("unsupported platform"));

        if created.is_err() {
            eprintln!("skipping symlink search test: cannot create symlink (needs privileges)");
            return;
        }

        let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["inside.txt"]);
        assert_eq!(summary.files_skipped_outside, 1);
    }

    #[test]
    fn search_does_not_follow_symlinked_directories() {
        let temp = TestDir::new("search-symlink-dir");
        let workspace = temp.workspace();
        temp.write("real/target.txt", "needle\n");

        #[cfg(windows)]
        let created =
            std::os::windows::fs::symlink_dir(temp.path().join("real"), temp.path().join("alias"));
        #[cfg(unix)]
        let created =
            std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("alias"));
        #[cfg(not(any(windows, unix)))]
        let created: std::io::Result<()> = Err(std::io::Error::other("unsupported platform"));

        if created.is_err() {
            eprintln!("skipping symlink dir test: cannot create symlink (needs privileges)");
            return;
        }

        let (hits, _) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(
            paths(&hits),
            vec!["real/target.txt"],
            "symlinked directory must not be traversed twice"
        );
    }

    #[test]
    fn search_skips_directories_named_like_files() {
        let temp = TestDir::new("search-dir-entry");
        let workspace = temp.workspace();
        temp.write("ok.txt", "needle\n");
        let target = temp.path().join("locked.txt");
        fs::write(&target, "needle\n").expect("write");

        // 同名目录：不是文件，既不算扫描也不算“跳过（错误）”。
        fs::remove_file(&target).expect("remove");
        fs::create_dir(&target).expect("create dir");

        let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["ok.txt"]);
        assert_eq!(summary.files_skipped_unreadable, 0);
        assert_eq!(summary.files_scanned, 1);
    }

    // ---- 并行化相关的新测试 ------------------------------------------------

    /// 显式指定线程数跑一次。
    fn run_with_threads(
        workspace: &Workspace,
        query: &str,
        options: &SearchOptions,
        threads: usize,
    ) -> (Vec<SearchHit>, SearchSummary) {
        let mut hits = Vec::new();
        let summary = workspace
            .search_with_threads(
                query,
                options,
                Arc::new(AtomicBool::new(false)),
                threads,
                |hit| hits.push(hit),
            )
            .expect("search");
        (hits, summary)
    }

    /// 造 `files` 个文件、每个 `lines` 行 `needle`，分散在 5 个子目录里。
    fn write_needle_tree(temp: &TestDir, files: usize, lines: usize) {
        for index in 0..files {
            let mut body = String::new();
            for line in 0..lines {
                body.push_str(&format!("needle {index} {line}\n"));
            }
            temp.write(&format!("d{}/f{index:03}.txt", index % 5), &body);
        }
    }

    /// 同一目录跑 5 次，`(path, line)` 序列必须完全一致，且必须是升序 ——
    /// 并行化最容易破的就是这一条（按完成顺序回调会让顺序随调度抖动）。
    #[test]
    fn results_are_deterministically_ordered() {
        let temp = TestDir::new("search-deterministic");
        let workspace = temp.workspace();
        write_needle_tree(&temp, 30, 3);

        let mut sequences: Vec<Vec<(String, u64)>> = Vec::new();
        for _ in 0..5 {
            let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
            assert_eq!(summary.hits, 90);
            sequences.push(
                hits.into_iter()
                    .map(|hit| (hit.path, hit.line_number))
                    .collect(),
            );
        }
        for (index, sequence) in sequences.iter().enumerate().skip(1) {
            assert_eq!(
                sequence,
                &sequences[0],
                "第 {} 次的命中顺序与第 1 次不同（并行版必须确定性排序）",
                index + 1
            );
        }
        assert_eq!(sequences[0].len(), 90);
        let mut sorted = sequences[0].clone();
        sorted.sort();
        assert_eq!(sequences[0], sorted, "命中必须按 (path, line) 升序");
    }

    /// 500 条命中、`max_results = 10`：正好 10 条，且必须是“全量排序后的前 10 条”，
    /// 不是随机的 10 条。
    #[test]
    fn max_results_is_respected_and_bounded() {
        let temp = TestDir::new("search-limit-bounded");
        let workspace = temp.workspace();
        write_needle_tree(&temp, 50, 10);

        let (all, all_summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(all.len(), 500);
        assert_eq!(all_summary.hits, 500);
        assert!(!all_summary.limit_reached);

        let options = SearchOptions {
            max_results: 10,
            ..SearchOptions::default()
        };
        for _ in 0..3 {
            let (hits, summary) = run(&workspace, "needle", &options);
            assert_eq!(hits.len(), 10, "必须正好 10 条");
            assert_eq!(summary.hits, 10);
            assert!(
                summary.limit_reached,
                "凑够 max_results 必须置 limit_reached"
            );
            assert_eq!(
                hits,
                all[..10].to_vec(),
                "截断必须发生在排序之后（前 10 条 = 全量排序的前 10 条）"
            );
        }

        // 单个文件里就超过上限：每文件的命中收集也要有界。
        let single = TestDir::new("search-limit-single-file");
        let single_workspace = single.workspace();
        let mut body = String::new();
        for index in 0..500 {
            body.push_str(&format!("needle {index}\n"));
        }
        single.write("all.txt", &body);
        let (hits, summary) = run(&single_workspace, "needle", &options);
        assert_eq!(hits.len(), 10);
        assert_eq!(summary.hits, 10);
        assert!(summary.limit_reached);
        assert_eq!(hits[0].line_number, 1);
        assert_eq!(hits[9].line_number, 10);
    }

    /// 1 线程与 8 线程的命中集合（含顺序与统计）必须完全相同 —— 这是防“并行版漏文件”
    /// 的关键一条。
    #[test]
    fn parallel_search_finds_the_same_hits_as_serial() {
        let temp = TestDir::new("search-parallel-vs-serial");
        let workspace = temp.workspace();
        write_needle_tree(&temp, 40, 4);
        temp.write("plain.txt", "nothing here\n");
        fs::write(temp.path().join("blob.bin"), b"needle\0needle").expect("write binary");

        let serial = run_with_threads(&workspace, "needle", &SearchOptions::default(), 1);
        let parallel = run_with_threads(&workspace, "needle", &SearchOptions::default(), 8);

        assert_eq!(serial.0.len(), 160);
        assert_eq!(serial.0, parallel.0, "1 线程与 8 线程的命中必须逐条相同");
        assert_eq!(serial.1, parallel.1, "统计也必须相同");
    }

    /// 候选文件在“遍历之后、读之前”消失：跳过它、不 panic、不影响其它文件。
    #[test]
    fn a_file_that_disappears_mid_search_is_skipped() {
        let temp = TestDir::new("search-vanish");
        let workspace = temp.workspace();
        temp.write("a-kept.txt", "needle\n");
        temp.write("b-vanish.txt", "needle\n");
        temp.write("c-kept.txt", "needle\n");

        // 钩子在每次 `fs::read` 之前触发：第一次读到消失文件时把它删掉，
        // 于是这次读取失败 —— 正是“搜索过程中文件被删”。
        let _hook = test_hooks::install(Box::new(|path: &std::path::Path| {
            if path.file_name().is_some_and(|name| name == "b-vanish.txt") {
                let _ = fs::remove_file(path);
            }
        }));

        let (hits, summary) = run(&workspace, "needle", &SearchOptions::default());
        assert_eq!(paths(&hits), vec!["a-kept.txt", "c-kept.txt"]);
        assert_eq!(
            summary.files_skipped_unreadable, 1,
            "消失的文件计入 unreadable"
        );
        assert_eq!(summary.files_scanned, 2);
        assert!(!summary.cancelled, "单个文件失败不许中断整个搜索");
    }

    /// 超过 `max_file_size` 的文件必须在**读之前**被大小筛掉：钩子记录所有被读的路径，
    /// 大文件一个字节都不该读。
    #[test]
    fn large_files_are_skipped_before_reading() {
        let temp = TestDir::new("search-large-not-read");
        let workspace = temp.workspace();
        temp.write("small.txt", "needle\n");
        let big = format!("SENTINEL_LARGE\n{}", "needle\n".repeat(200));
        temp.write("big.txt", &big);

        let read_paths: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&read_paths);
        let _hook = test_hooks::install(Box::new(move |path: &std::path::Path| {
            recorder
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(path.to_path_buf());
        }));

        let options = SearchOptions {
            max_file_size: 1024,
            ..SearchOptions::default()
        };
        let (hits, summary) = run(&workspace, "SENTINEL_LARGE", &options);

        assert!(hits.is_empty(), "大文件里的哨兵不该出现在结果里");
        assert_eq!(summary.files_skipped_too_large, 1);
        assert_eq!(summary.files_skipped_unreadable, 0);

        let read_paths = read_paths
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        assert!(
            !read_paths.iter().any(|path| path.ends_with("big.txt")),
            "big.txt 超过 max_file_size，必须在读之前筛掉；实际读过：{read_paths:?}"
        );
        assert!(
            read_paths.iter().any(|path| path.ends_with("small.txt")),
            "小文件应当被读过：{read_paths:?}"
        );
    }
}
