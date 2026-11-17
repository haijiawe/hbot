//! 无窗口的命令行入口。
//!
//! 存在的理由：**自启开关要能被脚本验证**。托盘菜单里的那个勾选项只能人手点，
//! 点完还得去注册表里翻，既不可重复也不进 CI。所以把同一段逻辑（[`crate::autostart`]
//! + [`crate::settings`]）也挂到命令行上，脚本就能"开 → 查 → 关 → 查"走一遍。
//!
//! 约定：`run_headless` 返回 `None` 表示"不是命令行请求，照常起 GUI"。返回 `Some(code)`
//! 表示已经处理完，进程应该用这个退出码退出（**不会**创建任何窗口 —— 这条路径刻意不碰
//! `hbot_rpc`，所以它可以在第一个实例正跑着的时候安全执行）。唯一例外是 `--logs-tail`：
//! 它会只读打开数据库，用来给崩溃上报脚本提供"日志表里能查到"的证据。
//!
//! 用法：
//!
//! ```text
//! hbot --set-autostart on|off|toggle   开/关开机自启（同时更新 desktop.json）
//! hbot --print-settings                打印偏好 + 系统里的实际自启状态（JSON）
//! hbot --tray-action show|autostart|quit
//!                                      （隐藏入口）把一次托盘菜单派发送给已运行的实例
//! hbot --logs-tail [n]                 打印最近 n 条 logs（默认 20，测试/诊断用）
//! hbot --help                          这段文字
//! ```
//!
//! `--tray-action` 刻意**不**在 `run_headless` 里处理：它必须先让单实例插件把 argv
//! 交给已经运行的实例，再由那个实例执行动作。真正的执行入口在 `lib.rs` 的单实例回调，
//! 与 `on_menu_event` 共用 [`crate::shell::handle_menu`] + [`crate::shell::dispatch_menu_action`]。
//! 它验证的是「菜单 id → 动作」的派发，**不是**「鼠标点中托盘图标」这一步。

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
    /// `--tray-action <菜单 id>`：交给已运行实例的隐藏入口（见模块文档）。
    TrayAction(String),
    /// `--logs-tail [n]`：只读打印最近 n 条 logs（默认 20），给崩溃上报脚本取证。
    LogsTail(usize),
    /// `--dpi-probe`：声明一次 Per-Monitor V2，把档位打成一行 JSON 后退出（不开窗口）。
    DpiProbe,
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
            "--tray-action" => {
                let Some(value) = args.get(index + 1) else {
                    return Invocation::Unknown(
                        "--tray-action 需要 show|autostart|quit 或完整菜单 id".into(),
                    );
                };
                if value.starts_with('-') {
                    return Invocation::Unknown(
                        "--tray-action 需要 show|autostart|quit 或完整菜单 id".into(),
                    );
                }
                index += 1;
                invocation = Invocation::TrayAction(value.clone());
            }
            "--dpi-probe" => invocation = Invocation::DpiProbe,
            "--logs-tail" => {
                let value = args.get(index + 1);
                match value {
                    Some(text) if !text.starts_with('-') => match text.parse::<usize>() {
                        Ok(limit) if limit > 0 => {
                            index += 1;
                            invocation = Invocation::LogsTail(limit);
                        }
                        _ => {
                            return Invocation::Unknown(
                                "--logs-tail 需要一个正整数（默认 20）".into(),
                            );
                        }
                    },
                    _ => invocation = Invocation::LogsTail(20),
                }
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
        Invocation::LogsTail(limit) => Some(print_logs_tail(limit)),
        Invocation::DpiProbe => Some(crate::dpi::probe()),
        // 留给单实例插件：先转发给已运行的实例，再在那边执行（见模块文档）。
        Invocation::TrayAction(_) => None,
    }
}

/// 从（第二个进程转交过来的）参数里找 `--tray-action` 的值。
///
/// 插件转交的是 `std::env::args_os()`，第一项是 exe 路径；这里**不假设位置**，
/// 从任意一项开始找，找到 flag 就取下一项。没有就返回 `None`（普通第二实例）。
pub fn tray_action_from_args(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--tray-action" {
            return args.get(index + 1).cloned();
        }
        index += 1;
    }
    None
}

