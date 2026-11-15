//! Tauri 桌面壳：一个进程里同时跑窗口与核心。
//!
//! 职责边界（`docs/02-architecture.md` §2）：本 crate 只做**窗口、启动装配与传输**。
//! 命令逻辑一律在 [`hbot_rpc::Dispatcher`] 后面，壳层**不实现任何命令分支**：桌面
//! 与后台（JSON-RPC / `hbot serve`）进的是同一个 `dispatch`，所以不存在"桌面能做
//! 的事后台做不了"。
//!
//! # 启动顺序（§2.1 的 M0 子集）
//!
//! 1. Tauri 按 `tauri.conf.json` 建窗口（`frontendDist` 指向 `apps/web/dist`）；
//! 2. `setup` 里确定数据目录（[`tauri::Manager::path`] → `app_data_dir()`），把
//!    [`DesktopHost`] 交给 Tauri 管理；
//! 3. 后台任务打开 [`AppState`]：SQLite 走 `spawn_blocking`，不占 UI 线程；失败则
//!    弹系统错误对话框并以退出码 1 结束，**绝不 panic**（库代码禁用 `unwrap`/`expect`）；
//! 4. 打开成功后给每个窗口起一条事件泵：订阅 `AppState` 的 [`EventBus`]，把事件原样
//!    `emit` 到 [`EVENT_CHANNEL`]。
//!
//! # 事件通道
//!
//! 通道名固定为 [`EVENT_CHANNEL`]，payload 是 [`hbot_rpc::Event`] 的 JSON
//! （`{"type":"agent.delta",…}`）。改名等于协议变更（`docs/02-architecture.md` §9.2），
//! web 侧（`apps/web/src/lib/ipc-tauri.ts`）的常量必须一起改。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbot_core::SystemClock;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::{AppState, EventBus};
use hbot_rpc::{ApiError, Command, Dispatcher, Response};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::OnceCell;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

/// web 侧订阅的事件通道名（`apps/web/src/lib/ipc-tauri.ts` 里的 `EVENT_CHANNEL`）。
///
/// 一进程一窗口的情况下它就是 `window.emit` 的事件名；协议常量，改名要同步 web 侧。
pub const EVENT_CHANNEL: &str = "hbot://event";

/// 核心未就绪时的错误码：`-32006` 内部错误（`docs/02-architecture.md` §9.4）。
///
/// 注意与"功能未实现"区分：后者由 `hbot-rpc` 产生，`message` 以 `尚未实现：` 开头；
/// 这里的 `message` 以 `桌面核心未就绪：` 开头，UI 应把它当普通错误展示。
const NOT_READY_CODE: i32 = -32006;

/// 已装配的核心：应用状态与共用它的分发器。
struct Core {
    /// 应用状态；事件总线的来源（每个窗口都从这里 `subscribe`）。
    state: Arc<AppState>,
    /// 与 CLI / 后台完全同一个分发器（`Dispatcher::with_state`）。
    dispatcher: Arc<Dispatcher>,
}

/// Tauri 管理的桌面宿主状态。
///
/// 打开过程是**惰性且只做一次**的：`setup` 之后由启动任务（或第一条命令）触发，
/// 结果（成功或失败原因）缓存在 [`OnceCell`] 里 —— 失败不会重试成死循环，也不会
/// 让每条命令都重新开一次 SQLite。
pub struct DesktopHost {
    /// 数据目录（DB、日志、缓存；来自 `app_data_dir()`）。
    data_dir: PathBuf,
    /// 核心装配结果。
    core: OnceCell<Result<Core, String>>,
}

impl DesktopHost {
    /// 用数据目录构造宿主；真正的打开发生在第一次 [`DesktopHost::core`]。
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            core: OnceCell::new(),
        }
    }

    /// 数据目录（装配 `AppState` 时用的那个）。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 取已装配的核心；第一次调用会把 `AppState` 打开在 blocking 线程上。
    ///
    /// 失败原因会被缓存并原样返回（含数据目录），供调用方拼可读错误。
    ///
    /// 只供 crate 内使用：返回值里的 [`Core`] 不对外，公开面只有
    /// [`dispatch_on`] 这条"窗口无关的命令层"。
    pub(crate) async fn core(&self) -> Result<&Core, String> {
        let opened = self
            .core
            .get_or_init(|| open_core(self.data_dir.clone()))
            .await;

        match opened {
            Ok(core) => Ok(core),
            Err(message) => Err(message.clone()),
        }
    }
}

