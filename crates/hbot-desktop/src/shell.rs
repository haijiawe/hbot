//! 桌面壳的"窗口之外"那一层：托盘、关窗到托盘、优雅退出、自启开关。
//!
//! 职责边界（与 `AGENTS.md` 的 crate 边界一致）：这里只**触发**行为，不做业务。
//! 停机器人走 `hbot_rpc` 已有的 `BotRegistry::stop_all()`，写库走 `hbot_rpc` 的
//! `AppState::append_log()`（壳不直接依赖 hbot-store），没有第二条关闭路径。
//!
//! 关键事实（决定实现形状，别改回去）：
//!
//! - **托盘图标由 `tauri.conf.json` 的 `app.trayIcon` 在 `Builder::build()` 里创建**，
//!   所以这里**不能**再建一个，否则任务栏上会出现两个图标。这里只 `tray_by_id("main")`
//!   拿到它、挂菜单。
//! - 菜单/托盘事件是**全局**的（`muda::MenuEvent` 由事件循环转发），所以菜单项用
//!   应用级 `Builder::on_menu_event` 处理即可，不需要按托盘各挂一个 handler。
//! - 关窗到托盘的做法是 `WindowEvent::CloseRequested` 里 `api.prevent_close()` + `hide()`。
//!   窗口对象不销毁，进程继续跑，bot 继续跑。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use crate::DesktopHost;
use crate::settings::DesktopSettings;

/// 主窗口标签（`tauri.conf.json` 里的 `app.windows[0]` 没写 `label`，默认就是 `main`）。
pub const MAIN_WINDOW: &str = "main";

/// 托盘图标 id（与 `tauri.conf.json` 的 `app.trayIcon.id` 必须一致）。
pub const TRAY_ID: &str = "main";

/// 托盘提示文字。
const TRAY_TOOLTIP: &str = "hbot（关闭窗口后仍在后台运行）";

/// 菜单项 id：显示主窗口。
pub const MENU_SHOW: &str = "desktop/tray/show";
/// 菜单项 id：开机自启（勾选项）。
pub const MENU_AUTOSTART: &str = "desktop/tray/autostart";
/// 菜单项 id：退出（真退出）。
pub const MENU_QUIT: &str = "desktop/tray/quit";

/// 托盘菜单 id 对应的动作（纯数据，不碰窗口）。
///
/// 存在的理由：真实鼠标点击系统托盘在 Windows 上不可靠，但「菜单 id → 动作」的
/// 派发必须能被脚本重复验证。所以把这一层抽成纯函数：真实点击走的
/// [`on_menu_event`] 和隐藏入口 `--tray-action` 都先过 [`handle_menu`]，再走同一个
/// [`dispatch_menu_action`]。**这只验证派发，不验证「鼠标点中托盘图标」本身。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// 显示并聚焦主窗口。
    Show,
    /// 翻转开机自启（系统 + `desktop.json`）。
    ToggleAutostart,
    /// 停止机器人并优雅退出。
    Quit,
    /// 不是本应用的托盘菜单项。
    Unhandled,
}

impl MenuAction {
    /// 稳定的字符串形式，只用于日志与脚本断言（不是菜单 id）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::ToggleAutostart => "toggleAutostart",
            Self::Quit => "quit",
            Self::Unhandled => "unhandled",
        }
    }
}

/// 菜单 id → 动作。纯函数；三个 id 的映射由单元测试钉住。
pub fn handle_menu(id: &MenuId) -> MenuAction {
    match id.0.as_str() {
        MENU_SHOW => MenuAction::Show,
        MENU_AUTOSTART => MenuAction::ToggleAutostart,
        MENU_QUIT => MenuAction::Quit,
        _ => MenuAction::Unhandled,
    }
}

/// `--tray-action` 的取值 → 菜单 id。
///
/// 接受三个短名（`show` / `autostart` / `quit`，方便脚本写）与完整菜单 id
/// （与真实事件里的 id 完全一致，方便证明走的是同一张表）。未知值原样包成
/// `MenuId`，交给 [`handle_menu`] 判为 [`MenuAction::Unhandled`] 并留下告警。
pub fn menu_id_for_cli(value: &str) -> MenuId {
    match value {
        "show" => MenuId::new(MENU_SHOW),
        "autostart" => MenuId::new(MENU_AUTOSTART),
        "quit" => MenuId::new(MENU_QUIT),
        other => MenuId::new(other),
    }
}

