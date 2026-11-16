//! 桌面端本地偏好：`<data_dir>/desktop.json`。
//!
//! 数据目录**只**从 [`hbot_core::data_dir`] 来（`AGENTS.md` 第 39 条：不许第四份路径
//! 算法）。本模块只接受调用方传进来的 `data_dir`，自己绝不拼 `APPDATA`/`HOME`。
//!
//! 文件形状是磁盘契约（`docs/spikes/S6-desktop-ops.md`）：
//!
//! ```json
//! { "autostart": false, "closeToTray": true }
//! ```
//!
//! 两个默认值都是产品决策，不是随手写的：`autostart` 默认 **关**（首次启动不许偷偷
//! 写注册表），`closeToTray` 默认 **开**（关窗之后后台 bot 要继续跑）。

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 偏好的文件名（数据目录根下，与 `hbot.db` 同级）。
pub const SETTINGS_FILE: &str = "desktop.json";

/// 原子写时用的临时后缀（与 `hbot-store` 的落盘写法同款：先写 `.tmp` 再 rename）。
const TEMP_SUFFIX: &str = ".tmp";

/// Windows 上 `rename` 撞到别的句柄会 EPERM，重试几次（`AGENTS.md` 第 7 条的教训）。
const RENAME_ATTEMPTS: u32 = 5;

/// rename 重试的间隔。
const RENAME_BACKOFF: Duration = Duration::from_millis(40);

/// 桌面端偏好。
///
/// `serde(default)` 让"老文件少一个字段"和"新文件多一个字段"都能读：偏好文件不该
/// 因为版本差异把桌面端卡在启动失败里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DesktopSettings {
    /// 开机自启（默认关）。
    pub autostart: bool,
    /// 关窗时最小化到托盘而不是退出（默认开）。
    pub close_to_tray: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            autostart: false,
            close_to_tray: true,
        }
    }
}

impl DesktopSettings {
    /// 偏好文件路径。
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// 读偏好。文件不存在 / 读不动 / JSON 坏掉都退回默认值并记一条 warn。
    ///
    /// **不返回错误**：偏好坏了不该让桌面端起不来。坏文件也不会被这里覆盖 ——
    /// 只有用户真的去切换开关时才重写它（避免把用户手写的内容悄悄抹掉）。
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "偏好文件读不动，改用默认值");
                return Self::default();
            }
        };

        match serde_json::from_str::<Self>(&text) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "偏好文件不是合法 JSON，改用默认值（文件保持原样）"
                );
                Self::default()
            }
        }
    }

    /// 写偏好（原子替换：先写 `.tmp` 再 rename）。
    ///
    /// 写出的 JSON 是**不带 BOM 的 UTF-8**：`serde_json` 不认 BOM，而 Windows 上
    /// `Set-Content -Encoding UTF8`（PS 5.1）恰好会加 BOM —— 所以这里只用
    /// `std::fs::write`，不在脚本里生成这个文件。
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;

        let path = Self::path(data_dir);
        let temp = path.with_extension(format!("json{TEMP_SUFFIX}"));

        let mut json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        json.push('\n');
        std::fs::write(&temp, json.as_bytes())?;

        // rename 在 Windows 上是 MoveFileEx(REPLACE_EXISTING)：目标刚被写出来的那一瞬
        // 可能被 Defender 之类的句柄拿着，于是 EPERM。这里重试几次，别把它当成写失败。
        let mut attempt = 0;
        loop {
            match std::fs::rename(&temp, &path) {
                Ok(()) => return Ok(()),
                Err(error) if attempt + 1 < RENAME_ATTEMPTS && is_transient(&error) => {
                    attempt += 1;
                    std::thread::sleep(RENAME_BACKOFF);
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&temp);
                    return Err(error);
                }
            }
        }
    }
}

/// 是否是"换个时机再来就好"的 rename 失败。
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 `lib.rs` 测试同一套临时目录写法（仓库不引 `tempfile`）。
    fn temp_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-desktop-settings-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("测试要能建临时目录");
        path
    }

    #[test]
    fn defaults_are_autostart_off_close_to_tray_on() {
        let settings = DesktopSettings::default();
        assert!(!settings.autostart, "首次启动不许默认开机自启");
        assert!(settings.close_to_tray, "关窗默认收到托盘");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(DesktopSettings::load(&dir), DesktopSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips_the_documented_shape() {
        let dir = temp_dir("round-trip");
        let settings = DesktopSettings {
            autostart: true,
            close_to_tray: false,
        };
        settings.save(&dir).expect("保存必须成功");

        let text = std::fs::read_to_string(DesktopSettings::path(&dir)).expect("文件必须存在");
        // 磁盘契约钉死：字段名是 camelCase 的 closeToTray + 小写的 autostart。
        let value: serde_json::Value = serde_json::from_str(&text).expect("必须是合法 JSON");
        assert_eq!(value["autostart"], serde_json::Value::Bool(true));
        assert_eq!(value["closeToTray"], serde_json::Value::Bool(false));
        assert!(text.ends_with('\n'), "写出的文件以换行结尾（editorconfig）");
        assert!(!text.contains('\u{feff}'), "不许带 BOM");
        assert!(
            !DesktopSettings::path(&dir)
                .with_extension("json.tmp")
                .exists(),
            "临时文件必须被 rename 掉"
        );

        assert_eq!(DesktopSettings::load(&dir), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_or_missing_fields_fall_back_to_defaults() {
        let dir = temp_dir("partial");
        // 只写一个字段：`serde(default)` 要把另一个补成默认值。
        std::fs::write(DesktopSettings::path(&dir), br#"{"autostart":true}"#).expect("写夹具");
        let loaded = DesktopSettings::load(&dir);
        assert!(loaded.autostart);
        assert!(loaded.close_to_tray, "缺字段必须退回默认值 true");

        // 多一个字段（未来版本写的）不许把文件判成坏文件。
        std::fs::write(
            DesktopSettings::path(&dir),
            br#"{"autostart":false,"closeToTray":false,"futureField":1}"#,
        )
        .expect("写夹具");
        let loaded = DesktopSettings::load(&dir);
        assert!(!loaded.autostart);
        assert!(!loaded.close_to_tray);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults_without_being_rewritten() {
        let dir = temp_dir("corrupt");
        let path = DesktopSettings::path(&dir);
        std::fs::write(&path, b"{ not json").expect("写夹具");

        assert_eq!(DesktopSettings::load(&dir), DesktopSettings::default());
        assert_eq!(
            std::fs::read(&path).expect("坏文件还在"),
            b"{ not json",
            "读失败不许顺手重写用户文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