/// 打开 `AppState` 并装上分发器。
///
/// SQLite 打开 + 迁移是阻塞 IO，必须离开 tokio reactor（`AGENTS.md` 的"阻塞工作
/// 不上 reactor"），所以整个 `AppState::open` 走 `spawn_blocking`。
async fn open_core(data_dir: PathBuf) -> Result<Core, String> {
    let blocking_dir = data_dir.clone();
    let opened = tokio::task::spawn_blocking(move || {
        AppState::open(&blocking_dir, Arc::new(SystemClock), Arc::new(UuidIds))
    })
    .await;

    let state = match opened {
        Ok(Ok(state)) => Arc::new(state),
        Ok(Err(error)) => {
            return Err(format!(
                "打开应用状态失败（{}）：{error}",
                data_dir.display()
            ));
        }
        Err(error) => return Err(format!("打开应用状态的阻塞任务失败：{error}")),
    };

    // 与 CLI 用的是同一个构造：`with_state` 顺带注入 clock / ids / events。
    let dispatcher = Arc::new(Dispatcher::new().with_state(Arc::clone(&state)));
    Ok(Core { state, dispatcher })
}

/// 命令层入口：转发给真正的分发器，自身不做任何命令分支。
///
/// 与窗口无关（参数里没有 Tauri 类型），所以单元测试可以直接调它，也保证测试
/// 覆盖的就是 Tauri 命令真实走的那段代码。
pub async fn dispatch_on(host: &DesktopHost, command: Command) -> Response {
    match host.core().await {
        Ok(core) => core.dispatcher.dispatch(command).await,
        Err(message) => Response::error(ApiError {
            code: NOT_READY_CODE,
            message: format!("桌面核心未就绪：{message}"),
            data: None,
        }),
    }
}

/// web 侧调用的唯一命令：`invoke('dispatch', { command })`。
///
/// 返回 [`Response`] 而不是 `Result<_, _>`：错误只有一条通道（§9.1），Tauri 的
/// `Err` 只留给传输层自身故障。
///
/// 刻意**不写 `pub`**：`#[tauri::command]` 对 `pub` 函数生成的宏带 `#[macro_export]`
/// （见 tauri-macros 的 `command/wrapper.rs`），而本文件就是 crate root —— 那会在
/// crate root 重绑一次同名宏，直接 E0255「`__cmd__dispatch` is defined multiple
/// times」。非 `pub` 不导出宏，`generate_handler![dispatch]` 在 crate 内照样解析得到。
#[tauri::command]
async fn dispatch(app: AppHandle, command: Command) -> Response {
    match app.try_state::<Arc<DesktopHost>>() {
        Some(host) => {
            // 先把 `Arc` 取出来，避免把 `State` 的借用带进 await。
            let host = Arc::clone(host.inner());
            dispatch_on(&host, command).await
        }
        // `setup` 一定 manage 了它；真到这里说明装配代码被改坏了，明确报错而不是 panic。
        None => Response::error(ApiError {
            code: NOT_READY_CODE,
            message: "桌面核心未装配：setup 没有注册 DesktopHost".to_owned(),
            data: None,
        }),
    }
}

/// 给一个窗口起一条事件泵：`EventBus` → `window.emit(EVENT_CHANNEL)`。
///
/// 每个窗口一条独立订阅（`broadcast::Receiver`），窗口关掉后接收端随任务结束
/// 释放，不会把事件堆在别处。订阅之前产生的事件拿不到（广播不补发），UI 首帧
/// 之后应当用快照命令补齐状态。
fn spawn_event_pump<R: Runtime>(window: tauri::WebviewWindow<R>, bus: Arc<EventBus>) {
    let label = window.label().to_owned();
    tauri::async_runtime::spawn(async move {
        let mut receiver = bus.subscribe();

        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if let Err(error) = window.emit(EVENT_CHANNEL, &event) {
                        // 窗口正在关闭时 emit 会失败，属于预期；只记日志，不中断其他窗口。
                        tracing::warn!(window = %label, %error, "推送事件失败");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // 订阅落后于生产速度：广播丢的是旧事件，不补发（§9.2 的流式语义）。
                    tracing::warn!(window = %label, skipped, "事件订阅落后，旧事件已丢弃");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }

        tracing::debug!(window = %label, "事件泵结束");
    });
}

