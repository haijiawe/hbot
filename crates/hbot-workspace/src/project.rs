//! 项目根与路径守卫。
//!
//! 这是 `hbot-workspace` 的安全边界：所有对外暴露的文件 API 都必须先经过
//! [`Workspace::resolve_existing`] 或 [`Workspace::resolve_new`]，得到
//! **规范化后仍在项目根内**的绝对路径，再交给 `files` / `search` 使用。
//!
//! 规则清单（可写进文档）：
//!
//! 1. 入口只接受**相对路径**；空路径、含 NUL、超过 4 KiB 的路径直接拒绝。
//! 2. 拒绝绝对路径：POSIX `/x`、Windows 盘符 `C:\x`、`\\server\share`、
//!    设备路径 `\\?\C:\x`。
//! 3. 即使不是绝对路径，也拒绝盘符前缀（`C:x`，Windows 的 drive-relative 形式）。
//! 4. 拒绝任何 `..` 段（`../x`、`a/../../x`）；`.` 段被忽略（`./a` 等价 `a`）。
//! 5. 拒绝 `.git/` 内部（策略 [`WorkspaceConfig::allow_git`] 可放开）。
//! 6. 拒绝外部访问 `.hbot/`（运行数据目录；内部回收站操作不受此限）。
//! 7. Windows 上额外拒绝：以 `.`/空格结尾的段（Win32 会静默裁剪，能绕过第 5 条）、
//!    `<>:"|?*` 与控制字符（含 NTFS ADS 的 `:`）、保留设备名（`CON`、`NUL`、`COM1`…`LPT9`）。
//! 8. [`Workspace::resolve_existing`]：`fs::canonicalize` 解析全部符号链接后，
//!    结果必须按**路径组件**逐个等于根前缀；逃逸（`..` 或符号链接指向外部）一律拒绝。
//! 9. [`Workspace::resolve_new`]：父目录必须已存在、可规范化且在根内；目标已存在时
//!    要求它也不是逃逸的符号链接；悬空符号链接拒绝写入。
//! 10. [`Workspace::resolve_write_target`]：写入用的**宽松版** —— 父目录可以尚不存在
//!     （写入方按 `create_dir_all` 语义逐级创建），但第 1–7 条对**每一段**（含尚不存在的
//!     父目录段）照旧生效，最深的已存在祖先仍要可规范化且在根内。**宽松只体现在
//!     “父目录可以缺失”，不体现在“路径可以越界”**：越界路径在创建任何目录之前就被拒绝。
//! 11. 组件比较在 Windows / macOS 上大小写不敏感（文件系统语义），其余平台按字节比较。

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use hbot_core::HbotError;

/// 单次读取的硬上限：2 MiB（超过则 UI 只读，见 `docs/06-ide.md` §4.2）。
pub const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;

/// 相对路径最大长度（字节）。Windows 长路径上限 32K，但 UI 路径远小于此。
const MAX_REL_PATH_LEN: usize = 4096;

/// Windows 保留设备名（含扩展名形式，如 `CON.txt` 也保留）。
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 工作区策略。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// 目录列出与搜索时跳过的条目名（默认 `.hbot`），大小写不敏感。
    pub skip_names: Vec<String>,
    /// 是否允许访问 `.git/` 内部（默认 `false`）。
    pub allow_git: bool,
    /// 是否允许外部 API 访问 `.hbot/`（默认 `false`）。
    pub allow_dot_hbot: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            skip_names: vec![".hbot".to_owned()],
            allow_git: false,
            allow_dot_hbot: false,
        }
    }
}

/// 守卫用途：决定 `.hbot/` 是否可访问。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    /// 必须已存在。
    Existing,
    /// 允许不存在（新建）。
    New,
    /// 内部调用（允许 `.hbot/`）。
    Internal,
}

/// 一个已打开的项目根。
///
/// `Workspace` 可跨线程共享（`Send + Sync`）；写操作在实例内串行化，避免
/// Windows 上“目标被占用 → rename EPERM”时互相踩（`AGENTS.md` §7 第 14 条）。
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    config: WorkspaceConfig,
    write_lock: Mutex<()>,
}

impl Workspace {
    /// 打开项目根；路径必须存在、是目录，且会被规范化为绝对路径。
    pub fn open(root: PathBuf) -> Result<Self, HbotError> {
        Self::open_with_config(root, WorkspaceConfig::default())
    }