/// 执行一个托盘菜单动作。
///
/// `source` / `menu_id` 只进日志：脚本按 `source="cli"` 断言这个动作确实由隐藏入口
/// 触发（而不是别的东西碰巧把窗口显示了），同时保留真实点击的 `source="tray-menu"`。
pub(crate) fn dispatch_menu_action(
    app: &AppHandle,
    action: MenuAction,
    source: &str,
    menu_id: &str,
) {
    tracing::info!(source, menu_id, action = action.as_str(), "托盘动作派发");
    match action {
        MenuAction::Show => show_main_window(app),
        MenuAction::ToggleAutostart => toggle_autostart(app),
        MenuAction::Quit => request_quit(app),
        MenuAction::Unhandled => tracing::warn!(source, menu_id, "未处理的托盘菜单项"),
    }
}

/// 托盘/窗口层的运行时状态。
struct ShellState {
    data_dir: PathBuf,
    settings: Mutex<DesktopSettings>,
    /// 退出只许走一次（托盘点了退出、用户又点了一次关窗，不能并发关两遍）。
    quitting: AtomicBool,
}

/// 装好托盘菜单与关窗行为。在 `setup` 里调用（此时配置里声明的窗口已经创建好了）。
pub fn install(app: &AppHandle, settings: DesktopSettings, data_dir: PathBuf) -> tauri::Result<()> {
    let menu = build_menu(app, &settings)?;
    attach_menu(app, menu);

    app.manage(ShellState {
        data_dir,
        settings: Mutex::new(settings),
        quitting: AtomicBool::new(false),
    });

    register_close_behavior(app);
    Ok(())
}

/// 建菜单。勾选状态来自**已保存的偏好**，不是系统状态 —— 两者的差异由
/// `setup` 里的对齐逻辑处理（见 `lib.rs`）。
fn build_menu(app: &AppHandle, settings: &DesktopSettings) -> tauri::Result<Menu<tauri::Wry>> {
    let show = MenuItem::with_id(app, MENU_SHOW, "显示主窗口", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let autostart = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        "开机自启",
        true,
        settings.autostart,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    Menu::with_items(app, &[&show, &separator, &autostart, &quit])
}

/// 把菜单挂到托盘上。
///
/// 正常路径：`tauri.conf.json` 已经声明了 `trayIcon`，`Builder::build()` 建好了图标，
/// 这里只需要 `set_menu`。兜底路径：配置被删掉/改成没有图标时自己建一个，
/// 让"关窗到托盘"不至于把窗口藏到无处可寻 —— 那样用户只能杀进程。
fn attach_menu(app: &AppHandle, menu: Menu<tauri::Wry>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_menu(Some(menu)) {
            tracing::error!(%error, "挂托盘菜单失败");
        }
        let _ = tray.set_tooltip(Some(TRAY_TOOLTIP));
        // 这行是"托盘真的建出来了"的唯一机器可读证据（图标是否出现在通知区
        // 是外壳的事，这里断言不了）。验证脚本按 `source=config` 匹配。
        tracing::info!(source = "config", "托盘已就绪，菜单已挂上");
        return;
    }

    tracing::warn!("tauri.conf.json 没有声明 trayIcon，退回到代码里创建托盘");
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip(TRAY_TOOLTIP)
        .show_menu_on_left_click(false);
    match app.default_window_icon().cloned() {
        Some(icon) => builder = builder.icon(icon),
        None => tracing::warn!("没有默认窗口图标，托盘可能不可见"),
    }
    if let Err(error) = builder.build(app) {
        tracing::error!(%error, "创建托盘失败：关闭窗口后将无法从托盘恢复");
    }
}

/// 关窗 → 收进托盘（默认）；`closeToTray=false` 时 → 走优雅退出。
fn register_close_behavior(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        tracing::warn!(window = MAIN_WINDOW, "找不到主窗口，关窗行为未装上");
        return;
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            // 一律拦下：窗口销毁了就再也拿不回来，而后台 bot 还要继续跑。
            api.prevent_close();
            if close_to_tray(&handle) {
                let Some(window) = handle.get_webview_window(MAIN_WINDOW) else {
                    return;
                };
                if let Err(error) = window.hide() {
                    tracing::error!(%error, "隐藏窗口失败");
                }
            } else {
                request_quit(&handle);
            }
        }
    });
}