/// 给当前所有窗口挂上事件泵。
///
/// Tauri 会先按 `tauri.conf.json` 建窗口再跑 `setup` 钩子，所以这里能拿到主窗口；
/// 运行时新建的窗口（M1 的多窗口/托盘菜单）需要自己调用这个组合。
fn attach_event_pumps<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>) {
    let bus = Arc::clone(state.events());
    let windows = app.webview_windows();

    if windows.is_empty() {
        tracing::warn!("没有窗口可推送事件：事件只会留在 EventBus 里");
        return;
    }

    for window in windows.into_values() {
        spawn_event_pump(window, Arc::clone(&bus));
    }
}

/// 启动期致命错误：先用系统对话框告诉用户，再写日志/stderr。
///
/// 不用 `tauri-plugin-dialog`：桌面壳不该为了失败路径多背一个插件依赖，而且
/// 对话框必须在"还没窗口/还不能开窗口"的时候也能弹出来。
fn report_fatal(message: &str) {
    tracing::error!(%message, "桌面壳启动失败");
    eprintln!("hbot 启动失败：{message}");
    show_error_dialog("hbot 无法启动", message);
}

/// Windows：`MessageBoxW`（无父窗口，阻塞到用户点"确定"）。
#[cfg(windows)]
fn show_error_dialog(title: &str, message: &str) {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }

    /// 红色错误图标。
    const MB_ICONERROR: u32 = 0x10;

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let text = wide(message);
    let caption = wide(title);

    // SAFETY: 两个指针都指向 `wide` 生成的、以 NUL 结尾的 UTF-16 缓冲区，且在调用
    // 期间生存；`MessageBoxW` 只读这两个缓冲，句柄传 null 表示无父窗口，返回码忽略。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

/// 非 Windows：没有跨平台对话框，靠 stderr 与日志（终端启动时可见）。
#[cfg(not(windows))]
fn show_error_dialog(title: &str, message: &str) {
    eprintln!("[{title}] {message}");
}

