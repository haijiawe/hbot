//! ctx.files 的宿主后端：一个把路径钉在项目根里的 std::fs 实现（C3.file）。
//!
//! 三条不变量：
//! 1. 用户给的路径已经被 `normalize_path` 归一过（相对、/ 分隔、没有 .. 与盘符），
//!    本模块只负责把它接到项目根上；
//! 2. 接上之后还要再验一次真实位置 —— 符号链接可以把 a/b 指到项目外，
//!    归一化只能挡住字面上的 ..，挡不住链接；
//! 3. 只读写文本。二进制内容既没有类型真源，也没有体积判据，不属于这一版能力。

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use hbot_core::{
    FileEntry, FileError, FileStore, ReadFile, validate_file_list, validate_file_read,
    validate_file_write,
};

/// 以项目根为唯一可写目录的文件后端。
#[derive(Debug)]
pub struct ProjectFiles {
    root: PathBuf,
}

impl ProjectFiles {
    /// 用项目根建一个后端；根必须已经存在（调用方在拿到项目根时才建它）。
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = fs::canonicalize(root.into())?;
        Ok(Self { root })
    }

    /// 已规范化的项目根（绝对路径）。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把一条已归一的相对路径接到根上，并验证真实位置仍在根内。
    ///
    /// 最深的已存在祖先做 canonicalize 后必须仍以根开头；剩余（不存在的）段
    /// 只能是普通相对段 —— 它们来自 `normalize_path`，不会含 .. 。
    fn resolve(&self, relative: &str) -> Result<PathBuf, FileError> {
        let mut resolved = self.root.clone();
        let mut missing: Vec<&str> = Vec::new();
        let mut parts = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .peekable();
        while let Some(part) = parts.next() {
            let candidate = resolved.join(part);
            if candidate.exists() {
                resolved =
                    fs::canonicalize(&candidate).map_err(|error| self.failed(&candidate, error))?;
                if !resolved.starts_with(&self.root) {
                    return Err(FileError::Denied {
                        path: relative.to_owned(),
                    });
                }
            } else {
                missing.push(part);
                missing.extend(parts);
                break;
            }
        }
        for part in missing {
            if Path::new(part)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(FileError::Denied {
                    path: relative.to_owned(),
                });
            }
            resolved.push(part);
        }
        Ok(resolved)
    }

    fn failed(&self, path: &Path, error: io::Error) -> FileError {
        FileError::Failed {
            message: format!("{}: {error}", self.display(path)),
        }
    }

    /// 只暴露相对项目根的路径：绝对路径进日志等于把用户的目录结构写进排障记录。
    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn entry_of(&self, path: &Path) -> Result<FileEntry, FileError> {
        let metadata = fs::metadata(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: self.display(path),
            },
            _ => self.failed(path, error),
        })?;
        let relative = self.display(path);
        if metadata.is_dir() {
            Ok(FileEntry::dir(relative))
        } else {
            Ok(FileEntry::file(relative, metadata.len()))
        }
    }
}

