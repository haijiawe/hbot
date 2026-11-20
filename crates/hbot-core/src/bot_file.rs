//! 生成代码读写项目内文件的窄接口（C3.file）。
//!
//! 三条硬判据：路径只能相对于项目根、只有清单里声明过才可用、单次读写的字节数有上限。
//! 路径规范化只在这里做一次 —— runtime 层不再自己判一遍，否则两份判据迟早会分叉。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 单条相对路径的字节上限（含子目录）。
pub const MAX_FILE_PATH_BYTES: usize = 256;
/// 一个路径段的字节上限。
pub const MAX_FILE_SEGMENT_BYTES: usize = 64;
/// 单次读取的字节上限。
pub const MAX_FILE_READ_BYTES: usize = 256 * 1024;
/// 单次按行读取最多跨多少行。
///
/// 字节上限挡不住「读第 900 万行」这种请求：它不占内存，但要让后端把整个文件
/// 逐行扫一遍。所以按行读取除了字节闸，还要有行数闸。
pub const MAX_FILE_READ_LINES: u64 = 10_000;
/// 单次写入的字节上限。
pub const MAX_FILE_WRITE_BYTES: usize = 1024 * 1024;
/// 目录列表一次最多返回多少条。
pub const MAX_FILE_LIST_ENTRIES: usize = 200;
/// 递归删除一次最多删掉多少条（含目录自身）。
///
/// 这个上限不为安全服务（路径已经钉在项目根里），而是为**可预测的耗时**服务：
/// 一次 `remove(recursive)` 在宿主线程上同步走完整棵树，几万条时 handler 早已超时，
/// 而用户只看到「删到一半」。超了就拒绝并让调用方分批删。
pub const MAX_FILE_REMOVE_ENTRIES: usize = 1_000;

/// 清单里 files 能力的声明。
///
/// 读与写分开声明：只读的项目（比如只做数据导出）不该顺手拿到删除权限。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesCapability {
    /// 允许读与列目录。
    pub read: bool,
    /// 允许写与删除。
    pub write: bool,
}

impl FilesCapability {
    /// 有没有声明任何一半。
    #[must_use]
    pub fn is_declared(&self) -> bool {
        self.read || self.write
    }

    /// 读与列目录是否被允许。
    #[must_use]
    pub fn allows_read(&self) -> bool {
        self.read
    }

    /// 写与删除是否被允许。
    #[must_use]
    pub fn allows_write(&self) -> bool {
        self.write
    }
}

/// 目录里一条记录的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// 普通文件。
    File,
    /// 目录。
    Dir,
}

impl FileKind {
    /// 写进日志与 JSON 的字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
        }
    }

    /// 从字符串解析；认不出来返回 None（不猜成 File）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "file" => Some(Self::File),
            "dir" => Some(Self::Dir),
            _ => None,
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 目录列表里的一条。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对项目根的路径，用 / 分隔。
    pub path: String,
    /// 文件还是目录。
    pub kind: FileKind,
    /// 字节数；目录恒为 0。
    pub size: u64,
}

impl FileEntry {
    /// 一条文件记录。
    #[must_use]
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            kind: FileKind::File,
            size,
        }
    }

    /// 一条目录记录。
    #[must_use]
    pub fn dir(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: FileKind::Dir,
            size: 0,
        }
    }
}

/// 按行读取的范围（1 起、两端都含）。
///
/// 存在这个类型里而不是拆成两个 `Option<u64>` 参数：行号必须成对解释，
/// 「只给了 start」和「只给了 end」在中途被谁默认成什么，是这类接口最容易分叉的地方，
/// 所以两个默认值只在 JS 桥那一层决定一次。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    /// 第一行（1 起）。
    pub start: u64,
    /// 最后一行（含）。
    pub end: u64,
}