    /// 用自定义策略打开项目根。
    pub fn open_with_config(root: PathBuf, config: WorkspaceConfig) -> Result<Self, HbotError> {
        let display = root.display().to_string();
        let canonical = fs::canonicalize(&root).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => HbotError::not_found("workspace root", display.clone()),
            io::ErrorKind::PermissionDenied => {
                HbotError::denied(format!("cannot access workspace root `{display}`: {err}"))
            }
            _ => HbotError::invalid(format!("cannot resolve workspace root `{display}`: {err}")),
        })?;
        let meta = fs::metadata(&canonical)
            .map_err(|err| io_error(&canonical, "workspace root", &canonical, err))?;
        if !meta.is_dir() {
            return Err(HbotError::invalid(format!(
                "workspace root `{display}` is not a directory"
            )));
        }
        Ok(Self {
            root: canonical,
            config,
            write_lock: Mutex::new(()),
        })
    }

    /// 规范化后的项目根（绝对路径，符号链接已解析）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 当前策略。
    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    /// 面向调用方（**包括模型**）的路径文案：相对项目根、`/` 分隔。
    ///
    /// 错误文案会随工具输出发给模型、可能离开本机，所以这里只做减法（RT-01，
    /// 见 `docs/security/red-team-findings.md`）：能相对化就相对化，根外只留
    /// 文件名，绝不再回显绝对路径 / `\\?\` 前缀 / 用户名。详细判据见
    /// [`display_rel`]。
    pub(crate) fn display_path(&self, path: &Path) -> String {
        display_rel(&self.root, path)
    }

    /// 把根内绝对路径转成 UI 用的相对路径（`/` 分隔）；根外路径返回 `Invalid`。
    pub fn relative_path(&self, path: &Path) -> Result<String, HbotError> {
        let rel = strip_root(&self.root, path).ok_or_else(|| {
            HbotError::invalid(format!(
                "path `{}` is outside the workspace root",
                self.display_path(path)
            ))
        })?;
        Ok(ui_path(&rel))
    }

    /// 路径是否在根内（按组件比较；仅对已规范化的路径有意义）。
    pub fn is_within(&self, path: &Path) -> bool {
        strip_root(&self.root, path).is_some()
    }

    /// 解析一个必须已存在的相对路径，返回根内规范化绝对路径。
    ///
    /// 任何逃逸（`..`、绝对路径、符号链接指向根外、`.git/`）都返回
    /// [`HbotError::Invalid`]，不落 IO。
    pub fn resolve_existing(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let normalized = self.normalized_rel(rel, Access::Existing)?;
        let joined = self.join_normalized(&normalized);
        self.canonical_within(&joined, Access::Existing, "path")
    }

    /// 解析一个**待新建**的相对路径：父目录必须已存在且在根内。
    ///
    /// 返回值的父目录是规范化后的真实目录；若目标已存在且是符号链接，
    /// 只有链接指向根内才允许，且**写操作替换链接本身、不跟随链接**。
    pub fn resolve_new(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let normalized = self.normalized_rel(rel, Access::New)?;
        let joined = self.join_normalized(&normalized);
        let parent = joined
            .parent()
            .ok_or_else(|| HbotError::invalid("path has no parent directory"))?;
        let name = joined
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| HbotError::invalid("path has no file name"))?;
        let parent_canon = self.canonical_within(parent, Access::New, "parent directory")?;
        if !parent_canon.is_dir() {
            return Err(HbotError::invalid(format!(
                "parent `{}` is not a directory",
                self.display_path(&parent_canon)
            )));
        }
        let target = parent_canon.join(name);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let real = fs::canonicalize(&target).map_err(|_| {
                    HbotError::invalid(format!(
                        "`{}` is a dangling symlink; refusing to write through it",
                        self.display_path(&target)
                    ))
                })?;
                self.ensure_within(&real, Access::Existing, "path")?;
                Ok(target)
            }
            Ok(_) => Ok(target),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(target),
            Err(err) => Err(io_error(&self.root, "path", &target, err)),
        }
    }

    /// 解析一个**待写入**的相对路径，允许父目录尚不存在。
    ///
    /// 与 [`Workspace::resolve_new`] 的差别只有一条：缺失的父目录不再报
    /// [`HbotError::NotFound`]，而是像 [`Workspace::resolve_new_dir`] 一样取最深的
    /// 已存在祖先做规范化与根内校验，再把缺失的父目录段原样拼回去。写入方据此可以
    /// 逐级 `create_dir_all` 后再落盘（见 [`Workspace::write_file`]）。
    ///
    /// **守卫强度不变**：`normalized_rel` 在解析的第一步就对**每一段**（含尚不存在的
    /// 父目录段）做绝对路径 / `..` / 盘符 / `.git` / `.hbot` / Windows 保留名检查，
    /// 这一步早于任何目录创建；最深的已存在祖先还要 `canonical_within`（符号链接
    /// 逃逸）且必须是目录。所以“先建父目录再写”不会变成越界后门。
    pub fn resolve_write_target(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let normalized = self.normalized_rel(rel, Access::New)?;
        let Some((parent_rel, name)) = normalized.rsplit_once('/') else {
            // 顶层文件：父目录就是根，沿用 `resolve_new` 的最后一段规则。
            return self.resolve_new(rel);
        };
        let parent = self.resolve_new_dir(Path::new(parent_rel))?;
        let target = parent.join(name);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let real = fs::canonicalize(&target).map_err(|_| {
                    HbotError::invalid(format!(
                        "`{}` is a dangling symlink; refusing to write through it",
                        self.display_path(&target)
                    ))
                })?;
                self.ensure_within(&real, Access::Existing, "path")?;
                Ok(target)
            }
            Ok(_) => Ok(target),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(target),
            Err(err) => Err(io_error(&self.root, "path", &target, err)),
        }
    }

    /// 解析一个**已存在**的条目，但**不跟随最后一段符号链接**。
    ///
    /// 用于“移动/删除链接本身”这类操作（重命名、移入回收站）：父目录规范化并校验在
    /// 根内，最后一段按名字返回。这样删除链接不会连带删除它指向的文件。
    pub(crate) fn resolve_entry(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let normalized = self.normalized_rel(rel, Access::Existing)?;
        let joined = self.join_normalized(&normalized);
        if joined == self.root {
            return Ok(self.root.clone());
        }
        let parent = joined
            .parent()
            .ok_or_else(|| HbotError::invalid("path has no parent directory"))?;
        let name = joined
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| HbotError::invalid("path has no file name"))?;
        let parent_canon = self.canonical_within(parent, Access::Existing, "parent directory")?;
        if !parent_canon.is_dir() {
            return Err(HbotError::invalid(format!(
                "parent `{}` is not a directory",
                self.display_path(&parent_canon)
            )));
        }
        let target = parent_canon.join(name);
        match fs::symlink_metadata(&target) {
            Ok(_) => Ok(target),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(HbotError::not_found("path", self.display_path(&target)))
            }
            Err(err) => Err(io_error(&self.root, "path", &target, err)),
        }
    }

    /// 解析一个用于**建目录**的路径：取最深的已存在祖先做规范化与根内校验，
    /// 再把尚不存在的部分拼回去（`create_dir_all` 语义）。
    pub(crate) fn resolve_new_dir(&self, rel: &Path) -> Result<PathBuf, HbotError> {
        let normalized = self.normalized_rel(rel, Access::New)?;
        let joined = self.join_normalized(&normalized);
        let mut cursor = joined.as_path();
        let mut missing: Vec<&OsStr> = Vec::new();
        loop {
            match fs::symlink_metadata(cursor) {
                Ok(_) => {
                    let canonical = self.canonical_within(cursor, Access::New, "path")?;
                    if !canonical.is_dir() {
                        return Err(HbotError::invalid(format!(
                            "`{}` is not a directory",
                            self.display_path(&canonical)
                        )));
                    }
                    let mut resolved = canonical;
                    for part in missing.iter().rev() {
                        resolved.push(part);
                    }
                    return Ok(resolved);
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    let name = cursor.file_name().ok_or_else(|| {
                        HbotError::invalid(format!(
                            "cannot resolve path `{}`",
                            self.display_path(&joined)
                        ))
                    })?;
                    missing.push(name);
                    cursor = cursor.parent().ok_or_else(|| {
                        HbotError::invalid(format!(
                            "cannot resolve path `{}`",
                            self.display_path(&joined)
                        ))
                    })?;
                }
                Err(err) => return Err(io_error(&self.root, "path", cursor, err)),
            }
        }
    }

    /// 串行化写操作的锁；中毒视为内部错误（不 panic）。
    pub(crate) fn lock_writes(&self) -> Result<std::sync::MutexGuard<'_, ()>, HbotError> {
        self.write_lock
            .lock()
            .map_err(|_| HbotError::internal("workspace write lock is poisoned"))
    }

    /// 目录列出/搜索时是否跳过某个条目名。
    pub(crate) fn is_skipped_name(&self, name: &str) -> bool {
        if !self.config.allow_git && name.eq_ignore_ascii_case(".git") {
            return true;
        }
        if name.starts_with(".hbot-tmp-") {
            return true;
        }
        self.config
            .skip_names
            .iter()
            .any(|skip| name.eq_ignore_ascii_case(skip))
    }

    fn join_normalized(&self, normalized: &str) -> PathBuf {
        if normalized.is_empty() {
            self.root.clone()
        } else {
            self.root.join(normalized)
        }
    }

    /// 校验并规范化相对路径，返回 `a/b/c` 形式（`.` 段被丢弃）。空串表示根。
    fn normalized_rel(&self, rel: &Path, access: Access) -> Result<String, HbotError> {
        let raw = rel
            .to_str()
            .ok_or_else(|| HbotError::invalid("path must be valid UTF-8"))?;
        if raw.is_empty() {
            return Err(HbotError::invalid("path is empty"));
        }
        if raw.contains('\0') {
            return Err(HbotError::invalid("path contains a NUL byte"));
        }
        if raw.len() > MAX_REL_PATH_LEN {
            return Err(HbotError::invalid(format!(
                "path is longer than {MAX_REL_PATH_LEN} bytes"
            )));
        }
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Err(HbotError::invalid(format!(
                "absolute paths are not allowed: `{raw}`"
            )));
        }
        if raw.starts_with("\\\\") || raw.starts_with("//") {
            return Err(HbotError::invalid(format!(
                "UNC or device paths are not allowed: `{raw}`"
            )));
        }
        if has_drive_prefix(raw) {
            return Err(HbotError::invalid(format!(
                "Windows drive paths are not allowed: `{raw}`"
            )));
        }

        let mut parts: Vec<&str> = Vec::new();
        for component in candidate.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(HbotError::invalid(format!(
                        "absolute paths are not allowed: `{raw}`"
                    )));
                }
                Component::ParentDir => {
                    return Err(HbotError::invalid(format!(
                        "path traversal (`..`) is not allowed: `{raw}`"
                    )));
                }
                Component::CurDir => {}
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| HbotError::invalid("path must be valid UTF-8"))?;
                    reject_windows_unsafe_name(name, raw)?;
                    self.check_reserved(name, access, raw)?;
                    parts.push(name);
                }
            }
        }

        if parts.is_empty() {
            if access == Access::New {
                return Err(HbotError::invalid(format!(
                    "path must name a file or directory: `{raw}`"
                )));
            }
            return Ok(String::new());
        }
        Ok(parts.join("/"))
    }

    fn check_reserved(&self, name: &str, access: Access, raw: &str) -> Result<(), HbotError> {
        if name.eq_ignore_ascii_case(".git") && !self.config.allow_git {
            return Err(HbotError::invalid(format!(
                "`{raw}` is inside .git/, which the workspace guard rejects"
            )));
        }
        if name.eq_ignore_ascii_case(".hbot")
            && access != Access::Internal
            && !self.config.allow_dot_hbot
        {
            return Err(HbotError::invalid(format!(
                "`{raw}` is inside .hbot/, which is reserved for runtime data"
            )));
        }
        Ok(())
    }

    fn canonical_within(
        &self,
        path: &Path,
        access: Access,
        what: &str,
    ) -> Result<PathBuf, HbotError> {
        let canonical = fs::canonicalize(path).map_err(|err| {
            let shown = self.display_path(path);
            match err.kind() {
                io::ErrorKind::NotFound => HbotError::not_found(what, shown),
                io::ErrorKind::PermissionDenied => {
                    HbotError::denied(format!("cannot access {what} `{shown}`: {err}"))
                }
                _ => HbotError::invalid(format!("cannot resolve {what} `{shown}`: {err}")),
            }
        })?;
        self.ensure_within(&canonical, access, what)?;
        Ok(canonical)
    }

    fn ensure_within(&self, path: &Path, access: Access, what: &str) -> Result<(), HbotError> {
        let rel = strip_root(&self.root, path).ok_or_else(|| {
            HbotError::invalid(format!(
                "{what} escapes the workspace root: `{}`",
                self.display_path(path)
            ))
        })?;
        let display = self.display_path(path);
        for component in rel.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(HbotError::invalid(format!(
                        "{what} contains `..`: `{display}`"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(HbotError::invalid(format!(
                        "{what} is not relative: `{display}`"
                    )));
                }
                Component::Normal(name) => {
                    self.check_reserved(&name.to_string_lossy(), access, &display)?;
                }
            }
        }
        Ok(())
    }
}

