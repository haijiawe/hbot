//! 无窗口的命令行入口。
//!
//! 存在的理由：**自启开关要能被脚本验证**。托盘菜单里的那个勾选项只能人手点，
//! 点完还得去注册表里翻，既不可重复也不进 CI。所以把同一段逻辑（[`crate::autostart`]
//! + [`crate::settings`]）也挂到命令行上，脚本就能"开 → 查 → 关 → 查"走一遍。
//!
//! 约定：`run_headless` 返回 `None` 表示"不是命令行请求，照常起 GUI"。返回 `Some(code)`
//! 表示已经处理完，进程应该用这个退出码退出（**不会**创建任何窗口，也不会打开数据库 ——
//! 这条路径刻意不碰 `hbot_rpc`，所以它可以在第一个实例正跑着的时候安全执行）。
//!
//! 用法：
//!
//! ```text
//! hbot --set-autostart on|off|toggle   开/关开机自启（同时更新 desktop.json）
//! hbot --print-settings                打印偏好 + 系统里的实际自启状态（JSON）
//! hbot --help                          这段文字
//! ```

use std::path::PathBuf;

use crate::autostart::{self, AutostartState};
use crate::settings::DesktopSettings;

/// 解析出来的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `--set-autostart on|off|toggle`。
    SetAutostart(Option<bool>),
    /// `--print-settings`。
    PrintSettings,
    /// `--help` / `-h`。
    Help,
    /// 没有任何参数：起 GUI。
    Launch,
    /// 不认识的长参数：报错退出，不要静默起 GUI（打错一个字母就不知道该发生了）。
    Unknown(String),
}

/// 解析命令行（纯函数，好测）。
pub fn parse(args: &[String]) -> Invocation {
    let mut invocation = Invocation::Launch;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--help" | "-h" => invocation = Invocation::Help,
            "--print-settings" => invocation = Invocation::PrintSettings,
            "--set-autostart" => {
                let value = args.get(index + 1).map(String::as_str);
                invocation = match value {
                    Some("on") | Some("true") | Some("1") => {
                        index += 1;
                        Invocation::SetAutostart(Some(true))
                    }
                    Some("off") | Some("false") | Some("0") => {
                        index += 1;
                        Invocation::SetAutostart(Some(false))
                    }
                    Some("toggle") => {
                        index += 1;
                        Invocation::SetAutostart(None)
                    }
                    _ => return Invocation::Unknown("--set-autostart 需要 on|off|toggle".into()),
                };
            }
            other if other.starts_with('-') => return Invocation::Unknown(other.to_string()),
            // 非参数（比如安装器塞进来的路径）：GUI 自己会忽略，别抢它的活。
            _ => {}
        }
        index += 1;
    }
    invocation
}

/// 处理命令行请求。`None` = 继续起 Tauri。
pub fn run_headless(args: &[String]) -> Option<i32> {
    let invocation = parse(args);
    match invocation {
        Invocation::Launch => None,
        Invocation::Help => {
            print!("{USAGE}");
            Some(0)
        }
        Invocation::Unknown(bad) => {
            eprintln!("无法识别的参数：{bad}\n\n{USAGE}");
            Some(2)
        }
        Invocation::PrintSettings => Some(print_settings()),
        Invocation::SetAutostart(desired) => Some(set_autostart(desired)),
    }
}

const USAGE: &str = "\
hbot —— HBot 桌面壳

用法:
  hbot                                  启动图形界面（已在运行则把它带到前台）
  hbot --set-autostart on|off|toggle     开/关开机自启（同时更新 desktop.json）
  hbot --print-settings                  打印偏好与系统实际自启状态（JSON）
  hbot --help                            显示这段帮助

说明:
  数据目录由 hbot_core::data_dir() 决定，偏好文件是其中的 desktop.json。
  开机自启只写当前用户（Windows: HKCU\\...\\Run；Linux: ~/.config/autostart；
  macOS: ~/Library/LaunchAgents），不需要管理员权限。
";