impl LineRange {
    /// 从 start 到 end（两端都含）。
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// 跨了多少行（含两端）。
    #[must_use]
    pub const fn span(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

/// 一次读取的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFile {
    /// 相对项目根的路径。
    pub path: String,
    /// 文件内容（UTF-8 文本）。
    pub text: String,
    /// 实际读到的字节数。
    pub bytes: usize,
    /// 是否因为在 max_bytes 处截断。
    pub truncated: bool,
}

/// 一次二进制读取的结果。
///
/// **core 不做编码**：`bytes` 就是原样字节，base64 在 runtime 层（`hbot-code`）编码，
/// 这样契约层不依赖编码格式，也不引依赖。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBytes {
    /// 相对项目根的路径。
    pub path: String,
    /// 原样字节（最多 `max_bytes` 个，`truncated` 为 true 时后面还有）。
    pub bytes: Vec<u8>,
    /// 是否因为在 max_bytes 处截断。
    pub truncated: bool,
}

/// 文件能力的错误。
///
/// NotDeclared / Unavailable 的分工与别的能力一致：前者补清单，后者配宿主。
#[derive(Debug)]
pub enum FileError {
    /// 清单里没声明。
    NotDeclared {
        /// 需要声明的是哪一半（read 或 write）。
        capability: String,
    },
    /// 宿主没装配。
    Unavailable {
        /// 说明。
        reason: String,
    },
    /// 参数不合法。
    Invalid {
        /// 说明。
        message: String,
    },
    /// 路径试图跑出项目根。
    Denied {
        /// 被拒的路径。
        path: String,
    },
    /// 文件或目录不存在。
    NotFound {
        /// 路径。
        path: String,
    },
    /// 超过体积上限。
    TooLarge {
        /// 路径。
        path: String,
        /// 上限。
        limit: usize,
    },
    /// 一次操作涉及的条目太多（目前只有递归删除会用它）。
    TooMany {
        /// 路径。
        path: String,
        /// 上限（条）。
        limit: usize,
    },
    /// 底层失败。
    Failed {
        /// 说明。
        message: String,
    },
}

impl FileError {
    /// 稳定的错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_FILE_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_FILE_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_FILE_INVALID",
            Self::Denied { .. } => "HBOT_FILE_DENIED",
            Self::NotFound { .. } => "HBOT_FILE_NOT_FOUND",
            Self::TooLarge { .. } => "HBOT_FILE_TOO_LARGE",
            Self::TooMany { .. } => "HBOT_FILE_TOO_MANY",
            Self::Failed { .. } => "HBOT_FILE_FAILED",
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDeclared { capability } => write!(
                f,
                "{}: 清单里没有声明 capabilities.files.{}，这个能力对生成代码不可见",
                self.code(),
                capability
            ),
            Self::Unavailable { reason } => {
                write!(f, "{}: {reason}", self.code())
            }
            Self::Invalid { message } => write!(f, "{}: {message}", self.code()),
            Self::Denied { path } => write!(
                f,
                "{}: 路径 {path} 跑出了项目根 —— 只接受项目内的相对路径，不带 .. 也不以 / 开头",
                self.code()
            ),
            Self::NotFound { path } => {
                write!(f, "{}: {path} 不存在", self.code())
            }
            Self::TooLarge { path, limit } => write!(
                f,
                "{}: {path} 超过上限 {limit} 字节，请分块读写",
                self.code()
            ),
            Self::TooMany { path, limit } => write!(
                f,
                "{}: {path} 一次会涉及超过 {limit} 条记录，请分批操作",
                self.code()
            ),
            Self::Failed { message } => write!(f, "{}: {message}", self.code()),
        }
    }
}

impl std::error::Error for FileError {}

/// 把用户给的路径规范成相对项目根的形式。
///
/// 空串表示项目根（只有列目录允许用它）。绝对路径、反斜杠、.. 与 . 段、
/// 控制字符一律 Denied 或 Invalid —— 不做「尽量拼回去」的猜测。
pub fn normalize_path(raw: &str) -> Result<String, FileError> {
    let trimmed = raw.trim();
    if trimmed.len() > MAX_FILE_PATH_BYTES {
        return Err(FileError::Invalid {
            message: format!("路径超过 {MAX_FILE_PATH_BYTES} 字节"),
        });
    }
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.contains('\\') {
        return Err(FileError::Invalid {
            message: "路径里不要用反斜杠，统一用 /".to_owned(),
        });
    }
    if trimmed.contains(':') {
        return Err(FileError::Denied {
            path: trimmed.to_owned(),
        });
    }
    if trimmed.starts_with('/') {
        return Err(FileError::Denied {
            path: trimmed.to_owned(),
        });
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(FileError::Invalid {
            message: "路径里不能有控制字符".to_owned(),
        });
    }
    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err(FileError::Invalid {
                message: "路径里不能有空目录段（不要出现 // 或结尾的 /）".to_owned(),
            });
        }
        if segment == "." || segment == ".." {
            return Err(FileError::Denied {
                path: trimmed.to_owned(),
            });
        }
        if segment.len() > MAX_FILE_SEGMENT_BYTES {
            return Err(FileError::Invalid {
                message: format!("单段路径超过 {MAX_FILE_SEGMENT_BYTES} 字节"),
            });
        }
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