/// 当前偏好是不是"关窗收到托盘"。
fn close_to_tray(app: &AppHandle) -> bool {
    app.try_state::<ShellState>()
        .map(|state| {
            state
                .settings
                .lock()
                .map(|settings| settings.close_to_tray)
                // 锁被毒化时按默认值走：宁可藏起来也不要莫名其妙退出（数据更重要）。
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

/// 把主窗口带到前台（单实例第二次启动、点托盘"显示主窗口"、左键点图标都走这里）。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        tracing::warn!(window = MAIN_WINDOW, "找不到主窗口，无法带到前台");
        return;
    };
    if let Err(error) = window.show() {
        tracing::error!(%error, "显示窗口失败");
    }
    if let Err(error) = window.unminimize() {
        tracing::error!(%error, "取消最小化失败");
    }
    if let Err(error) = window.set_focus() {
        tracing::error!(%error, "聚焦窗口失败");
    }
}

/// 菜单点击分发（由 `lib.rs` 的 `Builder::on_menu_event` 转进来）。
///
/// 这里只负责把真实事件翻译成 [`MenuAction`]，执行在 [`dispatch_menu_action`] ——
/// 与 `--tray-action` 隐藏入口共用同一段派发逻辑。
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id();
    dispatch_menu_action(app, handle_menu(id), "tray-menu", id.0.as_str());
}

/// 托盘图标事件：左键单击 = 显示主窗口（右键出菜单是平台默认行为）。
pub fn on_tray_icon_event(app: &AppHandle, event: &TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        show_main_window(app);
    }
}

/// 切换开机自启：先动系统，成功了才写偏好并更新勾选；失败就回滚并弹框。
///
/// 顺序很重要：**先系统后文件**。反过来的话，"注册表写失败但偏好已经写成 true"，
/// 下次启动会以为开着，而系统里其实没有 —— 用户看到勾选是亮的却不开机自启。
fn toggle_autostart(app: &AppHandle) {
    let Some(state) = app.try_state::<ShellState>() else {
        crate::show_error_dialog("hbot", "桌面端状态没初始化，无法切换开机自启");
        return;
    };

    let current = state
        .settings
        .lock()
        .map(|settings| *settings)
        .unwrap_or_default();
    let desired = !current.autostart;

    if let Err(error) = crate::autostart::set_enabled(desired) {
        tracing::error!(%error, desired, "切换开机自启失败");
        crate::show_error_dialog("hbot", &format!("切换开机自启失败：{error}"));
        return;
    }

    let mut updated = current;
    updated.autostart = desired;
    if let Err(error) = updated.save(&state.data_dir) {
        tracing::error!(%error, "保存桌面偏好失败，回滚自启状态");
        // 文件写不进去就别让系统状态和偏好分叉：把系统状态改回去。
        if let Err(revert) = crate::autostart::set_enabled(current.autostart) {
            tracing::error!(%revert, "回滚开机自启也失败了");
        }
        crate::show_error_dialog("hbot", &format!("保存 desktop.json 失败：{error}"));
        return;
    }

    if let Ok(mut settings) = state.settings.lock() {
        *settings = updated;
    }
    refresh_menu(app, &updated);
    tracing::info!(autostart = desired, "开机自启已切换");
}

/// 重建菜单（勾选状态变了就重建，避免把非 `Send` 的菜单句柄塞进全局状态）。
fn refresh_menu(app: &AppHandle, settings: &DesktopSettings) {
    let menu = match build_menu(app, settings) {
        Ok(menu) => menu,
        Err(error) => {
            tracing::error!(%error, "重建托盘菜单失败");
            return;
        }
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_menu(Some(menu)) {
            tracing::error!(%error, "更新托盘菜单失败");
        }
    }
}

/// 请求优雅退出（幂等：重复点只会生效一次）。
pub fn request_quit(app: &AppHandle) {
    let Some(state) = app.try_state::<ShellState>() else {
        tracing::warn!("桌面端状态没初始化，直接退出");
        app.exit(0);
        return;
    };
    if state.quitting.swap(true, Ordering::SeqCst) {
        tracing::debug!("退出已经在进行中");
        return;
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        shutdown(&handle).await;
        handle.exit(0);
    });
}