impl FileStore for ProjectFiles {
    fn read(&self, path: &str, max_bytes: usize) -> Result<ReadFile, FileError> {
        validate_file_read(path, max_bytes)?;
        let resolved = self.resolve(path)?;
        let metadata = fs::metadata(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            _ => self.failed(&resolved, error),
        })?;
        if metadata.is_dir() {
            return Err(FileError::Invalid {
                message: format!("{path} 是目录，不是文件：要列目录请用 list()"),
            });
        }
        let bytes = fs::read(&resolved).map_err(|error| self.failed(&resolved, error))?;
        let truncated = bytes.len() > max_bytes;
        let slice = &bytes[..bytes.len().min(max_bytes)];
        let text = String::from_utf8(slice.to_vec()).map_err(|_| FileError::Invalid {
            message: format!(
                "{path} 不是 UTF-8 文本，这个能力只读文本（二进制请自己转 base64 存）"
            ),
        })?;
        Ok(ReadFile {
            path: path.to_owned(),
            text,
            bytes: slice.len(),
            truncated,
        })
    }

    fn write(&self, path: &str, text: &str) -> Result<FileEntry, FileError> {
        validate_file_write(path, text)?;
        let resolved = self.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| self.failed(parent, error))?;
            }
        }
        fs::write(&resolved, text.as_bytes()).map_err(|error| self.failed(&resolved, error))?;
        self.entry_of(&resolved)
    }

    fn remove(&self, path: &str) -> Result<(), FileError> {
        let resolved = self.resolve(path)?;
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            _ => self.failed(&resolved, error),
        })?;
        if resolved == self.root {
            return Err(FileError::Denied {
                path: path.to_owned(),
            });
        }
        if metadata.is_dir() {
            // 只删空目录：递归删除在生成代码手里等于一次静默的批量数据丢失。
            fs::remove_dir(&resolved).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => FileError::NotFound {
                    path: path.to_owned(),
                },
                io::ErrorKind::DirectoryNotEmpty => FileError::Denied {
                    path: format!("{path}（目录非空：先删里面的东西，或换一个空目录）"),
                },
                _ => self.failed(&resolved, error),
            })
        } else {
            fs::remove_file(&resolved).map_err(|error| self.failed(&resolved, error))
        }
    }

    fn list(&self, path: &str, limit: usize) -> Result<Vec<FileEntry>, FileError> {
        validate_file_list(limit)?;
        let resolved = self.resolve(path)?;
        let mut entries = Vec::new();
        let reader = fs::read_dir(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            io::ErrorKind::NotADirectory => FileError::Invalid {
                message: format!("{path} 不是目录"),
            },
            _ => self.failed(&resolved, error),
        })?;
        for entry in reader {
            let entry = entry.map_err(|error| self.failed(&resolved, error))?;
            if entries.len() >= limit {
                break;
            }
            entries.push(self.entry_of(&entry.path())?);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }
}

/// 清单里 files 能力的声明（读 / 写两半）。
#[must_use]
pub fn declared_files(root: &Path) -> hbot_core::FilesCapability {
    hbot_code::manifest::load(root)
        .map(|manifest| manifest.capabilities.files())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::MAX_FILE_READ_BYTES;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hbot-files-{}-{unique}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_then_read_round_trips_inside_the_root() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        let entry = files.write("notes/a.txt", "hello").expect("write");
        assert_eq!(entry.size, 5);
        let read = files
            .read("notes/a.txt", MAX_FILE_READ_BYTES)
            .expect("read");
        assert_eq!(read.text, "hello");
        assert!(!read.truncated);
        assert!(root.0.join("notes/a.txt").exists());
    }

    #[test]
    fn escaping_the_root_is_refused() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        assert!(matches!(
            files.read("../secret", 16),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            files.write("a/../../x", "x"),
            Err(FileError::Denied { .. })
        ));
        // 绝对路径（`/etc/x`、`C:/x`）在槽位层的 `normalize_file_path` 就拒了，
        // 到不了这里；后端只负责"已经归一的相对路径不许借 .. 或链接逃出去"。
        assert!(!root.0.join("x").exists());
    }

    #[test]
    fn reading_a_missing_file_is_not_found() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        assert!(matches!(
            files.read("nope.txt", 16),
            Err(FileError::NotFound { .. })
        ));
    }

    #[test]
    fn listing_reports_files_and_directories_sorted() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("b.txt", "bb").expect("write b");
        files.write("a/x.txt", "x").expect("write a");
        let entries = files.list("", 10).expect("list");
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "b.txt"]);
    }

    #[test]
    fn removing_a_non_empty_directory_is_denied() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("dir/a.txt", "x").expect("write");
        assert!(matches!(files.remove("dir"), Err(FileError::Denied { .. })));
        files.remove("dir/a.txt").expect("remove file");
        files.remove("dir").expect("remove empty dir");
    }

    #[test]
    fn a_truncated_read_says_so() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("long.txt", "0123456789").expect("write");
        let read = files.read("long.txt", 4).expect("read");
        assert_eq!(read.text, "0123");
        assert!(read.truncated);
        assert_eq!(read.bytes, 4);
    }
}