/// 读之前的参数闸。
pub fn validate_read(
    path: &str,
    max_bytes: usize,
    range: Option<LineRange>,
) -> Result<(), FileError> {
    if max_bytes == 0 || max_bytes > MAX_FILE_READ_BYTES {
        return Err(FileError::Invalid {
            message: format!("maxBytes 必须在 1..={MAX_FILE_READ_BYTES} 之间"),
        });
    }
    if path.is_empty() {
        return Err(FileError::Invalid {
            message: "read 需要具体文件路径，不能是项目根".to_owned(),
        });
    }
    if let Some(range) = range {
        if range.start == 0 {
            return Err(FileError::Invalid {
                message: "startLine 从 1 开始（第 1 行就是文件的第一行）".to_owned(),
            });
        }
        if range.end < range.start {
            return Err(FileError::Invalid {
                message: format!(
                    "endLine（{}）不能小于 startLine（{}）",
                    range.end, range.start
                ),
            });
        }
        if range.span() > MAX_FILE_READ_LINES {
            return Err(FileError::Invalid {
                message: format!(
                    "一次最多读 {MAX_FILE_READ_LINES} 行（这次要 {} 行），请分段读",
                    range.span()
                ),
            });
        }
    }
    Ok(())
}

/// 写之前的参数闸。
pub fn validate_write(path: &str, text: &str) -> Result<(), FileError> {
    if path.is_empty() {
        return Err(FileError::Invalid {
            message: "write 需要具体文件路径，不能是项目根".to_owned(),
        });
    }
    if text.contains('\0') {
        return Err(FileError::Invalid {
            message: "文本里有 NUL 字节".to_owned(),
        });
    }
    if text.len() > MAX_FILE_WRITE_BYTES {
        return Err(FileError::TooLarge {
            path: path.to_owned(),
            limit: MAX_FILE_WRITE_BYTES,
        });
    }
    Ok(())
}

/// 按行写之前的参数闸。
///
/// `range` 是要被**替换掉**的那几行（1 起、两端都含）；`text` 是替换进去的内容。
/// 行号越界（0 / 逆序）报错不夹取 —— 夹取会让「替换第 0 行」静默变成「替换第 1 行」。
pub fn validate_write_range(path: &str, text: &str, range: LineRange) -> Result<(), FileError> {
    validate_write(path, text)?;
    if range.start == 0 {
        return Err(FileError::Invalid {
            message: "startLine 从 1 开始（第 1 行就是文件的第一行）".to_owned(),
        });
    }
    if range.end < range.start {
        return Err(FileError::Invalid {
            message: format!(
                "endLine（{}）不能小于 startLine（{}）",
                range.end, range.start
            ),
        });
    }
    Ok(())
}

/// 列目录之前的参数闸。
pub fn validate_list(limit: usize) -> Result<(), FileError> {
    if limit == 0 || limit > MAX_FILE_LIST_ENTRIES {
        return Err(FileError::Invalid {
            message: format!("limit 必须在 1..={MAX_FILE_LIST_ENTRIES} 之间"),
        });
    }
    Ok(())
}

/// 删之前的参数闸。
pub fn validate_remove(path: &str) -> Result<(), FileError> {
    if path.is_empty() {
        return Err(FileError::Invalid {
            message: "remove 需要具体路径，不能是项目根".to_owned(),
        });
    }
    Ok(())
}