/// 初始化 stderr 日志。
///
/// 与 `hbot-cli` 的 `init_tracing` 同一套口径：默认 `info`，`RUST_LOG` 可覆盖。
/// 桌面进程平时没有控制台，日志主要在从终端启动（`cargo run -p hbot-desktop`）时可见；
/// 图形下真正要让人看见的失败走 [`report_fatal`] 的系统对话框，不能只靠日志。
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// 启动桌面壳，返回进程退出码。
///
/// 只在最外层返回 `i32`；内部的失败都变成可读错误 + 退出码 1，不做 unwrap/panic。
pub fn run() -> i32 {
    init_tracing();

    let outcome = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![dispatch])
        .setup(|app| {
            let handle = app.handle().clone();

            let data_dir = match handle.path().app_data_dir() {
                Ok(dir) => dir,
                Err(error) => {
                    let message = format!("无法确定应用数据目录：{error}");
                    report_fatal(&message);
                    return Err(message.into());
                }
            };
            tracing::info!(data_dir = %data_dir.display(), "桌面壳数据目录");

            let host = Arc::new(DesktopHost::new(data_dir));
            app.manage(Arc::clone(&host));

            // 核心装配放后台任务：SQLite 打开 + 迁移不能挡住窗口首帧（§2.1）。
            tauri::async_runtime::spawn(async move {
                match host.core().await {
                    Ok(core) => attach_event_pumps(&handle, &core.state),
                    Err(message) => {
                        let message = format!("hbot 启动失败：{message}");
                        // 对话框会阻塞调用线程，放到 blocking 池，别堵住 async worker。
                        let report = message.clone();
                        let _ = tokio::task::spawn_blocking(move || report_fatal(&report)).await;
                        // 直接以退出码 1 结束进程（Tauri 内部走 `std::process::exit`）。
                        handle.exit(1);
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!());

    match outcome {
        Ok(()) => 0,
        Err(error) => {
            report_fatal(&format!("桌面运行失败：{error}"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_rpc::AppInfo;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 临时目录：仓库不引 `tempfile`（它不在 workspace 依赖里），照
    /// `crates/hbot-rpc/src/app/test_support.rs` 的写法自己来。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hbot-desktop-test-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("测试要能建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn app_info_returns_real_data() {
        let dir = TempDir::new("app-info");
        let data_dir = dir.path().join("data");
        let host = DesktopHost::new(data_dir.clone());

        let response = dispatch_on(&host, Command::AppInfo).await;
        let data = match response {
            Response::Ok { data } => data.expect("AppInfo 必须带 data"),
            Response::Error { error } => panic!("AppInfo 不该失败：{error:?}"),
        };

        let info: AppInfo = serde_json::from_value(data).expect("data 应是 AppInfo");
        assert_eq!(info.name, "hbot");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.platform, std::env::consts::OS);
        // 两侧都规范化再比：`AppState::open` 可能把路径规范化（Windows 的大小写 / 短名），
        // 这里要钉的是「dataDir 来自壳层传入的那个目录」，不是字面量逐字节相等。
        assert_eq!(
            std::fs::canonicalize(Path::new(&info.data_dir)).expect("dataDir 必须存在"),
            std::fs::canonicalize(&data_dir).expect("测试目录必须存在"),
            "dataDir 必须是壳层传入的数据目录（不是 rpc 自己算的默认目录）"
        );
        assert!(
            data_dir.join("hbot.db").exists(),
            "AppState::open 应该建出 SQLite 文件"
        );
    }

    /// 未实现的命令必须原样变成 `-32006`，不能变成"成功但没有数据"。
    ///
    /// 断言的是 `docs/02-architecture.md` §9.4 的约定：`-32006` + `尚未实现：<命令>`。
    /// 用 `file_delete{toTrash:false}` 当样本：那是**刻意**未实现的一条（`hbot-workspace`
    /// 只提供移入回收站，见 `crates/hbot-rpc/src/app/commands/files.rs` 的注释）。
    /// 挑一条"暂时没做"的命令会让这个测试随实现进度过期，故意未实现的则不会。
    #[tokio::test]
    async fn unimplemented_command_returns_32006() {
        let dir = TempDir::new("unimplemented");
        let host = DesktopHost::new(dir.path().join("data"));

        let command = Command::FileDelete {
            path: "notes.txt".to_string(),
            to_trash: false,
        };

        match dispatch_on(&host, command).await {
            Response::Error { error } => {
                assert_eq!(error.code, NOT_READY_CODE);
                assert!(
                    error.message.starts_with("尚未实现：file_delete"),
                    "错误消息应带命令名，实际 {}",
                    error.message
                );
            }
            other => panic!("未实现命令不能返回成功，否则 UI 会当成空结果：{other:?}"),
        }
    }

    /// 数据目录不可用时不 panic：返回可读错误，UI 照常显示。
    #[tokio::test]
    async fn unopenable_data_dir_reports_not_ready() {
        let dir = TempDir::new("not-ready");
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").expect("占位文件必须能写");

        let host = DesktopHost::new(blocker);
        match dispatch_on(&host, Command::AppInfo).await {
            Response::Error { error } => {
                assert_eq!(error.code, NOT_READY_CODE);
                assert!(
                    error.message.starts_with("桌面核心未就绪："),
                    "错误消息应说明核心没起来，实际 {}",
                    error.message
                );
            }
            other => panic!("打不开数据目录时必须报错，实际 {other:?}"),
        }
    }

    /// 通道名是协议常量（§9.2 改名 = 协议变更），钉住它防止静默漂移。
    #[test]
    fn event_channel_is_the_documented_protocol_name() {
        assert_eq!(EVENT_CHANNEL, "hbot://event");
    }
}
