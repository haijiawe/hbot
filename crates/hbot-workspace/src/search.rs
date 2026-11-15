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

use std::fs;
use std::io;
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
    /// `cancel` 置位后在**下一个命中或下一个文件**处停止，返回的
    /// [`SearchSummary::cancelled`] 为 `true`（已产出的命中不撤回）。
    pub fn search<F>(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Arc<AtomicBool>,
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

        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .multi_line(false)
            .binary_detection(BinaryDetection::quit(0))
            .build();
        let walker = self.build_walker(options)?;

        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            if summary.hits >= options.max_results {
                summary.limit_reached = true;
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
            let real = if file_type.is_symlink() {
                let Ok(real) = fs::canonicalize(path) else {
                    summary.files_skipped_unreadable += 1;
                    continue;
                };
                if !self.is_within(&real) {
                    tracing::debug!(path = %path.display(), "skipping symlink escaping workspace root");
                    summary.files_skipped_outside += 1;
                    continue;
                }
                real
            } else {
                path.to_path_buf()
            };
            let meta = match fs::metadata(&real) {
                Ok(meta) => meta,
                Err(err) => {
                    tracing::debug!(path = %real.display(), error = %err, "skipping file without metadata");
                    summary.files_skipped_unreadable += 1;
                    continue;
                }
            };
            if !meta.is_file() {
                // FIFO/设备等：不是文本文件，不算跳过也不算扫描。
                continue;
            }
            if meta.len() > file_limit {
                summary.files_skipped_too_large += 1;
                continue;
            }
            let bytes = match fs::read(&real) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::debug!(path = %real.display(), error = %err, "skipping unreadable file");
                    summary.files_skipped_unreadable += 1;
                    continue;
                }
            };
            if memchr::memchr(0, &bytes).is_some() {
                summary.files_skipped_binary += 1;
                continue;
            }
            if std::str::from_utf8(&bytes).is_err() {
                summary.files_skipped_non_utf8 += 1;
                continue;
            }
            let Ok(label) = self.relative_path(path) else {
                summary.files_skipped_outside += 1;
                continue;
            };

            let remaining = options.max_results - summary.hits;
            let mut produced = 0usize;
            let result = searcher.search_slice(
                &matcher,
                &bytes,
                UTF8(|line_number, line| {
                    if produced >= remaining || cancel.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    produced += 1;
                    sink(SearchHit {
                        path: label.clone(),
                        line_number,
                        line: line.trim_end_matches(['\r', '\n']).to_owned(),
                    });
                    Ok(true)
                }),
            );
            match result {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                    summary.files_skipped_non_utf8 += 1;
                    continue;
                }
                Err(err) => {
                    tracing::debug!(path = %real.display(), error = %err, "search failed");
                    summary.files_skipped_unreadable += 1;
                    continue;
                }
            }
            summary.files_scanned += 1;
            summary.hits += produced;
            if summary.hits >= options.max_results {
                summary.limit_reached = true;
                break;
            }
        }
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
}
