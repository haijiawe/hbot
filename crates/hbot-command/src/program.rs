//! 程序解析：把"要跑什么"解析成一个**绝对路径**，并对照本次允许清单。
//!
//! 三条规则（每条都对应一个真实坑）：
//!
//! 1. **相对路径一律拒绝**。项目目录是 Agent 可写的；允许 `.\cargo` 或 `bin\run`
//!    等于让被约束的一方决定"这次到底跑哪个文件"（CWD 劫持）。
//! 2. **裸文件名只按父进程 `PATH` 搜索**，绝不搜当前目录。Windows 上还要注意：
//!    `CreateProcessW` 的 `lpApplicationName = NULL` 会自己搜当前目录与 PATH，
//!    旧项目的脚本里踩过（`AGENTS.md` §4.4）；Rust 的 `std::process::Command`
//!    在拿到**绝对路径**时不做任何搜索，所以这里先自己解析成绝对路径。
//! 3. **允许清单为空 = 什么都不许跑**，且比较的是 `canonicalize` 之后的绝对路径
//!    （防止同名程序靠 PATH 顺序顶替）。

use std::path::{Path, PathBuf};

use hbot_core::{CommandDenied, CommandSpec};

/// 一个参数的长度上限（Windows 单条命令行 32767 字符，这里按参数收紧）。
const MAX_ARG_BYTES: usize = 8 * 1024;

/// 全部参数加起来的长度上限。
const MAX_ARGS_BYTES: usize = 64 * 1024;

/// 解析并校验程序路径。
pub(crate) fn resolve_program(spec: &CommandSpec) -> Result<PathBuf, CommandDenied> {
    let raw = spec.program.as_str();
    if raw.is_empty() || raw.trim() != raw {
        return Err(CommandDenied::InvalidSpec {
            reason: "程序名不能为空，也不能带首尾空白".to_owned(),
        });
    }
    if raw.bytes().any(|byte| byte == 0) {
        return Err(CommandDenied::InvalidSpec {
            reason: "程序名里不能有 NUL".to_owned(),
        });
    }

    let looks_like_path = has_separator(raw) || is_windows_drive_relative(raw);
    let resolved = if looks_like_path {
        let path = Path::new(raw);
        if !path.is_absolute() {
            return Err(CommandDenied::ProgramNotAllowed {
                program: raw.to_owned(),
            });
        }
        canonical_executable(path)?
    } else {
        search_path(raw)?.ok_or_else(|| CommandDenied::ProgramNotFound {
            program: raw.to_owned(),
        })?
    };

    ensure_allowed(&resolved, spec)
}

/// 校验参数规模（超长参数在 Windows 上会被截断成另一条命令，比报错更危险）。
pub(crate) fn validate_args(args: &[String]) -> Result<(), CommandDenied> {
    let mut total = 0usize;
    for arg in args {
        if arg.bytes().any(|byte| byte == 0) {
            return Err(CommandDenied::InvalidSpec {
                reason: "参数里不能有 NUL".to_owned(),
            });
        }
        if arg.len() > MAX_ARG_BYTES {
            return Err(CommandDenied::InvalidSpec {
                reason: format!("单个参数超过 {MAX_ARG_BYTES} 字节"),
            });
        }
        total = total.saturating_add(arg.len() + 1);
        if total > MAX_ARGS_BYTES {
            return Err(CommandDenied::InvalidSpec {
                reason: format!("参数总长度超过 {MAX_ARGS_BYTES} 字节"),
            });
        }
    }
    Ok(())
}

fn ensure_allowed(resolved: &Path, spec: &CommandSpec) -> Result<PathBuf, CommandDenied> {
    if spec.allowed_programs.is_empty() {
        return Err(CommandDenied::ProgramNotAllowed {
            program: spec.program.clone(),
        });
    }
    let matched = spec.allowed_programs.iter().any(|allowed| {
        resolve_allowed_entry(allowed)
            .map(|candidate| paths_eq(&candidate, resolved))
            .unwrap_or(false)
    });
    if matched {
        Ok(resolved.to_path_buf())
    } else {
        Err(CommandDenied::ProgramNotAllowed {
            program: spec.program.clone(),
        })
    }
}

/// 解析允许清单里的一条（解析失败 = 这条匹配不上，不是错误）。
fn resolve_allowed_entry(allowed: &str) -> Option<PathBuf> {
    let path = Path::new(allowed);
    if path.is_absolute() {
        canonical_executable(path).ok()
    } else if has_separator(allowed) {
        None
    } else {
        search_path(allowed).ok().flatten()
    }
}

