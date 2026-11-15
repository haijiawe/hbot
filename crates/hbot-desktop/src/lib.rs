//! Tauri 桌面壳。
//!
//! 这一层只做**宿主**该做的事：开窗口、单实例、托盘、自动更新、把 UI 的
//! `dispatch(command)` 转给应用层。业务规则一律不写在这里
//! （`docs/03-directory.md` §2.1 “desktop 无业务规则”）。
//!
//! 当前是 M0 骨架：只有 `AppInfo` 有实现，其余命令返回“尚未实现”，
//! 目的是先把窗口与 IPC 通路打通，S2 才有东西可测。

use hbot_rpc::{ApiError, Command, Response};
use serde_json::json;

/// UI 调用应用层的唯一入口。
///
/// 返回 [`Response`]（含错误变体）而不是 `Result`：UI 只有一种错误形状要解析，
/// 见 `docs/02-architecture.md` §9.1。
#[tauri::command]
fn dispatch(command: Command) -> Response {
    match command {
        Command::AppInfo => Response::with_data(json!({
            "name": "hbot",
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "dataDir": null,
        })),
        other => Response::error(ApiError {
            code: -32006,
            message: format!("尚未实现：{other:?}"),
            data: None,
        }),
    }
}

/// 启动桌面应用；返回进程退出码。
pub fn run() -> i32 {
    let result = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![dispatch])
        .run(tauri::generate_context!());

    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("桌面启动失败：{error}");
            1
        }
    }
}
