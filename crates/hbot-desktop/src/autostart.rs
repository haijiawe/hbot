//! 开机自启开关的三平台落地。
//!
//! | 平台 | 落点 | 关闭时 |
//! |---|---|---|
//! | Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的字符串值 `hbot` | 删掉这个值 |
//! | Linux | `~/.config/autostart/hbot.desktop`（`XDG_CONFIG_HOME` 优先） | 删掉这个文件 |
//! | macOS | `~/Library/LaunchAgents/com.hbot.desktop.plist` | 删掉这个文件 |
//!
//! 三条硬约束：
//!
//! 1. **只写当前用户**：Windows 用 `HKCU`（**不是** `HKLM`），所以不需要管理员权限；
//!    Linux/macOS 只写自己家目录下的 autostart 位置。
//! 2. **幂等**：Windows 用固定值名 `hbot` 覆盖写，Linux/macOS 用固定文件名整覆盖 ——
//!    开两次不会出现两条。
//! 3. **只动自己那一个标识**：关闭时只删 `hbot` 这个值/文件，不扫描、不清空整个键或目录。
//!
//! 数据目录不在这里算（`AGENTS.md` 第 39 条）：本模块只拿可执行文件路径，位置由调用方
//! （[`crate::cli`] 的 `--set-autostart` 或托盘菜单）决定。这里出现的 `~/.config`、
//! `~/Library` 是**配置**目录，不是产品数据目录，与 `hbot_core::data_dir` 不重叠。

use std::io;
use std::path::{Path, PathBuf};

/// Windows `Run` 键下的值名，也用作 Linux/macOS 的文件名主语。
pub const AUTOSTART_ID: &str = "hbot";

/// Windows 的 `Run` 键路径（`HKCU` 相对路径）。
pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Linux 的 autostart 文件名。
pub const DESKTOP_ENTRY_FILE: &str = "hbot.desktop";

/// macOS 的 LaunchAgent 标签。
pub const LAUNCH_AGENT_LABEL: &str = "com.hbot.desktop";

/// macOS 的 LaunchAgent 文件名（与标签同名，是约定）。
pub const LAUNCH_AGENT_FILE: &str = "com.hbot.desktop.plist";

/// 开机自启的**实际**系统状态（不是偏好文件里的记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartState {
    /// 系统里确实注册了。
    Enabled,
    /// 系统里确实没有。
    Disabled,
    /// 查不出来（权限、坏键、读不到家目录）。
    Unknown,
}

/// 当前可执行文件路径（自启项要指向它）。
pub fn exec_path() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// 按当前可执行文件打开/关闭自启。
pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let exec = exec_path()?;
    set_enabled_for(&exec, enabled)
}

/// 按指定可执行文件打开/关闭自启（测试与脚本用这个，免得依赖 `current_exe`）。
pub fn set_enabled_for(exec: &Path, enabled: bool) -> io::Result<()> {
    imp::set_enabled_for(exec, enabled)
}

/// 查系统里的实际状态。
pub fn current() -> AutostartState {
    imp::current()
}

// ---------------------------------------------------------------------------
// 纯函数：把"自启项内容长什么样"从"写到哪"里拆出来，这样在任何平台上都能测。
// ---------------------------------------------------------------------------

/// Linux `.desktop` 文件内容。
///
/// 字段说明：`X-GNOME-Autostart-enabled=true` 是 GNOME 的开关（缺省 true，显式写出
/// 是为了让 `grep` 能验），`Terminal=false` 因为桌面壳自带窗口。
pub fn desktop_entry_contents(exec: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=HBot\n\
         Comment=HBot 桌面壳（QQ 机器人 IDE）\n\
         Exec={}\n\
         Terminal=false\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_field(exec)
    )
}

/// Linux `Exec=` 的取值：桌面条目规范里带保留字符的路径**必须**加引号并转义。
///
/// 见 freedesktop 的 Desktop Entry Specification：双引号内的 `"`、`` ` ``、`$`、`\`
/// 要用反斜杠转义；`%` 是字段码前缀，要写成 `%%`。
fn exec_field(exec: &Path) -> String {
    let raw = exec.to_string_lossy().replace('%', "%%");
    let needs_quote = raw
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '`' | '$' | '\\' | '\'' | '%'));

    if !needs_quote {
        return raw;
    }

    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    for c in raw.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// macOS LaunchAgent plist 内容（`RunAtLoad` 即可；不写 `KeepAlive`，崩溃后不要无限重启）。