/// 规范化一个应当存在的可执行文件。
///
/// Windows 上额外拒绝 `.bat` / `.cmd`：它们必须由 `cmd.exe /c` 解释，
/// 而那条路径重新引入了引号解析、变量展开与重定向——等于我们手动把 shell 装回去。
fn canonical_executable(path: &Path) -> Result<PathBuf, CommandDenied> {
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd") {
            return Err(CommandDenied::ProgramNotAllowed {
                program: path.display().to_string(),
            });
        }
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| CommandDenied::ProgramNotFound {
        program: path.display().to_string(),
    })?;
    if canonical.is_file() {
        Ok(canonical)
    } else {
        Err(CommandDenied::ProgramNotFound {
            program: canonical.display().to_string(),
        })
    }
}

fn has_separator(raw: &str) -> bool {
    raw.contains('\\') || raw.contains('/')
}

/// `C:foo`（盘符相对）不是绝对路径，却也不是裸文件名——单独抓出来拒绝。
fn is_windows_drive_relative(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    cfg!(windows) && bytes.len() >= 2 && bytes[1] == b':' && !Path::new(raw).is_absolute()
}

/// 在父进程 `PATH` 里找可执行文件（不搜当前目录）。
fn search_path(name: &str) -> Result<Option<PathBuf>, CommandDenied> {
    let Some(path_value) = std::env::var_os("PATH") else {
        return Err(CommandDenied::ProgramNotFound {
            program: name.to_owned(),
        });
    };
    for directory in std::env::split_paths(&path_value) {
        if directory.as_os_str().is_empty() {
            // PATH 里的空段在 POSIX 语义下表示"当前目录"。我们刻意不支持它：
            // 那正是 CWD 劫持的入口。
            continue;
        }
        for candidate in name_candidates(name) {
            let full = directory.join(&candidate);
            if full.is_file() {
                if let Ok(canonical) = std::fs::canonicalize(&full) {
                    if canonical.is_file() {
                        return Ok(Some(canonical));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// 一个裸文件名可能对应的实际文件名（Windows 要按 `PATHEXT` 补扩展名）。
fn name_candidates(name: &str) -> Vec<String> {
    if cfg!(windows) {
        let has_known_extension = [".exe", ".com"]
            .iter()
            .any(|ext| name.len() > ext.len() && name.to_ascii_lowercase().ends_with(ext));
        if has_known_extension {
            vec![name.to_owned()]
        } else {
            vec![
                format!("{name}.exe"),
                format!("{name}.com"),
                name.to_owned(),
            ]
        }
    } else {
        vec![name.to_owned()]
    }
}

fn paths_eq(left: &Path, right: &Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_for(program: &str, allowed: &[&str]) -> CommandSpec {
        let mut spec = CommandSpec::new(program, std::env::temp_dir());
        spec.allowed_programs = allowed.iter().map(|entry| (*entry).to_owned()).collect();
        spec
    }

    #[test]
    fn empty_program_is_invalid() {
        let spec = spec_for("", &[]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::InvalidSpec { .. })
        ));
    }

    #[test]
    fn relative_path_is_rejected() {
        let spec = spec_for(".\\cargo.exe", &[".\\cargo.exe"]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::ProgramNotAllowed { .. })
        ));
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        let exe = std::env::current_exe().expect("current_exe");
        let spec = spec_for(&exe.display().to_string(), &[]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::ProgramNotAllowed { .. })
        ));
    }

    #[test]
    fn existing_absolute_program_is_accepted_when_allowed() {
        let exe = std::env::current_exe().expect("current_exe");
        let text = exe.display().to_string();
        let spec = spec_for(&text, &[&text]);
        let resolved = resolve_program(&spec).expect("允许清单内的绝对路径应通过");
        assert!(resolved.is_absolute());
        assert!(resolved.is_file());
    }

    #[test]
    fn program_outside_allowlist_is_rejected() {
        let exe = std::env::current_exe().expect("current_exe");
        let other = std::env::temp_dir().join("some-other-binary.exe");
        let spec = spec_for(&exe.display().to_string(), &[&other.display().to_string()]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::ProgramNotAllowed { .. })
        ));
    }

    #[test]
    fn missing_absolute_program_is_not_found() {
        let missing = std::env::temp_dir().join("hbot-definitely-missing-binary");
        let text = missing.display().to_string();
        let spec = spec_for(&text, &[&text]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::ProgramNotFound { .. })
        ));
    }

    #[test]
    fn batch_files_are_rejected() {
        let spec = spec_for("C:\\temp\\run.cmd", &["C:\\temp\\run.cmd"]);
        assert!(matches!(
            resolve_program(&spec),
            Err(CommandDenied::ProgramNotAllowed { .. })
        ));
    }

    #[test]
    fn oversized_args_are_invalid() {
        let args = vec!["x".repeat(MAX_ARG_BYTES + 1)];
        assert!(matches!(
            validate_args(&args),
            Err(CommandDenied::InvalidSpec { .. })
        ));
    }
}