/// 把路径换成**可以发给模型**的文案：相对项目根（`/` 分隔），根外只留文件名。
///
/// RT-01 的本体就在这里（见 `docs/security/red-team-findings.md`）：普通拼错文件名
/// 曾把 `\\?\E:\...\用户名\...` 整条绝对路径写进 `NotFound` 文案，随工具输出发给
/// 模型、离开本机。所以这里**只做减法**：
///
/// - 在根内 → `ui_path`（`src/main.ts`）；根自身 → `.`；
/// - 在根外 → 只留最后一段文件名（`Path::file_name`），拿不到就 `<path>`。
///
/// 无论如何都不回显绝对路径、`\\?\` 前缀或用户目录。唯一的例外是
/// [`Workspace::open_with_config`] 里"根本身打不开"的文案 —— 那串是**调用方自己传
/// 进来的根路径**，此时还没有可相对化的基准，也没有项目根可泄漏。
pub(crate) fn display_rel(root: &Path, path: &Path) -> String {
    if let Some(rel) = strip_root(root, path) {
        let shown = ui_path(&rel);
        return if shown.is_empty() {
            ".".to_owned()
        } else {
            shown
        };
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<path>".to_owned())
}

/// 把 IO 错误映射成 [`HbotError`]：不存在 → `NotFound`，无权限 → `Denied`，其余 → `Internal`。
///
/// `root` 只用来把 `path` 相对化（[`display_rel`]）；错误文案里**不出现**它的绝对形态。
pub(crate) fn io_error(root: &Path, context: &str, path: &Path, err: io::Error) -> HbotError {
    let shown = display_rel(root, path);
    match err.kind() {
        io::ErrorKind::NotFound => HbotError::not_found(context, shown),
        io::ErrorKind::PermissionDenied => HbotError::denied(format!("{context} `{shown}`: {err}")),
        _ => HbotError::internal(format!("{context} `{shown}`: {err}")),
    }
}

/// `C:` / `c:` 这类盘符前缀（Windows drive-relative，`Path::is_absolute` 不认为是绝对路径）。
fn has_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// Windows 文件系统的静默改写会绕过前面的守卫，因此在 Windows 上直接拒绝这些名字。
fn reject_windows_unsafe_name(name: &str, raw: &str) -> Result<(), HbotError> {
    if !cfg!(windows) {
        return Ok(());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(HbotError::invalid(format!(
            "path component `{name}` ends with a dot or space, which Windows strips: `{raw}`"
        )));
    }
    if name.chars().any(|ch| {
        matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\') || (ch as u32) < 0x20
    }) {
        return Err(HbotError::invalid(format!(
            "path component `{name}` contains characters Windows rejects: `{raw}`"
        )));
    }
    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(HbotError::invalid(format!(
            "path component `{name}` is a reserved Windows device name: `{raw}`"
        )));
    }
    Ok(())
}