pub fn launch_agent_contents(exec: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exec}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#,
        label = xml_escape(LAUNCH_AGENT_LABEL),
        exec = xml_escape(&exec.to_string_lossy())
    )
}

/// XML 文本转义（只覆盖文本节点里必须转义的五个字符）。
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// 写文件（父目录不存在就建出来；已存在就整覆盖 —— 这就是幂等的来源）。
///
/// 只有 Linux/macOS 用得上（Windows 走注册表 API），所以在 Windows 上要整个 cfg 掉，
/// 否则 `-D warnings` 会红。
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents.as_bytes())
}

/// 删文件：不存在算成功（关闭一个本来就没开的开关不该报错）。
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;

    /// `ERROR_FILE_NOT_FOUND` 的 HRESULT：值/键不存在时注册表 API 返回这个码。
    ///
    /// 注意 `windows_registry::Error` **不是**公开名字（crate 里只有私有的
    /// `use windows_result::*`），所以这里不写类型标注，靠调用点的推断拿 `.code()`；
    /// 常量也就不用依赖 `windows_result` 的导出名，直接拼成 `0x80070002`。
    const ERROR_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;

    pub(super) fn set_enabled_for(exec: &Path, enabled: bool) -> io::Result<()> {
        set_run_value_at(RUN_KEY_PATH, AUTOSTART_ID, exec, enabled)
    }

    pub(super) fn current() -> AutostartState {
        state_at(RUN_KEY_PATH, AUTOSTART_ID)
    }

    /// 在指定 `Run` 键里设置/删除一个值。
    ///
    /// 单独抽出来是为了测试能写进 `HKCU\Software\hbot-desktop-selftest\...` 而不是真的
    /// 注册开机自启（测试不该改用户的真实自启项）。
    pub(super) fn set_run_value_at(
        key_path: &str,
        value_name: &str,
        exec: &Path,
        enabled: bool,
    ) -> io::Result<()> {
        let key = windows_registry::CURRENT_USER
            .create(key_path)
            .map_err(|error| io::Error::other(error.to_string()))?;

        if enabled {
            // 值名固定 → 重复开只是覆盖同一条，不会出现两条。
            key.set_string(value_name, command_line(exec))
                .map_err(|error| io::Error::other(error.to_string()))
        } else {
            match key.remove_value(value_name) {
                Ok(()) => Ok(()),
                // 本来就没有 = 已经是关闭状态。
                Err(error) if error.code().0 == ERROR_FILE_NOT_FOUND => Ok(()),
                Err(error) => Err(io::Error::other(error.to_string())),
            }
        }
    }

    /// 查某个 `Run` 键里有没有这个值（只关心在不在，不关心内容）。
    pub(super) fn state_at(key_path: &str, value_name: &str) -> AutostartState {
        let opened = match windows_registry::CURRENT_USER.open(key_path) {
            Ok(key) => key,
            Err(error) if error.code().0 == ERROR_FILE_NOT_FOUND => {
                return AutostartState::Disabled;
            }
            Err(error) => {
                tracing::warn!(%key_path, error = %error, "读自启注册表失败");
                return AutostartState::Unknown;
            }
        };

        match opened.get_string(value_name) {
            Ok(_) => AutostartState::Enabled,
            Err(error) if error.code().0 == ERROR_FILE_NOT_FOUND => AutostartState::Disabled,
            Err(error) => {
                tracing::warn!(%key_path, %value_name, error = %error, "读自启值失败");
                AutostartState::Unknown
            }
        }
    }

    /// 注册表里存的命令行。
    ///
    /// **一律加引号**，不管路径里有没有空格。原因：装到 `C:\Program Files\...`
    /// 之后路径就带空格了，而 Windows 会在第一个空格处断开、把后面的部分当参数。
    /// 与其在"当前路径恰好没空格"时写一个以后会坏的值，不如永远写安全的那种 ——
    /// 带引号的无空格路径同样能被正确解析。
    fn command_line(exec: &Path) -> String {
        format!("\"{}\"", exec.to_string_lossy())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 测试专用键：跑完自己删掉，绝不碰真正的 `Run` 键。
        fn scratch_key() -> String {
            format!(
                "Software\\hbot-desktop-selftest\\{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            )
        }

        fn cleanup(key_path: &str) {
            let _ = windows_registry::CURRENT_USER.remove_tree(key_path);
        }

        #[test]
        fn enable_is_idempotent_and_disable_removes_exactly_one_value() {
            let key_path = scratch_key();
            let exec = Path::new(r"C:\Program Files\HBot\hbot.exe");

            assert_eq!(state_at(&key_path, AUTOSTART_ID), AutostartState::Disabled);

            set_run_value_at(&key_path, AUTOSTART_ID, exec, true).expect("开启自启");
            assert_eq!(state_at(&key_path, AUTOSTART_ID), AutostartState::Enabled);

            // 开两次：还是同一个值，不产生第二条。
            set_run_value_at(&key_path, AUTOSTART_ID, exec, true).expect("再开一次");

            let key = windows_registry::CURRENT_USER
                .open(&key_path)
                .expect("测试键必须能打开");
            let names = key
                .values()
                .expect("必须能枚举值")
                .map(|(name, _)| name)
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                vec![AUTOSTART_ID.to_string()],
                "只许有 hbot 这一个值"
            );
            assert_eq!(
                key.get_string(AUTOSTART_ID).expect("值必须读得回来"),
                r#""C:\Program Files\HBot\hbot.exe""#,
                "含空格的路径必须加引号"
            );

            set_run_value_at(&key_path, AUTOSTART_ID, exec, false).expect("关闭自启");
            assert_eq!(state_at(&key_path, AUTOSTART_ID), AutostartState::Disabled);

            // 关两次：第二次是 no-op，不许报错。
            set_run_value_at(&key_path, AUTOSTART_ID, exec, false).expect("再关一次");

            cleanup(&key_path);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub(super) fn set_enabled_for(exec: &Path, enabled: bool) -> io::Result<()> {
        let dir = autostart_dir()?.join("autostart");
        set_entry_at(&dir, exec, enabled)
    }

    pub(super) fn current() -> AutostartState {
        match autostart_dir().map(|dir| dir.join("autostart").join(DESKTOP_ENTRY_FILE)) {
            Ok(path) => match path.try_exists() {
                Ok(true) => AutostartState::Enabled,
                Ok(false) => AutostartState::Disabled,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "读自启文件失败");
                    AutostartState::Unknown
                }
            },
            Err(error) => {
                tracing::warn!(%error, "定位 ~/.config 失败");
                AutostartState::Unknown
            }
        }
    }

    fn set_entry_at(dir: &Path, exec: &Path, enabled: bool) -> io::Result<()> {
        let path = dir.join(DESKTOP_ENTRY_FILE);
        if enabled {
            write_file(&path, &desktop_entry_contents(exec))
        } else {
            remove_file_if_exists(&path)
        }
    }

    /// `$XDG_CONFIG_HOME` 优先，否则 `~/.config`。
    ///
    /// 注意：这是**配置**目录（自启项的家），与 `hbot_core::data_dir()` 的数据目录
    /// （SQLite/日志）不是一回事。
    fn autostart_dir() -> io::Result<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Ok(PathBuf::from(xdg));
            }
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME 未设置"))?;
        Ok(PathBuf::from(home).join(".config"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn enabling_writes_one_file_and_disabling_removes_it() {
            let dir = std::env::temp_dir().join(format!(
                "hbot-desktop-autostart-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            let exec = Path::new("/usr/local/bin/hbot");

            set_entry_at(&dir, exec, true).expect("开启自启");
            set_entry_at(&dir, exec, true).expect("重复开启必须幂等");
            let path = dir.join(DESKTOP_ENTRY_FILE);
            let text = std::fs::read_to_string(&path).expect("文件必须存在");
            assert_eq!(
                text.matches("Type=Application").count(),
                1,
                "重复开启不能追加第二段"
            );
            assert!(text.contains("Exec=/usr/local/bin/hbot"));

            set_entry_at(&dir, exec, false).expect("关闭自启");
            assert!(!path.exists(), "关闭后文件必须没了");
            set_entry_at(&dir, exec, false).expect("重复关闭必须幂等");

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub(super) fn set_enabled_for(exec: &Path, enabled: bool) -> io::Result<()> {
        let dir = launch_agents_dir()?;
        set_agent_at(&dir, exec, enabled)
    }

    pub(super) fn current() -> AutostartState {
        match launch_agents_dir().map(|dir| dir.join(LAUNCH_AGENT_FILE)) {
            Ok(path) => match path.try_exists() {
                Ok(true) => AutostartState::Enabled,
                Ok(false) => AutostartState::Disabled,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "读 LaunchAgent 失败");
                    AutostartState::Unknown
                }
            },
            Err(error) => {
                tracing::warn!(%error, "定位 ~/Library/LaunchAgents 失败");
                AutostartState::Unknown
            }
        }
    }

    fn set_agent_at(dir: &Path, exec: &Path, enabled: bool) -> io::Result<()> {
        let path = dir.join(LAUNCH_AGENT_FILE);
        if enabled {
            write_file(&path, &launch_agent_contents(exec))
        } else {
            remove_file_if_exists(&path)
        }
    }

    fn launch_agents_dir() -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME 未设置"))?;
        Ok(PathBuf::from(home).join("Library").join("LaunchAgents"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_is_idempotent_and_removable() {
            let dir =
                std::env::temp_dir().join(format!("hbot-desktop-agent-{}", std::process::id()));
            let exec = Path::new("/Applications/hbot.app/Contents/MacOS/hbot");

            set_agent_at(&dir, exec, true).expect("开启自启");
            set_agent_at(&dir, exec, true).expect("重复开启必须幂等");
            let path = dir.join(LAUNCH_AGENT_FILE);
            let text = std::fs::read_to_string(&path).expect("文件必须存在");
            assert_eq!(text.matches("<key>Label</key>").count(), 1);
            assert!(text.contains("com.hbot.desktop"));

            set_agent_at(&dir, exec, false).expect("关闭自启");
            assert!(!path.exists());

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// 其它平台（含测试用的宿主平台）：明确报"不支持"，不要假装成功。
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;

    pub(super) fn set_enabled_for(_exec: &Path, _enabled: bool) -> io::Result<()> {
        Err(io::Error::other("这个平台没有实现开机自启"))
    }

    pub(super) fn current() -> AutostartState {
        AutostartState::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_quotes_paths_with_reserved_characters() {
        // 没有保留字符：原样写。
        assert_eq!(
            exec_field(Path::new("/usr/local/bin/hbot")),
            "/usr/local/bin/hbot"
        );
        // 含空格：整段加引号。
        assert_eq!(
            exec_field(Path::new("/opt/my hbot/hbot")),
            "\"/opt/my hbot/hbot\""
        );
        // 含规范里的保留字符：引号内再转义。
        assert_eq!(
            exec_field(Path::new("/opt/a$b/\"hbot\"/x")),
            "\"/opt/a\\$b/\\\"hbot\\\"/x\""
        );
        // `%` 是字段码前缀，必须写成 `%%`。
        assert_eq!(
            exec_field(Path::new("/opt/100%/hbot")),
            "\"/opt/100%%/hbot\""
        );
    }

    #[test]
    fn desktop_entry_has_the_fields_desktop_spec_needs() {
        let text = desktop_entry_contents(Path::new("/usr/local/bin/hbot"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("Type=Application\n"));
        assert!(text.contains("Exec=/usr/local/bin/hbot\n"));
        assert!(text.contains("Terminal=false\n"));
        assert!(text.ends_with('\n'), "文件要以换行结尾");
    }

    #[test]
    fn launch_agent_is_valid_xml_with_the_expected_keys() {
        let text = launch_agent_contents(Path::new("/Applications/hbot.app/Contents/MacOS/hbot"));
        assert!(text.contains("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(text.contains("<key>Label</key>"));
        assert!(text.contains("com.hbot.desktop"));
        assert!(text.contains("<key>RunAtLoad</key>"));
        assert!(text.contains("<string>/Applications/hbot.app/Contents/MacOS/hbot</string>"));
        assert!(text.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn xml_escape_covers_the_five_entities() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }
}
