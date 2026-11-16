//! 应用数据目录的**唯一**算法。
//!
//! 这里只算路径，不建目录、不碰文件（所以仍然符合 core「不做 IO」的边界）。
//!
//! 为什么放 core：此前这段逻辑有三份拷贝，其中两份给 `%APPDATA%\hbot`，
//! 第三份（桌面壳的 `app.path().app_data_dir()`）给 `%APPDATA%\com.hbot.app`
//! —— 同一个产品的桌面端与后台端**各开一个库**，`hbot replay` 又按另一份找。
//! 更糟的是它把数据目录绑在 Tauri 的 `identifier` 上：改一次 identifier
//! 就等于静默搬家，用户数据留在旧目录里没人再去看。
//!
//! 现在目录只由 [`APP_DIR`] 决定，identifier 只影响安装/签名身份。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::command::Platform;

/// 应用目录名。
///
/// **改这个等于迁库**：老目录不会自动搬，需要写一次显式迁移（见
/// `docs/02-architecture.md` §9.1 的迁移纪律）。
pub const APP_DIR: &str = "hbot";

/// 当前平台的数据目录。
///
/// - Windows：`%APPDATA%\hbot`（缺失则 `%USERPROFILE%\AppData\Roaming\hbot`）
/// - macOS：`~/Library/Application Support/hbot`
/// - 其他 Unix：`$XDG_DATA_HOME/hbot`（缺失则 `~/.local/share/hbot`）
/// - 环境变量全缺（容器 / 精简服务）：当前目录下的 `hbot`，至少不 panic
pub fn data_dir() -> PathBuf {
    data_dir_for(current_platform(), |key| std::env::var_os(key))
}

/// 注入平台与环境的版本，供测试逐个分支钉住。
pub fn data_dir_for(platform: Platform, get: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    match platform {
        Platform::Windows => get("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join(APP_DIR))
            .or_else(|| {
                get("USERPROFILE").map(|profile| {
                    PathBuf::from(profile)
                        .join("AppData")
                        .join("Roaming")
                        .join(APP_DIR)
                })
            })
            .unwrap_or_else(fallback),
        // macOS 不该落 `~/.local/share`：那是 XDG 的口径，Finder 里看不见，
        // 用户备份工具也会跳过它。
        Platform::MacOs => get("HOME")
            .map(|home| {
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join(APP_DIR)
            })
            .unwrap_or_else(fallback),
        Platform::Linux | Platform::Other => get("XDG_DATA_HOME")
            .map(|data_home| PathBuf::from(data_home).join(APP_DIR))
            .or_else(|| {
                get("HOME").map(|home| {
                    PathBuf::from(home)
                        .join(".local")
                        .join("share")
                        .join(APP_DIR)
                })
            })
            .unwrap_or_else(fallback),
    }
}

/// 环境变量全缺时的落点：当前目录下的 `hbot`。有总比 panic 好，且日志里会写明。
fn fallback() -> PathBuf {
    PathBuf::from(APP_DIR)
}

/// 编译期决定的平台，避免 `cfg!(windows)` 在测试里不可注入。
#[must_use]
pub fn current_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn windows_prefers_appdata() {
        let dir = data_dir_for(
            Platform::Windows,
            env(&[
                ("APPDATA", r"C:\Users\me\AppData\Roaming"),
                ("USERPROFILE", r"C:\Users\me"),
            ]),
        );
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\me\AppData\Roaming").join("hbot")
        );
    }

    #[test]
    fn windows_falls_back_to_userprofile() {
        let dir = data_dir_for(Platform::Windows, env(&[("USERPROFILE", r"C:\Users\me")]));
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\me")
                .join("AppData")
                .join("Roaming")
                .join("hbot")
        );
    }

    #[test]
    fn macos_uses_application_support() {
        let dir = data_dir_for(Platform::MacOs, env(&[("HOME", "/Users/me")]));
        assert_eq!(
            dir,
            PathBuf::from("/Users/me")
                .join("Library")
                .join("Application Support")
                .join("hbot")
        );
    }

    #[test]
    fn linux_prefers_xdg_data_home() {
        let dir = data_dir_for(
            Platform::Linux,
            env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/me")]),
        );
        assert_eq!(dir, PathBuf::from("/xdg").join("hbot"));
    }

    #[test]
    fn linux_falls_back_to_local_share() {
        let dir = data_dir_for(Platform::Linux, env(&[("HOME", "/home/me")]));
        assert_eq!(
            dir,
            PathBuf::from("/home/me")
                .join(".local")
                .join("share")
                .join("hbot")
        );
    }

    #[test]
    fn no_environment_still_returns_a_relative_dir() {
        for platform in [
            Platform::Windows,
            Platform::MacOs,
            Platform::Linux,
            Platform::Other,
        ] {
            assert_eq!(data_dir_for(platform, env(&[])), PathBuf::from("hbot"));
        }
    }

    /// 三端必须算到同一个目录名：这条是本次修复的回归守卫。
    #[test]
    fn every_platform_ends_with_the_same_app_dir() {
        for (platform, pairs) in [
            (Platform::Windows, vec![("APPDATA", r"C:\a")]),
            (Platform::MacOs, vec![("HOME", "/h")]),
            (Platform::Linux, vec![("XDG_DATA_HOME", "/x")]),
        ] {
            let dir = data_dir_for(platform, env(&pairs));
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(APP_DIR));
        }
    }
}