fn set_autostart(desired: Option<bool>) -> i32 {
    let data_dir = hbot_core::data_dir();
    let mut settings = DesktopSettings::load(&data_dir);
    let original = settings.autostart;
    let target = desired.unwrap_or(!original);

    // 先系统后文件：文件写失败时系统状态会被回滚，避免"偏好开着、系统没开"。
    if let Err(error) = autostart::set_enabled(target) {
        eprintln!("设置开机自启失败：{error}");
        return 1;
    }

    settings.autostart = target;
    if let Err(error) = settings.save(&data_dir) {
        eprintln!("保存 desktop.json 失败：{error}");
        if let Err(revert) = autostart::set_enabled(original) {
            eprintln!("回滚系统自启状态也失败了：{revert}");
        }
        return 1;
    }

    println!(
        "{}",
        serde_json::json!({
            "autostart": target,
            "systemAutostart": state_name(autostart::current()),
            "dataDir": data_dir.to_string_lossy(),
        })
    );
    // 提示一句人能看懂的结果，脚本只用上面的 JSON。
    eprintln!("开机自启已{}", if target { "开启" } else { "关闭" });
    0
}

fn print_settings() -> i32 {
    let data_dir = hbot_core::data_dir();
    let settings = DesktopSettings::load(&data_dir);
    println!(
        "{}",
        serde_json::json!({
            "settings": settings,
            "systemAutostart": state_name(autostart::current()),
            "dataDir": data_dir.to_string_lossy(),
            "settingsPath": DesktopSettings::path(&data_dir).to_string_lossy(),
            "execPath": autostart::exec_path().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
        })
    );
    0
}

fn state_name(state: AutostartState) -> &'static str {
    match state {
        AutostartState::Enabled => "enabled",
        AutostartState::Disabled => "disabled",
        AutostartState::Unknown => "unknown",
    }
}

/// 数据目录的期望值（给测试和文档引用，避免到处手拼路径）。
pub fn expected_settings_path() -> PathBuf {
    DesktopSettings::path(&hbot_core::data_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn no_arguments_means_launch_the_gui() {
        assert_eq!(parse(&[]), Invocation::Launch);
    }

    #[test]
    fn set_autostart_accepts_the_three_spellings() {
        assert_eq!(
            parse(&args(&["--set-autostart", "on"])),
            Invocation::SetAutostart(Some(true))
        );
        assert_eq!(
            parse(&args(&["--set-autostart", "off"])),
            Invocation::SetAutostart(Some(false))
        );
        assert_eq!(
            parse(&args(&["--set-autostart", "toggle"])),
            Invocation::SetAutostart(None)
        );
    }

    #[test]
    fn set_autostart_without_a_value_is_a_usage_error() {
        assert_eq!(
            parse(&args(&["--set-autostart"])),
            Invocation::Unknown("--set-autostart 需要 on|off|toggle".into())
        );
        assert_eq!(
            parse(&args(&["--set-autostart", "maybe"])),
            Invocation::Unknown("--set-autostart 需要 on|off|toggle".into())
        );
    }

    #[test]
    fn unknown_flags_do_not_silently_start_the_gui() {
        assert_eq!(
            parse(&args(&["--set-autostartt", "on"])),
            Invocation::Unknown("--set-autostartt".into())
        );
    }

    #[test]
    fn non_flag_arguments_are_ignored() {
        // 安装器/快捷方式可能塞路径进来，那不是我们的参数，交给 GUI。
        assert_eq!(parse(&args(&["C:\\some\\path"])), Invocation::Launch);
    }

    #[test]
    fn print_settings_and_help_are_recognized() {
        assert_eq!(
            parse(&args(&["--print-settings"])),
            Invocation::PrintSettings
        );
        assert_eq!(parse(&args(&["--help"])), Invocation::Help);
        assert_eq!(parse(&args(&["-h"])), Invocation::Help);
    }

    #[test]
    fn settings_path_follows_the_data_dir_not_a_hand_rolled_path() {
        let expected = hbot_core::data_dir().join(crate::settings::SETTINGS_FILE);
        assert_eq!(expected_settings_path(), expected);
    }
}