/// 优雅关闭：停 bot → 排空写队列（日志/DB 落到磁盘）→ 由调用方 `exit`。
///
/// 复用现有路径，不新写一条：
/// - 停 bot：`AppState::shutdown()`（逐个走 `stop_with_reason`，`onStop` 收到 `shutdown`；每个 bot 等 5 秒，
///   超时才 abort，见 `crates/hbot-rpc/src/app/bots.rs` 的 `STOP_GRACE`）。
/// - 排空写队列：底下的 `Store` 是"单写线程 + 有界队列 + 提交阻塞到回执"，
///   所以一次成功的 `AppState::append_log` 就是写屏障 —— 它返回时，它之前排队的写都已经落库。
///   （`Store` 没有公开的 `flush()`/`close()`；`Shutdown` 只由它自己的 `Drop` 发。）
async fn shutdown(app: &AppHandle) {
    let host: Option<Arc<DesktopHost>> = app
        .try_state::<Arc<DesktopHost>>()
        .map(|state| Arc::clone(&state));

    let Some(host) = host else {
        tracing::warn!("内核还没装配好，没有需要停的机器人");
        return;
    };

    let core = match host.core().await {
        Ok(core) => core,
        Err(error) => {
            tracing::warn!(%error, "退出时打不开内核，跳过机器人收尾");
            return;
        }
    };

    tracing::info!("桌面端开始优雅退出：先停机器人");
    core.state.shutdown().await;

    // 写日志表走 `AppState::append_log`（壳不直接依赖 hbot-store）。
    let state = Arc::clone(&core.state);
    let barrier = tokio::task::spawn_blocking(move || {
        let bots = match state.store().list_bots() {
            Ok(bots) => bots,
            Err(error) => {
                tracing::warn!(%error, "退出时列机器人失败，写屏障没打成");
                return;
            }
        };
        let Some(bot) = bots.first() else {
            tracing::info!("空库（没有机器人），写队列无内容可排");
            return;
        };
        match state.append_log(bot.id, "info", "桌面端退出：机器人已停，写队列已排空")
        {
            Ok(()) => tracing::info!("写队列已排空（本次 append_log 是屏障）"),
            Err(error) => tracing::warn!(%error, "退出写屏障失败"),
        }
    })
    .await;

    if let Err(error) = barrier {
        tracing::warn!(%error, "退出写屏障的任务挂了");
    }
    tracing::info!("桌面端优雅退出完成");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_are_distinct_and_namespaced() {
        // 三个 id 必须互不相同，否则菜单点击会串台。
        assert_ne!(MENU_SHOW, MENU_AUTOSTART);
        assert_ne!(MENU_SHOW, MENU_QUIT);
        assert_ne!(MENU_AUTOSTART, MENU_QUIT);
        for id in [MENU_SHOW, MENU_AUTOSTART, MENU_QUIT] {
            assert!(id.starts_with("desktop/tray/"), "{id} 要有命名空间前缀");
        }
    }

    #[test]
    fn tray_id_matches_the_window_label_convention() {
        // 窗口标签与托盘 id 都用 "main"：`tray_by_id` 与 `get_webview_window` 依赖它。
        assert_eq!(TRAY_ID, "main");
        assert_eq!(MAIN_WINDOW, "main");
    }

    #[test]
    fn the_three_menu_ids_map_to_their_actions() {
        assert_eq!(handle_menu(&MenuId::new(MENU_SHOW)), MenuAction::Show);
        assert_eq!(
            handle_menu(&MenuId::new(MENU_AUTOSTART)),
            MenuAction::ToggleAutostart
        );
        assert_eq!(handle_menu(&MenuId::new(MENU_QUIT)), MenuAction::Quit);
        assert_eq!(
            handle_menu(&MenuId::new("desktop/tray/unknown")),
            MenuAction::Unhandled
        );
    }

    #[test]
    fn cli_short_names_and_full_ids_resolve_to_the_same_menu_ids() {
        // 隐藏入口既能写短名，也能写真实事件里的完整 id。
        for (short, full) in [
            ("show", MENU_SHOW),
            ("autostart", MENU_AUTOSTART),
            ("quit", MENU_QUIT),
        ] {
            assert_eq!(menu_id_for_cli(short), MenuId::new(full));
            assert_eq!(menu_id_for_cli(full), MenuId::new(full));
        }
        assert_eq!(
            menu_id_for_cli("nope"),
            MenuId::new("nope"),
            "未知值原样保留，由 handle_menu 判为 Unhandled"
        );
    }
}