/// 文件能力的端口。
///
/// 实现方收到的路径已经被 normalize_path 过：相对、/ 分隔、没有 .. 。
pub trait FileStore: Send + Sync {
    /// 读一个文本文件。
    ///
    /// `range` 为 `None` 时从文件头读到 `max_bytes`；给了范围就只读那几行
    /// （实现方应当逐行流式读，不要为了取第 5 万行把整个文件读进内存）。
    fn read(
        &self,
        path: &str,
        max_bytes: usize,
        range: Option<LineRange>,
    ) -> Result<ReadFile, FileError>;
    /// 读一个二进制文件（原样字节，最多 `max_bytes` 个）。
    ///
    /// 与 [`FileStore::read`] 的区别只有一处：**不做 UTF-8 校验**。图片、zip、
    /// protobuf 这类内容进来是什么就是什么，编码交给调用方。
    fn read_bytes(&self, path: &str, max_bytes: usize) -> Result<ReadBytes, FileError>;
    /// 覆盖写一个文本文件。
    fn write(&self, path: &str, text: &str) -> Result<FileEntry, FileError>;
    /// 把 `range` 那几行替换成 `text`，其余行原样保留。
    ///
    /// 行尾（`\n` 还是 `\r\n`）按**未触碰的那一行**来，不统一重写：把 CRLF 项目
    /// 整文件改成 LF 会让一次「改第 3 行」变成全文件 diff。
    fn write_range(&self, path: &str, text: &str, range: LineRange)
    -> Result<FileEntry, FileError>;
    /// 删除一个文件或目录；`recursive` 为 false 时只允许删空目录。
    ///
    /// 返回删掉的条目数（1 = 只删了一个文件或空目录）。实现方**不许跟随符号链接**：
    /// 递归时遇到链接一律拒绝，否则 `remove(recursive)` 会变成「删掉链接指向的项目外目录」。
    fn remove(&self, path: &str, recursive: bool) -> Result<u32, FileError>;
    /// 列一个目录（不递归）。
    fn list(&self, path: &str, limit: usize) -> Result<Vec<FileEntry>, FileError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_relative_paths_survive_normalization() {
        assert_eq!(normalize_path("data/a.txt").unwrap(), "data/a.txt");
        assert_eq!(normalize_path("  a.txt  ").unwrap(), "a.txt");
        assert_eq!(normalize_path("").unwrap(), "");
    }

