//! 文件读写、目录操作与回收站。
//!
//! 所有写盘都是 **同目录 tmp → 写入 → fsync → rename 覆盖**（`AGENTS.md` §7 第 14 条：
//! Windows 上目标被占用时 rename 会 EPERM，因此有界重试，失败则清理 tmp）。
//! 本模块不做路径解析：入口一律先经 [`Workspace`] 的守卫。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hbot_core::HbotError;

use crate::project::{MAX_READ_BYTES, Workspace, io_error};

/// rename 的重试次数（Windows 上杀软/编辑器短暂占用会让首次失败）。
const RENAME_ATTEMPTS: u32 = 5;

/// 一次读取的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    /// UTF-8 文本内容。
    pub content: String,
    /// 内容的 blake3 十六进制哈希（乐观并发的版本标识）。
    pub hash: String,
    /// 字节数。
    pub size: u64,
}

/// 目录条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// 条目名（不含父目录）。
    pub name: String,
    /// 相对项目根的路径，`/` 分隔。
    pub path: String,
    /// 是否目录（符号链接解析到根内后判断）。
    pub is_dir: bool,
    /// 文件字节数；目录为 0。
    pub size: u64,
}

/// 计算 blake3 十六进制哈希。
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

impl Workspace {
    /// 读取 UTF-8 文本文件。
    ///
    /// `max_bytes` 与硬上限 [`MAX_READ_BYTES`]（2 MiB）取较小值；超过返回
    /// [`HbotError::Invalid`]（UI 显示“只读，用外部工具打开”）。非 UTF-8 同样返回
    /// `Invalid`，不静默替换字符。
    pub fn read_file(&self, rel: &Path, max_bytes: u64) -> Result<FileContent, HbotError> {
        let limit = max_bytes.min(MAX_READ_BYTES);
        let path = self.resolve_existing(rel)?;
        let meta = fs::metadata(&path).map_err(|err| io_error("file", &path, err))?;
        if meta.is_dir() {
            return Err(HbotError::invalid(format!(
                "`{}` is a directory, not a file",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(HbotError::invalid(format!(
                "`{}` is not a regular file",
                path.display()
            )));
        }
        if meta.len() > limit {
            return Err(HbotError::invalid(format!(
                "file `{}` is {} bytes, which exceeds the read limit of {limit} bytes",
                path.display(),
                meta.len()
            )));
        }
        let bytes = fs::read(&path).map_err(|err| io_error("file", &path, err))?;
        if bytes.len() as u64 > limit {
            return Err(HbotError::invalid(format!(
                "file `{}` grew to {} bytes while reading, which exceeds the read limit of {limit} bytes",
                path.display(),
                bytes.len()
            )));
        }
        let size = bytes.len() as u64;
        let hash = hash_bytes(&bytes);
        let content = String::from_utf8(bytes).map_err(|_| {
            HbotError::invalid(format!(
                "file `{}` is not valid UTF-8; open it with an external tool",
                path.display()
            ))
        })?;
        Ok(FileContent {
            content,
            hash,
            size,
        })
    }

    /// 原子写入 UTF-8 文本。
    ///
    /// 给了 `expected_hash` 时做乐观并发校验：当前内容哈希不匹配返回
    /// [`HbotError::Conflict`]（文件不存在也算冲突）。父目录不存在返回 `Invalid`，
    /// 不自动建目录（建目录用 [`Workspace::create_dir`]）。
    pub fn write_file(
        &self,
        rel: &Path,
        content: &str,
        expected_hash: Option<&str>,
    ) -> Result<FileContent, HbotError> {
        // 父目录必须已存在：不存在是调用方的错误，不是“帮你建一下”。
        let path = self.resolve_new(rel).map_err(|err| match err {
            HbotError::NotFound { .. } => HbotError::invalid(format!(
                "parent directory of `{}` does not exist; create it first",
                rel.display()
            )),
            other => other,
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| HbotError::invalid("path has no parent directory"))?
            .to_path_buf();
        if !parent.is_dir() {
            return Err(HbotError::invalid(format!(
                "parent directory `{}` does not exist; create it first",
                parent.display()
            )));
        }
        if path.is_dir() {
            return Err(HbotError::invalid(format!(
                "`{}` is a directory",
                path.display()
            )));
        }

        let _guard = self.lock_writes()?;
        if let Some(expected) = expected_hash {
            let current = match fs::read(&path) {
                Ok(bytes) => Some(hash_bytes(&bytes)),
                Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                Err(err) => return Err(io_error("file", &path, err)),
            };
            match current {
                Some(actual) if actual.eq_ignore_ascii_case(expected) => {}
                Some(actual) => {
                    return Err(HbotError::conflict(format!(
                        "file `{}` changed on disk (expected hash {expected}, found {actual})",
                        path.display()
                    )));
                }
                None => {
                    return Err(HbotError::conflict(format!(
                        "file `{}` does not exist, but hash {expected} was supplied",
                        path.display()
                    )));
                }
            }
        }

        let tmp = parent.join(format!(".hbot-tmp-{}", uuid::Uuid::new_v4()));
        write_atomic(&tmp, &path, content.as_bytes())?;
        Ok(FileContent {
            content: content.to_owned(),
            hash: hash_bytes(content.as_bytes()),
            size: content.len() as u64,
        })
    }

    /// 列出目录条目：目录在前，再按名字排序；跳过策略里配置的名字（默认 `.hbot`）、
    /// `.git/`、临时文件与逃逸的符号链接。不可读/非 UTF-8 的条目跳过并记 `tracing`。
    pub fn list_dir(&self, rel: &Path) -> Result<Vec<DirEntryInfo>, HbotError> {
        let dir = self.resolve_existing(rel)?;
        if !dir.is_dir() {
            return Err(HbotError::invalid(format!(
                "`{}` is not a directory",
                dir.display()
            )));
        }
        let prefix = self.relative_path(&dir)?;
        let mut entries = Vec::new();
        let reading = fs::read_dir(&dir).map_err(|err| io_error("directory", &dir, err))?;
        for item in reading {
            let entry = match item {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(path = %dir.display(), error = %err, "skipping unreadable directory entry");
                    continue;
                }
            };
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    tracing::debug!(path = %dir.display(), "skipping non-UTF-8 directory entry name");
                    continue;
                }
            };
            if self.is_skipped_name(&name) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    tracing::debug!(path = %entry.path().display(), error = %err, "skipping entry with unknown type");
                    continue;
                }
            };
            let (is_dir, size) = if file_type.is_symlink() {
                let real = match fs::canonicalize(entry.path()) {
                    Ok(real) => real,
                    Err(err) => {
                        tracing::debug!(path = %entry.path().display(), error = %err, "skipping dangling symlink");
                        continue;
                    }
                };
                if !self.is_within(&real) {
                    tracing::debug!(path = %entry.path().display(), "skipping symlink escaping workspace root");
                    continue;
                }
                match fs::metadata(&real) {
                    Ok(meta) => (meta.is_dir(), if meta.is_dir() { 0 } else { meta.len() }),
                    Err(err) => {
                        tracing::debug!(path = %real.display(), error = %err, "skipping unreadable symlink target");
                        continue;
                    }
                }
            } else {
                match entry.metadata() {
                    Ok(meta) => (meta.is_dir(), if meta.is_dir() { 0 } else { meta.len() }),
                    Err(err) => {
                        tracing::debug!(path = %entry.path().display(), error = %err, "skipping entry without metadata");
                        continue;
                    }
                }
            };
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            entries.push(DirEntryInfo {
                name,
                path,
                is_dir,
                size,
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    /// 建目录（`create_dir_all` 语义）；返回根内规范化绝对路径。
    pub fn create_dir(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let path = self.resolve_new_dir(rel)?;
        fs::create_dir_all(&path).map_err(|err| io_error("directory", &path, err))?;
        let canonical = fs::canonicalize(&path).map_err(|err| io_error("directory", &path, err))?;
        if !self.is_within(&canonical) {
            return Err(HbotError::invalid(format!(
                "directory `{}` escaped the workspace root",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// 重命名/移动（同盘）。目标已存在返回 `Conflict`，不静默覆盖。
    ///
    /// 移动的是**条目本身**：`from` 是符号链接时移动链接，不跟随（见
    /// [`Workspace::resolve_entry`]）。
    pub fn rename(&self, from: &Path, to: &Path) -> Result<(), HbotError> {
        let source = self.resolve_entry(from)?;
        if source == self.root() {
            return Err(HbotError::invalid("cannot rename the workspace root"));
        }
        let destination = self.resolve_new(to)?;
        if source == destination {
            return Ok(());
        }
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(HbotError::conflict(format!(
                    "destination `{}` already exists",
                    destination.display()
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error("path", &destination, err)),
        }

        let _guard = self.lock_writes()?;
        rename_with_retry(&source, &destination).map_err(|err| {
            if is_cross_device(&err) {
                HbotError::invalid(format!(
                    "cannot rename `{}` across devices; copy it into the workspace first",
                    source.display()
                ))
            } else {
                io_error("rename", &destination, err)
            }
        })?;
        sync_parent_dir(&destination);
        Ok(())
    }

    /// 移动到 `<root>/.hbot/trash/<timestamp>-<name>`（**不真删**）。
    ///
    /// 同盘用 rename；跨盘退回“复制整棵树 + 删除源”。返回回收站里的绝对路径。
    /// 移入的是**条目本身**：符号链接只移动链接，不动它指向的文件。
    pub fn delete_to_trash(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let source = self.resolve_entry(rel)?;
        if source == self.root() {
            return Err(HbotError::invalid(
                "cannot move the workspace root to trash",
            ));
        }
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| HbotError::invalid("path has no file name"))?
            .to_owned();

        let _guard = self.lock_writes()?;
        let trash = self.root().join(".hbot").join("trash");
        fs::create_dir_all(&trash).map_err(|err| io_error("trash directory", &trash, err))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let destination = unique_trash_path(&trash, stamp, &name)?;

        match rename_with_retry(&source, &destination) {
            Ok(()) => {
                sync_parent_dir(&destination);
                Ok(destination)
            }
            Err(err) if is_cross_device(&err) => {
                copy_tree_then_remove(&source, &destination)
                    .map_err(|err| io_error("move to trash", &destination, err))?;
                Ok(destination)
            }
            Err(err) => Err(io_error("move to trash", &destination, err)),
        }
    }
}

/// 同目录 tmp → 写入 → fsync → rename 覆盖；失败清理 tmp。
fn write_atomic(tmp: &Path, target: &Path, bytes: &[u8]) -> Result<(), HbotError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|err| io_error("temporary file", tmp, err))?;
    let written = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(err) = written {
        cleanup_tmp(tmp);
        return Err(io_error("write temporary file", tmp, err));
    }
    match rename_with_retry(tmp, target) {
        Ok(()) => {
            sync_parent_dir(target);
            Ok(())
        }
        Err(err) => {
            cleanup_tmp(tmp);
            Err(io_error("replace file", target, err))
        }
    }
}

fn cleanup_tmp(tmp: &Path) {
    if let Err(err) = fs::remove_file(tmp) {
        tracing::debug!(path = %tmp.display(), error = %err, "failed to remove temporary file");
    }
}

/// rename 的有界重试：Windows 上目标被占用会短暂失败（EPERM / 共享冲突）。
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last_error = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(err) if is_retryable(&err) && attempt + 1 < RENAME_ATTEMPTS => {
                last_error = Some(err);
                std::thread::sleep(Duration::from_millis(20 * u64::from(attempt + 1)));
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("rename failed without an error")))
}

fn is_retryable(err: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED=5 / ERROR_SHARING_VIOLATION=32 / ERROR_LOCK_VIOLATION=33
    matches!(err.raw_os_error(), Some(5) | Some(32) | Some(33))
        || matches!(
            err.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
        )
}

fn is_cross_device(err: &io::Error) -> bool {
    // Windows ERROR_NOT_SAME_DEVICE=17，Unix EXDEV=18
    matches!(err.raw_os_error(), Some(17) | Some(18))
}

fn unique_trash_path(trash: &Path, stamp: u128, name: &str) -> Result<PathBuf, HbotError> {
    for attempt in 0..1000u32 {
        let candidate = if attempt == 0 {
            trash.join(format!("{stamp}-{name}"))
        } else {
            trash.join(format!("{stamp}-{name}-{attempt}"))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(HbotError::internal(
        "could not allocate a unique trash path",
    ))
}

/// 跨盘兜底：整树复制后删除源。符号链接不跟随，直接报错（不猜语义）。
fn copy_tree_then_remove(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other("cannot move a symlink across devices"));
    }
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree_then_remove(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::remove_dir(from)
    } else {
        fs::copy(from, to)?;
        fs::remove_file(from)
    }
}

/// rename 的持久化依赖父目录 fsync；Windows 不支持目录 fsync，跳过。
#[cfg(unix)]
fn sync_parent_dir(target: &Path) {
    let Some(parent) = target.parent() else {
        return;
    };
    match std::fs::File::open(parent) {
        Ok(dir) => {
            if let Err(err) = dir.sync_all() {
                tracing::debug!(path = %parent.display(), error = %err, "directory fsync failed");
            }
        }
        Err(err) => {
            tracing::debug!(path = %parent.display(), error = %err, "cannot open directory for fsync");
        }
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_target: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::path::PathBuf;

    fn leftover_tmp_files(dir: &Path) -> Vec<PathBuf> {
        let mut leftovers = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".hbot-tmp-") {
                    leftovers.push(entry.path());
                }
            }
        }
        leftovers
    }

    #[test]
    fn read_file_reports_hash_and_content() {
        let temp = TestDir::new("read");
        let workspace = temp.workspace();
        temp.write("hello.txt", "你好\nworld");
        let read = workspace
            .read_file(Path::new("hello.txt"), MAX_READ_BYTES)
            .expect("read");
        assert_eq!(read.content, "你好\nworld");
        assert_eq!(read.size, "你好\nworld".len() as u64);
        assert_eq!(read.hash, hash_bytes("你好\nworld".as_bytes()));
        assert_eq!(read.hash.len(), 64);
    }

    #[test]
    fn read_file_rejects_missing_and_directory() {
        let temp = TestDir::new("read-errors");
        let workspace = temp.workspace();
        temp.write("dir/keep.txt", "x");
        let missing = workspace
            .read_file(Path::new("nope.txt"), MAX_READ_BYTES)
            .expect_err("missing file");
        assert!(
            matches!(missing, HbotError::NotFound { .. }),
            "got {missing:?}"
        );
        let directory = workspace
            .read_file(Path::new("dir"), MAX_READ_BYTES)
            .expect_err("directory");
        assert!(
            matches!(directory, HbotError::Invalid { .. }),
            "got {directory:?}"
        );
    }

    #[test]
    fn read_file_enforces_two_mib_limit() {
        let temp = TestDir::new("read-limit");
        let workspace = temp.workspace();
        let big = vec![b'a'; (MAX_READ_BYTES + 1) as usize];
        fs::write(temp.path().join("big.txt"), &big).unwrap();
        let err = workspace
            .read_file(Path::new("big.txt"), u64::MAX)
            .expect_err("must exceed hard limit");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");

        let exactly = vec![b'a'; MAX_READ_BYTES as usize];
        fs::write(temp.path().join("exact.txt"), &exactly).unwrap();
        assert!(
            workspace
                .read_file(Path::new("exact.txt"), MAX_READ_BYTES)
                .is_ok(),
            "exactly 2 MiB must be readable"
        );

        let err = workspace
            .read_file(Path::new("exact.txt"), 10)
            .expect_err("caller limit must win");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn read_file_rejects_non_utf8() {
        let temp = TestDir::new("read-non-utf8");
        let workspace = temp.workspace();
        fs::write(temp.path().join("latin1.txt"), [0x66, 0xFF, 0x6F]).unwrap();
        let err = workspace
            .read_file(Path::new("latin1.txt"), MAX_READ_BYTES)
            .expect_err("non-UTF-8 must be rejected");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn write_file_rejects_escaping_path_and_missing_parent() {
        let temp = TestDir::new("write-guard");
        let workspace = temp.workspace();
        let escape = workspace
            .write_file(Path::new("../escape.txt"), "x", None)
            .expect_err("escape must fail");
        assert!(
            matches!(escape, HbotError::Invalid { .. }),
            "got {escape:?}"
        );
        assert!(!temp.path().parent().unwrap().join("escape.txt").exists());

        let missing_parent = workspace
            .write_file(Path::new("no/such/dir.txt"), "x", None)
            .expect_err("missing parent must fail");
        assert!(
            matches!(missing_parent, HbotError::Invalid { .. }),
            "got {missing_parent:?}"
        );
        assert!(leftover_tmp_files(temp.path()).is_empty());
    }

    #[test]
    fn write_file_then_read_roundtrip() {
        let temp = TestDir::new("write-roundtrip");
        let workspace = temp.workspace();
        let written = workspace.write_file(Path::new("src/main.ts"), "export default 1;\n", None);
        assert!(written.is_err(), "parent must not be auto-created");
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        let written = workspace
            .write_file(Path::new("src/main.ts"), "export default 1;\n", None)
            .expect("write");
        let read = workspace
            .read_file(Path::new("src/main.ts"), MAX_READ_BYTES)
            .expect("read");
        assert_eq!(read.content, written.content);
        assert_eq!(read.hash, written.hash);
        assert!(leftover_tmp_files(temp.path().join("src").as_path()).is_empty());
    }

    #[test]
    fn write_file_optimistic_concurrency() {
        let temp = TestDir::new("write-hash");
        let workspace = temp.workspace();
        let first = workspace
            .write_file(Path::new("a.txt"), "one", None)
            .expect("first write");

        let conflict = workspace
            .write_file(Path::new("a.txt"), "two", Some("deadbeef"))
            .expect_err("wrong hash must conflict");
        assert!(
            matches!(conflict, HbotError::Conflict { .. }),
            "got {conflict:?}"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "one"
        );

        let updated = workspace
            .write_file(Path::new("a.txt"), "two", Some(&first.hash))
            .expect("matching hash must write");
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "two"
        );
        assert_ne!(updated.hash, first.hash);

        let missing = workspace
            .write_file(Path::new("b.txt"), "x", Some(&first.hash))
            .expect_err("missing file with hash must conflict");
        assert!(
            matches!(missing, HbotError::Conflict { .. }),
            "got {missing:?}"
        );
    }

    #[test]
    fn write_file_is_atomic_on_failure() {
        let temp = TestDir::new("write-atomic");
        let workspace = temp.workspace();
        // 目标是一个目录：tmp 已写入，rename 必然失败，必须清理 tmp 且原目录不受伤。
        std::fs::create_dir_all(temp.path().join("target-dir")).unwrap();
        let err = workspace
            .write_file(Path::new("target-dir"), "boom", None)
            .expect_err("cannot replace a directory");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
        assert!(temp.path().join("target-dir").is_dir());
        assert!(
            leftover_tmp_files(temp.path()).is_empty(),
            "temporary files must be cleaned up"
        );
    }

    #[test]
    fn atomic_write_cleans_tmp_when_rename_fails() {
        let temp = TestDir::new("write-rename-fail");
        // 目标是非空目录：tmp 能写成，finish 的 rename 必然失败 → 必须清理 tmp、不动目录。
        let target = temp.path().join("occupied");
        std::fs::create_dir_all(target.join("inner")).unwrap();
        let tmp = temp.path().join(".hbot-tmp-direct-test");
        let err = write_atomic(&tmp, &target, b"data").expect_err("rename onto a directory fails");
        assert!(
            matches!(err, HbotError::Internal { .. } | HbotError::Denied { .. }),
            "got {err:?}"
        );
        assert!(
            !tmp.exists(),
            "temporary file must be removed after failure"
        );
        assert!(target.is_dir(), "target directory must survive");
        assert!(
            target.join("inner").is_dir(),
            "target contents must survive"
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        assert!(leftover_tmp_files(temp.path()).is_empty());
    }

    #[test]
    fn write_file_never_leaves_partial_content() {
        let temp = TestDir::new("write-partial");
        let workspace = temp.workspace();
        let original = workspace
            .write_file(Path::new("keep.txt"), "original", None)
            .expect("write");
        let err = workspace
            .write_file(Path::new("keep.txt"), "replacement", Some("bad-hash"))
            .expect_err("conflict");
        assert!(matches!(err, HbotError::Conflict { .. }));
        let read = workspace
            .read_file(Path::new("keep.txt"), MAX_READ_BYTES)
            .expect("read");
        assert_eq!(read.content, "original");
        assert_eq!(read.hash, original.hash);
    }

    #[test]
    fn list_dir_skips_hbot_and_sorts() {
        let temp = TestDir::new("list");
        let workspace = temp.workspace();
        temp.write(".hbot/hbot.db", "db");
        temp.write(".git/config", "[core]");
        temp.write("b.txt", "b");
        temp.write("a.txt", "a");
        temp.write("dir/inner.txt", "i");
        let entries = workspace.list_dir(Path::new(".")).expect("list");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["dir", "a.txt", "b.txt"]);
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].path, "a.txt");
        assert_eq!(entries[1].size, 1);
    }

    #[test]
    fn create_dir_creates_nested_directories() {
        let temp = TestDir::new("mkdir");
        let workspace = temp.workspace();
        let created = workspace
            .create_dir(Path::new("a/b/c"))
            .expect("create nested");
        assert!(created.is_dir());
        assert_eq!(created, workspace.root().join("a").join("b").join("c"));
        assert!(workspace.create_dir(Path::new("a/b/c")).is_ok());
        let escape = workspace
            .create_dir(Path::new("../escape"))
            .expect_err("escape must fail");
        assert!(matches!(escape, HbotError::Invalid { .. }));
    }

    #[test]
    fn rename_moves_file_and_rejects_conflicts() {
        let temp = TestDir::new("rename");
        let workspace = temp.workspace();
        temp.write("a.txt", "a");
        temp.write("b.txt", "b");
        workspace
            .rename(Path::new("a.txt"), Path::new("c.txt"))
            .expect("rename");
        assert!(!temp.path().join("a.txt").exists());
        assert_eq!(fs::read_to_string(temp.path().join("c.txt")).unwrap(), "a");

        let conflict = workspace
            .rename(Path::new("c.txt"), Path::new("b.txt"))
            .expect_err("destination exists");
        assert!(
            matches!(conflict, HbotError::Conflict { .. }),
            "got {conflict:?}"
        );
        assert_eq!(fs::read_to_string(temp.path().join("b.txt")).unwrap(), "b");
    }

    #[test]
    fn trash_moves_instead_of_deleting() {
        let temp = TestDir::new("trash");
        let workspace = temp.workspace();
        temp.write("doomed.txt", "keep me");
        let destination = workspace
            .delete_to_trash(Path::new("doomed.txt"))
            .expect("trash");
        assert!(!temp.path().join("doomed.txt").exists());
        assert!(destination.starts_with(workspace.root().join(".hbot").join("trash")));
        assert_eq!(fs::read_to_string(&destination).unwrap(), "keep me");
        assert!(
            destination
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("doomed.txt")
        );

        temp.write("dir/inner.txt", "inner");
        let destination = workspace
            .delete_to_trash(Path::new("dir"))
            .expect("trash dir");
        assert!(!temp.path().join("dir").exists());
        assert!(destination.join("inner.txt").is_file());

        let escape = workspace
            .delete_to_trash(Path::new("../outside.txt"))
            .expect_err("escape must fail");
        assert!(matches!(escape, HbotError::Invalid { .. }));
    }

    #[test]
    fn rename_and_trash_move_the_symlink_itself() {
        let temp = TestDir::new("symlink-move");
        let workspace = temp.workspace();
        temp.write("real/secret.txt", "keep me");

        let make_link = |name: &str| -> std::io::Result<()> {
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(
                    temp.path().join("real").join("secret.txt"),
                    temp.path().join(name),
                )
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(
                    temp.path().join("real").join("secret.txt"),
                    temp.path().join(name),
                )
            }
            #[cfg(not(any(windows, unix)))]
            {
                let _ = name;
                Err(std::io::Error::other("unsupported platform"))
            }
        };

        if make_link("link.txt").is_err() {
            eprintln!("skipping symlink move test: cannot create symlink (needs privileges)");
            return;
        }

        let destination = workspace
            .delete_to_trash(Path::new("link.txt"))
            .expect("trash symlink");
        assert!(
            fs::symlink_metadata(temp.path().join("link.txt")).is_err(),
            "the link itself must be moved out"
        );
        assert!(
            temp.path().join("real").join("secret.txt").is_file(),
            "the link target must survive"
        );
        assert!(
            destination
                .symlink_metadata()
                .expect("trash entry")
                .file_type()
                .is_symlink(),
            "trash entry must still be a symlink"
        );

        if make_link("second.txt").is_err() {
            return;
        }
        workspace
            .rename(Path::new("second.txt"), Path::new("moved.txt"))
            .expect("rename symlink");
        assert!(temp.path().join("moved.txt").is_symlink());
        assert!(temp.path().join("real").join("secret.txt").is_file());
    }

    #[test]
    fn trash_keeps_unique_names() {
        let temp = TestDir::new("trash-unique");
        let workspace = temp.workspace();
        temp.write("same.txt", "one");
        let first = workspace
            .delete_to_trash(Path::new("same.txt"))
            .expect("first");
        temp.write("same.txt", "two");
        let second = workspace
            .delete_to_trash(Path::new("same.txt"))
            .expect("second");
        assert_ne!(first, second);
        assert!(first.is_file() && second.is_file());
    }
}