const USAGE: &str = "\
hbot —— HBot 桌面壳

用法:
  hbot                                  启动图形界面（已在运行则把它带到前台）
  hbot --set-autostart on|off|toggle     开/关开机自启（同时更新 desktop.json）
  hbot --print-settings                  打印偏好与系统实际自启状态（JSON）
  hbot --tray-action show|autostart|quit 把一次托盘菜单动作交给已运行的实例（测试用）
  hbot --logs-tail [n]                   打印最近 n 条 logs（默认 20，测试/诊断用）
  hbot --dpi-probe                       声明 Per-Monitor V2 并打印档位（JSON，诊断用）
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

/// 只读打印最近若干条 logs。`bot_id = None` 是全项目视图（包含 Agent 行）。
///
/// 需要这个入口的理由：崩溃上报写的是 `logs` 表（`bot_id = NULL`、`source = 'agent'`），
/// 验证脚本要能直接断言"下一次启动后 `logs_tail` 真能查到"，而不是只看 stderr。
fn print_logs_tail(limit: usize) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("启动日志查询运行时失败：{error}");
            return 1;
        }
    };

    runtime.block_on(async {
        let host = crate::DesktopHost::new(hbot_core::data_dir());
        let response = crate::dispatch_on(
            &host,
            hbot_rpc::Command::LogsTail {
                bot_id: None,
                limit,
            },
        )
        .await;
        match response {
            hbot_rpc::Response::Ok { data } => {
                println!("{}", data.unwrap_or_else(|| serde_json::json!([])));
                0
            }
            hbot_rpc::Response::Error { error } => {
                eprintln!("logs_tail 失败：{error:?}");
                1
            }
        }
    })
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
    fn dpi_probe_is_headless_and_does_not_start_the_gui() {
        assert_eq!(parse(&args(&["--dpi-probe"])), Invocation::DpiProbe);
        // 与 `--logs-tail` 一样是**一次性**命令：`run_headless` 必须返回 `Some`，
        // 否则会既打印 JSON 又起一个 GUI（脚本里就成了"探测顺带开窗口"）。
        assert!(
            run_headless(&args(&["--dpi-probe"])).is_some(),
            "--dpi-probe 不能落到 GUI 分支"
        );
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
    fn tray_action_is_parsed_and_kept_out_of_headless() {
        assert_eq!(
            parse(&args(&["--tray-action", "show"])),
            Invocation::TrayAction("show".into())
        );
        assert_eq!(
            parse(&args(&["--tray-action", "desktop/tray/quit"])),
            Invocation::TrayAction("desktop/tray/quit".into())
        );
        assert_eq!(
            parse(&args(&["--tray-action"])),
            Invocation::Unknown("--tray-action 需要 show|autostart|quit 或完整菜单 id".into())
        );
    }

    #[test]
    fn tray_action_from_forwarded_args_does_not_assume_position() {
        // 单实例插件转发的是 args_os()，第一项是 exe 路径，flag 永远不在第一个。
        let forwarded = args(&[
            "C:\\Program Files\\hbot\\hbot-desktop.exe",
            "--tray-action",
            "autostart",
        ]);
        assert_eq!(
            tray_action_from_args(&forwarded),
            Some("autostart".to_string())
        );
        assert_eq!(tray_action_from_args(&args(&["hbot.exe"])), None);
    }

    #[test]
    fn logs_tail_is_parsed_with_an_optional_positive_limit() {
        assert_eq!(parse(&args(&["--logs-tail"])), Invocation::LogsTail(20));
        assert_eq!(
            parse(&args(&["--logs-tail", "50"])),
            Invocation::LogsTail(50)
        );
        assert_eq!(
            parse(&args(&["--logs-tail", "abc"])),
            Invocation::Unknown("--logs-tail 需要一个正整数（默认 20）".into())
        );
        assert_eq!(
            parse(&args(&["--logs-tail", "0"])),
            Invocation::Unknown("--logs-tail 需要一个正整数（默认 20）".into())
        );
    }

    #[test]
    fn settings_path_follows_the_data_dir_not_a_hand_rolled_path() {
        let expected = hbot_core::data_dir().join(crate::settings::SETTINGS_FILE);
        assert_eq!(expected_settings_path(), expected);
    }
}