    #[test]
    fn absolute_and_backslash_paths_are_refused() {
        assert!(matches!(
            normalize_path("/etc/passwd"),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            normalize_path("C:/x"),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            normalize_path("data\\a.txt"),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn dot_dot_is_a_denial_not_a_rewrite() {
        assert!(matches!(
            normalize_path("../secret"),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            normalize_path("a/../../b"),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            normalize_path("./a"),
            Err(FileError::Denied { .. })
        ));
    }

    #[test]
    fn empty_segments_are_invalid() {
        assert!(matches!(
            normalize_path("a//b"),
            Err(FileError::Invalid { .. })
        ));
        assert!(matches!(
            normalize_path("a/"),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn control_characters_are_invalid() {
        assert!(matches!(
            normalize_path("a\u{7}b"),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn overlong_paths_are_invalid() {
        let long = "a".repeat(MAX_FILE_PATH_BYTES + 1);
        assert!(matches!(
            normalize_path(&long),
            Err(FileError::Invalid { .. })
        ));
        let segment = "b".repeat(MAX_FILE_SEGMENT_BYTES + 1);
        assert!(matches!(
            normalize_path(&segment),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn reading_the_project_root_is_invalid() {
        assert!(matches!(
            validate_read("", 1024, None),
            Err(FileError::Invalid { .. })
        ));
        assert!(validate_read("a.txt", 1024, None).is_ok());
    }

    #[test]
    fn read_limits_are_enforced_before_any_io() {
        assert!(matches!(
            validate_read("a.txt", 0, None),
            Err(FileError::Invalid { .. })
        ));
        assert!(matches!(
            validate_read("a.txt", MAX_FILE_READ_BYTES + 1, None),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn oversized_writes_are_too_large() {
        let big = "x".repeat(MAX_FILE_WRITE_BYTES + 1);
        assert!(matches!(
            validate_write("a.txt", &big),
            Err(FileError::TooLarge { .. })
        ));
        assert!(validate_write("a.txt", "ok").is_ok());
    }

    #[test]
    fn nul_bytes_are_invalid() {
        assert!(matches!(
            validate_write("a.txt", "a\0b"),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn the_declaration_is_read_and_write_separately() {
        let read_only = FilesCapability {
            read: true,
            write: false,
        };
        assert!(read_only.is_declared());
        assert!(read_only.allows_read());
        assert!(!read_only.allows_write());
        let empty = FilesCapability::default();
        assert!(!empty.is_declared());
    }

    #[test]
    fn error_codes_all_carry_the_prefix() {
        let errors = [
            FileError::NotDeclared {
                capability: "read".to_owned(),
            },
            FileError::Unavailable {
                reason: "x".to_owned(),
            },
            FileError::Invalid {
                message: "x".to_owned(),
            },
            FileError::Denied {
                path: "x".to_owned(),
            },
            FileError::NotFound {
                path: "x".to_owned(),
            },
            FileError::TooLarge {
                path: "x".to_owned(),
                limit: 1,
            },
            FileError::TooMany {
                path: "x".to_owned(),
                limit: 1,
            },
            FileError::Failed {
                message: "x".to_owned(),
            },
        ];
        for error in errors {
            assert!(error.to_string().starts_with(error.code()), "{error}");
            assert!(error.code().starts_with("HBOT_FILE_"));
        }
    }

    #[test]
    fn kinds_round_trip_through_their_strings() {
        for kind in [FileKind::File, FileKind::Dir] {
            assert_eq!(FileKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(FileKind::parse("socket"), None);
    }

    #[test]
    fn list_limits_are_enforced() {
        assert!(matches!(validate_list(0), Err(FileError::Invalid { .. })));
        assert!(validate_list(MAX_FILE_LIST_ENTRIES).is_ok());
        assert!(matches!(
            validate_list(MAX_FILE_LIST_ENTRIES + 1),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn a_line_range_is_one_based_and_inclusive() {
        assert!(validate_read("a.txt", 1024, Some(LineRange::new(1, 1))).is_ok());
        assert_eq!(LineRange::new(1, 1).span(), 1);
        assert_eq!(LineRange::new(10, 20).span(), 11);
    }

    #[test]
    fn zero_and_reversed_line_ranges_are_refused() {
        assert!(matches!(
            validate_read("a.txt", 1024, Some(LineRange::new(0, 5))),
            Err(FileError::Invalid { .. })
        ));
        assert!(matches!(
            validate_read("a.txt", 1024, Some(LineRange::new(9, 3))),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn a_line_range_wider_than_the_limit_is_refused() {
        assert!(matches!(
            validate_read(
                "a.txt",
                1024,
                Some(LineRange::new(1, MAX_FILE_READ_LINES + 1))
            ),
            Err(FileError::Invalid { .. })
        ));
        assert!(validate_read("a.txt", 1024, Some(LineRange::new(1, MAX_FILE_READ_LINES))).is_ok());
    }

    #[test]
    fn zero_and_reversed_write_ranges_are_refused() {
        assert!(matches!(
            validate_write_range("a.txt", "x", LineRange::new(0, 1)),
            Err(FileError::Invalid { .. })
        ));
        assert!(matches!(
            validate_write_range("a.txt", "x", LineRange::new(5, 2)),
            Err(FileError::Invalid { .. })
        ));
        assert!(validate_write_range("a.txt", "x", LineRange::new(3, 3)).is_ok());
    }

    #[test]
    fn a_write_range_still_obeys_the_text_limits() {
        let big = "x".repeat(MAX_FILE_WRITE_BYTES + 1);
        assert!(matches!(
            validate_write_range("a.txt", &big, LineRange::new(1, 1)),
            Err(FileError::TooLarge { .. })
        ));
        assert!(matches!(
            validate_write_range("a.txt", "a\0b", LineRange::new(1, 1)),
            Err(FileError::Invalid { .. })
        ));
    }

    #[test]
    fn the_remove_budget_is_a_named_constant() {
        // 递归删除的条目上限是给「可预测的耗时」用的，不是安全边界；变了要有测试提醒。
        assert_eq!(MAX_FILE_REMOVE_ENTRIES, 1_000);
    }
}