/// 去掉根前缀，返回剩余相对路径；不在根内返回 `None`。
fn strip_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut path_components = path.components();
    for root_component in root.components() {
        match path_components.next() {
            Some(path_component) if components_eq(root_component, path_component) => {}
            _ => return None,
        }
    }
    Some(path_components.collect())
}

fn components_eq(a: Component<'_>, b: Component<'_>) -> bool {
    match (a, b) {
        (Component::Prefix(a), Component::Prefix(b)) => os_str_eq(a.as_os_str(), b.as_os_str()),
        (Component::RootDir, Component::RootDir) => true,
        (Component::CurDir, Component::CurDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(a), Component::Normal(b)) => os_str_eq(a, b),
        _ => false,
    }
}

fn os_str_eq(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// 相对路径 → UI 路径（`/` 分隔）。
fn ui_path(rel: &Path) -> String {
    let mut out = String::new();
    for (index, component) in rel.components().enumerate() {
        if index > 0 {
            out.push('/');
        }
        match component {
            Component::Normal(name) => out.push_str(&name.to_string_lossy()),
            Component::ParentDir => out.push_str(".."),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn assert_invalid(workspace: &Workspace, rel: &str) {
        let err = workspace
            .resolve_existing(Path::new(rel))
            .expect_err("escape must be rejected");
        assert!(
            matches!(err, HbotError::Invalid { .. }),
            "expected Invalid for `{rel}`, got {err:?}"
        );
    }

    #[test]
    fn open_rejects_missing_root() {
        let temp = TestDir::new("missing-root");
        let missing = temp.path().join("nope");
        let err = Workspace::open(missing).expect_err("missing root must fail");
        assert!(matches!(err, HbotError::NotFound { .. }), "got {err:?}");
    }

    #[test]
    fn open_rejects_file_root() {
        let temp = TestDir::new("file-root");
        let file = temp.write("plain.txt", "x");
        let err = Workspace::open(file).expect_err("file root must fail");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn open_canonicalizes_root() {
        let temp = TestDir::new("canonical");
        let workspace = Workspace::open(temp.path().join(".")).expect("open");
        assert!(workspace.root().is_absolute(), "root must be absolute");
        assert_eq!(workspace.root(), workspace.root().canonicalize().unwrap());
    }

    #[test]
    fn resolve_existing_rejects_traversal() {
        let temp = TestDir::new("traversal");
        let workspace = temp.workspace();
        temp.write("inside.txt", "hi");
        assert_invalid(&workspace, "../x");
        assert_invalid(&workspace, "a/../../x");
        assert_invalid(&workspace, "..");
        assert_invalid(&workspace, "a/../b");
        assert_invalid(&workspace, "");
    }

    #[test]
    fn resolve_existing_rejects_absolute_and_drive_paths() {
        let temp = TestDir::new("absolute");
        let workspace = temp.workspace();
        assert_invalid(&workspace, "/etc/passwd");
        assert_invalid(&workspace, "C:\\Windows\\x");
        assert_invalid(&workspace, "\\\\?\\C:\\x");
        assert_invalid(&workspace, "\\\\server\\share\\x");
        assert_invalid(&workspace, "//server/share/x");
        assert_invalid(&workspace, "C:x");
    }

    #[test]
    fn resolve_existing_rejects_git_internals() {
        let temp = TestDir::new("git-dir");
        let workspace = temp.workspace();
        temp.write(".git/config", "[core]");
        assert_invalid(&workspace, ".git/config");
        assert_invalid(&workspace, ".git");
        assert_invalid(&workspace, "sub/.git/config");
    }

    #[test]
    fn resolve_existing_rejects_dot_hbot() {
        let temp = TestDir::new("hbot-dir");
        let workspace = temp.workspace();
        temp.write(".hbot/hbot.db", "db");
        assert_invalid(&workspace, ".hbot/hbot.db");
    }

    #[test]
    fn resolve_new_rejects_escaping_parent() {
        let temp = TestDir::new("new-parent");
        let workspace = temp.workspace();
        for bad in [
            "../x.txt",
            "a/../../x.txt",
            "C:\\x.txt",
            ".git/x.txt",
            ".hbot/x.txt",
        ] {
            let err = workspace
                .resolve_new(Path::new(bad))
                .expect_err("must be rejected");
            assert!(
                matches!(err, HbotError::Invalid { .. }),
                "expected Invalid for `{bad}`, got {err:?}"
            );
        }
    }

    #[test]
    fn resolve_new_requires_existing_parent() {
        let temp = TestDir::new("new-missing-parent");
        let workspace = temp.workspace();
        let err = workspace
            .resolve_new(Path::new("missing/dir/file.txt"))
            .expect_err("missing parent must fail");
        assert!(matches!(err, HbotError::NotFound { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_new_accepts_inside_path() {
        let temp = TestDir::new("new-ok");
        let workspace = temp.workspace();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        let resolved = workspace
            .resolve_new(Path::new("./src/./main.ts"))
            .expect("resolve");
        assert_eq!(resolved, workspace.root().join("src").join("main.ts"));
    }

    #[test]
    fn resolve_write_target_allows_missing_parents_without_creating_them() {
        let temp = TestDir::new("write-target-missing");
        let workspace = temp.workspace();
        let target = workspace
            .resolve_write_target(Path::new("src/lib/x.ts"))
            .expect("missing parents must be resolvable");
        assert_eq!(
            target,
            workspace.root().join("src").join("lib").join("x.ts")
        );
        assert!(
            !temp.path().join("src").exists(),
            "resolving a write target must not create directories"
        );
    }

    /// 宽松解析不等于放开守卫：每一种越界写法都在**产生任何 IO 之前**被拒。
    #[test]
    fn resolve_write_target_rejects_escaping_shapes() {
        let temp = TestDir::new("write-target-guard");
        let workspace = temp.workspace();
        let bad = [
            "../x.ts",
            r"C:\x.ts",
            "/abs/x.ts",
            "src/../../../x.ts",
            "src/.git/x.ts",
            ".hbot/x.ts",
        ];
        for rel in bad {
            let err = workspace
                .resolve_write_target(Path::new(rel))
                .expect_err(rel);
            assert!(
                matches!(err, HbotError::Invalid { .. }),
                "expected Invalid for `{rel}`, got {err:?}"
            );
        }
        assert!(
            !temp.path().join("src").exists() && !temp.path().join(".git").exists(),
            "rejected paths must not create anything"
        );
    }

    #[test]
    fn resolve_write_target_rejects_symlinked_ancestor_escape() {
        let temp = TestDir::new("write-target-symlink");
        let workspace = temp.workspace();
        let outside = TestDir::new("write-target-symlink-outside");

        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(outside.path(), temp.path().join("escape"));
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(outside.path(), temp.path().join("escape"));
        #[cfg(not(any(windows, unix)))]
        let created: std::io::Result<()> = Err(std::io::Error::other("unsupported platform"));

        if created.is_err() {
            eprintln!(
                "skipping symlink write-target test: cannot create symlink (needs privileges)"
            );
            return;
        }

        let err = workspace
            .resolve_write_target(Path::new("escape/sub/x.ts"))
            .expect_err("write through escaping symlink must fail");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
        assert!(!outside.path().join("sub").exists());
    }

    #[test]
    fn relative_path_rejects_outside_path() {
        let temp = TestDir::new("relative");
        let workspace = temp.workspace();
        let outside = temp.path().parent().unwrap().to_path_buf();
        assert!(workspace.relative_path(&outside).is_err());
        temp.write("a/b.txt", "x");
        let inside = workspace.resolve_existing(Path::new("a/b.txt")).unwrap();
        assert_eq!(workspace.relative_path(&inside).unwrap(), "a/b.txt");
    }

    #[test]
    fn symlink_escape_is_rejected() {
        let temp = TestDir::new("symlink");
        let workspace = temp.workspace();
        let outside = TestDir::new("symlink-outside");
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(outside.path(), temp.path().join("escape"));
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(outside.path(), temp.path().join("escape"));
        #[cfg(not(any(windows, unix)))]
        let created: std::io::Result<()> = Err(std::io::Error::other("unsupported platform"));

        if created.is_err() {
            eprintln!("skipping symlink escape test: cannot create symlink (needs privileges)");
            return;
        }

        assert_invalid(&workspace, "escape/secret.txt");
        assert_invalid(&workspace, "escape");
        let err = workspace
            .resolve_new(Path::new("escape/new.txt"))
            .expect_err("write through escaping symlink must fail");
        assert!(matches!(err, HbotError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn symlink_inside_root_is_allowed() {
        let temp = TestDir::new("symlink-inside");
        let workspace = temp.workspace();
        temp.write("real/secret.txt", "ok");

        #[cfg(windows)]
        let created =
            std::os::windows::fs::symlink_dir(temp.path().join("real"), temp.path().join("alias"));
        #[cfg(unix)]
        let created =
            std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("alias"));
        #[cfg(not(any(windows, unix)))]
        let created: std::io::Result<()> = Err(std::io::Error::other("unsupported platform"));

        if created.is_err() {
            eprintln!("skipping symlink-inside test: cannot create symlink (needs privileges)");
            return;
        }

        let resolved = workspace
            .resolve_existing(Path::new("alias/secret.txt"))
            .expect("inside symlink must resolve");
        assert_eq!(resolved, workspace.root().join("real").join("secret.txt"));
    }

    #[test]
    fn windows_unsafe_names_are_rejected_on_windows() {
        if !cfg!(windows) {
            eprintln!("skipping Windows-only guard test");
            return;
        }
        let temp = TestDir::new("win-names");
        let workspace = temp.workspace();
        for bad in [
            ".git./config",
            "CON",
            "a/nul.txt",
            "trailing./x",
            "ads:stream",
        ] {
            assert_invalid(&workspace, bad);
        }
    }
}
