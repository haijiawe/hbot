//! 工作目录守卫：命令的 cwd 必须落在项目根内。
//!
//! 规则与 `hbot-workspace` 的路径守卫同源（`crates/hbot-workspace/src/project.rs`）：
//! 先 `canonicalize`（消掉 `..`、符号链接、`\\?\` 前缀），再**按组件**比较，
//! 不做字符串前缀判断——`C:\proj-evil` 不该被 `C:\proj` 当成"里面"。
//!
//! 为什么这里要抄一份而不是 `use hbot_workspace`：依赖方向是
//! `hbot-command → hbot-core`，本 crate 不许依赖 workspace（那会形成
//! command ← workspace 的回边，破坏 ADR-0002 的分层）。两处规则要一起改，
//! 这一点写在 ADR-0003「可逆性」里。

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use hbot_core::CommandDenied;

/// 解析并守卫工作目录。
///
/// - 空 `cwd` → 项目根；
/// - 相对 `cwd` → 相对项目根解析；
/// - 绝对 `cwd` → 原样使用（仍要过根内检查）；
/// - 最后一律 `canonicalize` 后按组件比较，逃逸（含 `..`、符号链接指向根外）即拒绝。
pub(crate) fn resolve_cwd(workspace_root: &Path, cwd: &Path) -> Result<PathBuf, CommandDenied> {
    let root = std::fs::canonicalize(workspace_root).map_err(|err| CommandDenied::InvalidSpec {
        reason: format!("项目根 `{}` 无法规范化（{err}）", workspace_root.display()),
    })?;

    let candidate = if cwd.as_os_str().is_empty() {
        root.clone()
    } else if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        root.join(cwd)
    };

    let canonical = std::fs::canonicalize(&candidate).map_err(|err| {
        // 目录不存在 / 不可访问也按"越界"拒绝：参数是 Agent 给的，不是用户点的，
        // 让它先学会给存在的目录；这里给的是可展示的一句话而不是 io 错误原文。
        CommandDenied::OutsideWorkspace {
            path: format!("{}（无法规范化：{err}）", candidate.display()),
        }
    })?;

    if is_within(&root, &canonical) {
        Ok(canonical)
    } else {
        Err(CommandDenied::OutsideWorkspace {
            path: canonical.display().to_string(),
        })
    }
}

/// `candidate` 是否在 `root` 之内（含等于 `root`）。
///
/// 先试 `strip_prefix`（字节级，快路径），失败再按组件大小写不敏感比较
/// ——Windows 盘符/目录名大小写可以不同，而 `\\?\C:\Proj` 与 `\\?\C:\proj`
/// 指向同一个目录。
pub(crate) fn is_within(root: &Path, candidate: &Path) -> bool {
    if candidate == root {
        return true;
    }
    if let Ok(rest) = candidate.strip_prefix(root) {
        return !rest.as_os_str().is_empty();
    }
    let root_components: Vec<Component<'_>> = root.components().collect();
    let candidate_components: Vec<Component<'_>> = candidate.components().collect();
    if candidate_components.len() <= root_components.len() {
        // 等长时已在上面 `==` 判过；短于根一定不在根内。
        return false;
    }
    root_components
        .iter()
        .zip(candidate_components.iter())
        .all(|(left, right)| component_eq(left, right))
}

fn component_eq(left: &Component<'_>, right: &Component<'_>) -> bool {
    match (left, right) {
        (Component::Prefix(a), Component::Prefix(b)) => os_str_eq(a.as_os_str(), b.as_os_str()),
        (Component::RootDir, Component::RootDir) => true,
        (Component::CurDir, Component::CurDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(a), Component::Normal(b)) => os_str_eq(a, b),
        _ => false,
    }
}

fn os_str_eq(left: &OsStr, right: &OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hbot-command-path-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inside")).expect("建测试目录");
        std::fs::canonicalize(dir).expect("规范化测试目录")
    }

    #[test]
    fn inside_directory_is_accepted() {
        let root = temp_root("inside");
        let cwd = resolve_cwd(&root, Path::new("inside")).expect("根内目录应通过");
        assert!(is_within(&root, &cwd));
    }

    #[test]
    fn empty_cwd_means_project_root() {
        let root = temp_root("empty");
        assert_eq!(resolve_cwd(&root, Path::new("")).expect("空 cwd"), root);
    }

    #[test]
    fn parent_dir_escape_is_rejected() {
        let root = temp_root("escape");
        let denied = resolve_cwd(&root, Path::new("..")).expect_err("`..` 必须被拒绝");
        assert!(matches!(denied, CommandDenied::OutsideWorkspace { .. }));
    }

    #[test]
    fn absolute_outside_directory_is_rejected() {
        let root = temp_root("absolute");
        let outside = std::env::temp_dir();
        let denied = resolve_cwd(&root, &outside).expect_err("根外绝对路径必须被拒绝");
        assert!(matches!(denied, CommandDenied::OutsideWorkspace { .. }));
    }

    #[test]
    fn sibling_with_shared_prefix_is_not_inside() {
        let root = temp_root("prefix");
        let sibling = root.with_file_name(format!(
            "{}-evil",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).expect("建同名前缀目录");
        assert!(
            !is_within(&root, &sibling),
            "共享字符串前缀的兄弟目录不算根内"
        );
    }
}
