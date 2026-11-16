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

pub mod autostart;
pub mod cli;
pub mod crash;
pub mod settings;
pub mod shell;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hbot_core::SystemClock;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::{AppState, EventBus};
use hbot_rpc::{ApiError, Command, Dispatcher, Response};
use tauri::{AppHandle, Emitter, Listener, Manager, Runtime};
use tokio::sync::OnceCell;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

/// 启动分段计时（`HBOT_STARTUP_TRACE=1` 时开启，默认零开销）。
///
/// `hbot-store` 里有同一份（rpc 侧用它记库内阶段）。桌面壳不能依赖 `hbot-store`
/// （`xtask/src/deps.rs` 的依赖白名单只有 `hbot-core` / `hbot-rpc`），所以这里内联
/// 一份最小实现：所有事件都从 [`run`] 入口的同一个 `Instant` 起算，输出
/// `hbot-startup ev=<event> t_ms=<f64> unix_ms=<u64> [k=v ...]` 到 stderr。
mod startup_trace {
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    static ENABLED: OnceLock<bool> = OnceLock::new();
    static ORIGIN: OnceLock<Instant> = OnceLock::new();

    /// 是否开启启动计时：只有 `HBOT_STARTUP_TRACE=1` 才开启。
    pub fn enabled() -> bool {
        *ENABLED.get_or_init(|| std::env::var("HBOT_STARTUP_TRACE").as_deref() == Ok("1"))
    }

    fn origin() -> &'static Instant {
        ORIGIN.get_or_init(Instant::now)
    }

    /// 打一个时间点事件（相对 [`run`] 入口的毫秒数）。
    pub fn point(event: &str, detail: &str) {
        if !enabled() {
            return;
        }
        let t_ms = origin().elapsed().as_secs_f64() * 1000.0;
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        if detail.is_empty() {
            eprintln!("hbot-startup ev={event} t_ms={t_ms:.3} unix_ms={unix_ms}");
        } else {
            eprintln!("hbot-startup ev={event} t_ms={t_ms:.3} unix_ms={unix_ms} {detail}");
        }
    }

    /// 计时开启时返回当前 `Instant`，供需要算时长的调用点使用。
    pub fn now() -> Option<Instant> {
        enabled().then(Instant::now)
    }
}

/// 只在 `HBOT_STARTUP_TRACE=1` 时注入页面的首帧标记脚本（不进入前端产物）。
///
/// 页面加载完成后读取 Navigation Timing 与 paint timing，再等两个 `requestAnimationFrame`
/// 把标记经 `plugin:event|emit` 送回 Rust 侧（事件名 [`STARTUP_MARK_EVENT`]）。
/// 测量专用：失败（例如 ACL 拒绝）只影响本脚本，不影响页面。
const STARTUP_MARK_SCRIPT: &str = r#"
(function () {
  var report = function (phase) {
    try {
      var nav = performance.getEntriesByType('navigation')[0];
      var paints = performance.getEntriesByType('paint') || [];
      var paintOf = function (name) {
        for (var i = 0; i < paints.length; i += 1) {
          if (paints[i].name === name) { return paints[i].startTime; }
        }
        return null;
      };
      var firstPaint = paintOf('first-paint');
      var payload = {
        phase: phase,
        time_origin: performance.timeOrigin,
        nav_start: nav ? nav.startTime : null,
        dom_interactive: nav ? nav.domInteractive : null,
        load_end: nav ? nav.loadEventEnd : null,
        first_paint: firstPaint,
        fcp: paintOf('first-contentful-paint'),
        now: performance.now()
      };
      // 外部探针的兜底信号（不依赖 IPC 权限）：窗口标题里带 first-paint 的绝对时间
      // （epoch 毫秒，= timeOrigin + 相对毫秒），探针可直接换算到进程启动基准。
      // 只在 `HBOT_STARTUP_TRACE=1` 时注入，正常启动不会执行这段脚本。
      document.title = 'hbot-frame@' + Math.round(performance.timeOrigin + (firstPaint === null ? performance.now() : firstPaint));
      var internals = window.__TAURI_INTERNALS__;
      if (internals && internals.invoke) {
        // 走应用命令（不是 plugin:），不受 ACL 影响。
        internals.invoke('startup_mark', { payload: payload }).then(function () {}, function () {});
      }
    } catch (error) {
      // 测量脚本，任何失败都不该影响页面。
    }
  };
  report('eval');
  var sent = false;
  var send = function () {
    if (sent) { return; }
    sent = true;
    report('frame');
  };
  if (window.requestAnimationFrame) {
    requestAnimationFrame(function () {
      requestAnimationFrame(send);
    });
  }
  // 窗口不可见时 rAF 可能不跑，300ms 后兜底再报一次（只报一次）。
  setTimeout(send, 300);
})();
"#;

/// 前端首帧标记使用的事件名（只给 [`STARTUP_MARK_SCRIPT`] 与监听端共用）。
const STARTUP_MARK_EVENT: &str = "hbot://startup-mark";

/// 前几条命令的序号从 1 开始，用于把"首个 RPC"和它的响应配成一对。
static DISPATCH_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

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

/// 核心预取插件：**在单实例检查之后、创建窗口之前**把 `AppState::open` 放到后台线程。
///
/// 插件按注册顺序初始化，而单实例插件注册在本插件之前：第二个实例会在单实例插件
/// 的 `initialize` 里直接 `exit(0)`，根本走不到这里，所以不会出现两个进程同时打开
/// 同一个 SQLite 库的写入者。第一个实例则可以在 WebView2 初始化（约 300 ms）的
/// 同时打开数据库，把这段 IO 完全藏进窗口创建时间里。
struct CorePrefetchPlugin {
    host: Option<Arc<DesktopHost>>,
}

impl<R: tauri::Runtime> tauri::plugin::Plugin<R> for CorePrefetchPlugin {
    fn name(&self) -> &'static str {
        "hbot-core-prefetch"
    }

    fn initialize(
        &mut self,
        _app: &tauri::AppHandle<R>,
        _config: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(host) = self.host.take() else {
            return Ok(());
        };
        startup_trace::point("core_prefetch_start", "");
        tauri::async_runtime::spawn(async move {
            match host.core().await {
                // 真正的「核心可用」时刻：预取任务完成，比 setup 里观察到的更早。
                Ok(_) => startup_trace::point("core_ready", ""),
                Err(_) => startup_trace::point("core_failed", ""),
            }
        });
        Ok(())
    }
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
    startup_trace::point("open_core_begin", "");
    let blocking_dir = data_dir.clone();
    let opened = tokio::task::spawn_blocking(move || {
        startup_trace::point("open_blocking_enter", "");
        let opened = AppState::open(&blocking_dir, Arc::new(SystemClock), Arc::new(UuidIds));
        startup_trace::point("appstate_open_returned", "");
        opened
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
    startup_trace::point("dispatcher_ready", "");
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
    let sequence = DISPATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let started = startup_trace::now();
    let kind = command.kind();
    if sequence <= 8 && startup_trace::enabled() {
        startup_trace::point("dispatch_in", &format!("n={sequence} cmd={kind}"));
    }

    let response = match app.try_state::<Arc<DesktopHost>>() {
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
    };

    if let Some(started) = started {
        if sequence <= 8 {
            startup_trace::point(
                "dispatch_out",
                &format!(
                    "n={sequence} cmd={kind} dur_ms={:.3} ok={}",
                    started.elapsed().as_secs_f64() * 1000.0,
                    matches!(response, Response::Ok { .. })
                ),
            );
        }
    }

    response
}

/// 测量专用：接收页面脚本送来的首帧标记（`HBOT_STARTUP_TRACE=1` 时才会被调用）。
///
/// 走应用命令而不是 `plugin:event|emit`：应用命令不经过 capabilities ACL，页面脚本
/// 无需任何额外权限；关闭 trace 时函数体只剩一次 `enabled()`（OnceLock 读取）。
#[tauri::command]
fn startup_mark(payload: serde_json::Value) {
    if startup_trace::enabled() {
        eprintln!("hbot-startup ev=frontend_mark payload={payload}");
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
pub(crate) fn show_error_dialog(title: &str, message: &str) {
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
pub(crate) fn show_error_dialog(title: &str, message: &str) {
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

/// 仅测试用：启动期主动 panic，验证 `<data_dir>/logs/crash-*.log` 真的落盘。
///
/// 两道闸，保证普通用户（包括 release 用户）误触发不了：
///
/// 1. **编译期**：整个函数体只在 `debug_assertions` 构建里存在。release 下即使
///    环境变量写成 `1` 也不会 panic（这段代码根本不编译进去）。
/// 2. **运行期**：debug 构建里也必须显式设置 `HBOT_TEST_PANIC_ON_START=1`。
///
/// panic 消息里故意带 `api_key=sk-live-test-s6-crash-e2e-7f3a9b2c`，让
/// `scripts/verify-crash-report.ps1` 能断言报告经过了
/// `hbot_rpc::app::agent::log::redact_text`（见 `crash.rs` 的 `render`）。
/// `marker=S6-CRASH-E2E` 是脚本定位这一次 panic 的稳定标记。
fn maybe_trigger_test_panic() {
    #[cfg(debug_assertions)]
    {
        if std::env::var("HBOT_TEST_PANIC_ON_START").as_deref() == Ok("1") {
            panic!(
                "HBOT_TEST_PANIC_ON_START=1：启动期自检 panic（marker=S6-CRASH-E2E；\
                 api_key=sk-live-test-s6-crash-e2e-7f3a9b2c）"
            );
        }
    }
}

/// 启动桌面壳，返回进程退出码。
///
/// 只在最外层返回 `i32`；内部的失败都变成可读错误 + 退出码 1，不做 unwrap/panic。
///
/// 顺序有三处刻意安排：
///
/// 1. **命令行请求先于一切**：`--set-autostart` / `--print-settings` 不建窗口、不开
///    SQLite，所以第一个实例正跑着时也能安全执行（不会被单实例拦下，也不需要拦）。
/// 2. **崩溃 hook 在 `Builder` 之前装**：启动期的 panic 也要留报告。
/// 3. **单实例插件第一个注册**：插件按注册顺序初始化，而插件初始化发生在配置窗口
///    创建之前（`tauri-2.12.5/src/app.rs`：先 `initialize_plugins` 再
///    `WebviewWindowBuilder::from_config`）。第二个实例在插件里就把参数交给第一个实例
///    并以 `0` 退出，**不会**再开一个 WebView、也不会再开一个数据库写入者。
pub fn run() -> i32 {
    startup_trace::point("run_enter", "");

    // 1. 命令行入口（`hbot --set-autostart on` 之类）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::run_headless(&args) {
        return code;
    }
    startup_trace::point("cli_headless_done", "");

    init_tracing();
    startup_trace::point("tracing_ready", "");

    // 2. 崩溃上报：只写本机 `<data_dir>/logs/crash-*.log`，下次启动变成一条 error 日志。
    //    数据目录算法只有一份（`hbot_core::data_dir`），这里和 `setup` 用的是同一个。
    //    同时把核心预取要用的绝对路径算出来：没有绝对路径时（极端环境缺 APPDATA）
    //    退到 setup 里再用 Tauri 的 `app_data_dir`，这时不预取。
    let resolved = hbot_core::data_dir();
    let data_dir = resolved.is_absolute().then_some(resolved);
    let early_host = data_dir.clone().map(|dir| Arc::new(DesktopHost::new(dir)));
    crash::install(Some(data_dir.clone().unwrap_or_else(hbot_core::data_dir)));
    // 仅测试用的启动期 panic（debug 构建 + 显式环境变量，见函数文档）。
    maybe_trigger_test_panic();
    startup_trace::point("crash_hook_ready", "");
    startup_trace::point("builder_begin", "");

    let outcome = tauri::Builder::default()
        // 3. 单实例：第二个进程把 argv/cwd 交给第一个，然后自己 exit(0)。
        //    回调里"把已有窗口带到前台"就是用户能感知到的全部效果。
        //    隐藏入口 `--tray-action <id>` 也走这条通道：第二个进程把 argv 交过来，
        //    这里把它翻译成与真实菜单点击完全相同的 `MenuAction` 再派发。
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(raw) = cli::tray_action_from_args(&argv) {
                let id = shell::menu_id_for_cli(&raw);
                shell::dispatch_menu_action(app, shell::handle_menu(&id), "cli", id.0.as_str());
                return;
            }
            tracing::info!("检测到第二次启动，把已有窗口带到前台");
            shell::show_main_window(app);
        }))
        // 预取核心：必须在单实例插件之后注册，第二个实例走不到这里（见结构体文档）。
        .plugin(CorePrefetchPlugin {
            host: early_host.clone(),
        })
        .invoke_handler(tauri::generate_handler![dispatch, startup_mark])
        .on_menu_event(shell::on_menu_event)
        .on_tray_icon_event(|app, event| shell::on_tray_icon_event(app, &event))
        .on_page_load(|webview, payload| {
            if !startup_trace::enabled() {
                return;
            }
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                startup_trace::point("page_load_finished", "");
                if let Err(error) = webview.eval(STARTUP_MARK_SCRIPT) {
                    eprintln!("hbot-startup ev=page_mark_eval_failed message={error}");
                }
            }
        })
        .setup(move |app| {
            startup_trace::point("setup_enter", "");
            let handle = app.handle().clone();

            // 数据目录必须与 `hbot serve` / `hbot replay` 一致，否则同一个产品
            // 的桌面端和后台端会各开一个库（`%APPDATA%\hbot` vs
            // `%APPDATA%\com.hbot.app`）。算法只有一份，在 `hbot_core::paths`。
            let data_dir = match data_dir.clone() {
                Some(dir) => dir,
                None => {
                    // 连 HOME/APPDATA 都读不到的极端环境才走 Tauri 的兜底，
                    // 并明确记日志——这不是正常路径（也不会预取核心）。
                    let resolved = hbot_core::data_dir();
                    tracing::warn!(
                        fallback = %resolved.display(),
                        "环境变量里没有用户目录，改用 Tauri 的应用数据目录"
                    );
                    match handle.path().app_data_dir() {
                        Ok(dir) => {
                            tracing::warn!(data_dir = %dir.display(), "已切换到 Tauri 应用数据目录");
                            dir
                        }
                        Err(error) => {
                            let message = format!("无法确定应用数据目录：{error}");
                            report_fatal(&message);
                            return Err(message.into());
                        }
                    }
                }
            };
            tracing::info!(data_dir = %data_dir.display(), "桌面壳数据目录");
            if startup_trace::enabled() {
                startup_trace::point(
                    "data_dir_resolved",
                    &format!("data_dir={}", data_dir.display()),
                );
            }

            // 测量专用：收前端首帧标记（`STARTUP_MARK_SCRIPT` 通过 plugin:event|emit 发出）。
            // 这条路径只在 `HBOT_STARTUP_TRACE=1` 时执行。
            if startup_trace::enabled() {
                // `AppHandle::listen` 返回 `EventId`（u32）而不是 `Result`；Tauri 2.12
                // 里没有可失败的分支。AppHandle 级监听器随应用存活，不需要 unlisten。
                let _event_id = app.handle().listen(STARTUP_MARK_EVENT, |event| {
                    eprintln!("hbot-startup ev=frontend_mark payload={}", event.payload());
                });
            }

            let host = match early_host {
                Some(host) => host,
                None => Arc::new(DesktopHost::new(data_dir.clone())),
            };
            app.manage(Arc::clone(&host));
            startup_trace::point("host_managed", "");

            // 托盘菜单、关窗到托盘、退出按钮。托盘图标本身由 `tauri.conf.json` 的
            // `app.trayIcon` 在 `Builder::build()` 里建好，这里只挂菜单。
            let prefs = settings::DesktopSettings::load(&data_dir);
            startup_trace::point("settings_loaded", "");
            reconcile_autostart(&prefs, &data_dir);
            startup_trace::point("autostart_reconciled", "");
            if let Err(error) = shell::install(&handle, prefs, data_dir.clone()) {
                // 托盘装不上不该让应用起不来，但这意味着关窗行为可能不对 —— 必须留痕。
                tracing::error!(%error, "托盘/关窗行为安装失败");
            }
            startup_trace::point("shell_installed", "");

            // 核心装配放后台任务：SQLite 打开 + 迁移不能挡住窗口首帧（§2.1）。
            let crash_dir = data_dir;
            tauri::async_runtime::spawn(async move {
                startup_trace::point("open_task_start", "");
                match host.core().await {
                    Ok(core) => {
                        startup_trace::point("core_ready_observed", "");
                        attach_event_pumps(&handle, &core.state);
                        startup_trace::point("event_pumps_ready", "");
                        // 事件泵起来之后再看上次有没有崩过：`EventBus` 是广播，
                        // 没有订阅者的事件会被丢掉，所以必须等订阅挂上再说。
                        crash::report_pending(crash_dir, Arc::clone(&core.state)).await;
                    }
                    Err(message) => {
                        startup_trace::point("core_failed", "");
                        let message = format!("hbot 启动失败：{message}");
                        // 对话框会阻塞调用线程，放到 blocking 池，别堵住 async worker。
                        let report = message.clone();
                        let _ = tokio::task::spawn_blocking(move || report_fatal(&report)).await;
                        // 直接以退出码 1 结束进程（Tauri 内部走 `std::process::exit`）。
                        handle.exit(1);
                    }
                }
            });

            startup_trace::point("setup_exit", "");
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

/// 让"偏好文件"与"系统里的实际自启项"一致。
///
/// 两个方向都用**事实**作准，不做单边覆盖：
///
/// - 系统里开着、偏好说关 → 把偏好改成开（并落盘）。这是升级/重装后最常见的情况：
///   注册表项还在，而 `desktop.json` 是新建的默认值。此时**不删系统项** —— 用户
///   上次明确开过，静默删掉才是更大的意外。
/// - 偏好说开、系统里没有 → 重新写系统项（用户手工清过注册表或换了安装路径）。
///   写失败就把偏好改回关，避免"勾选亮着却不起作用"。
fn reconcile_autostart(prefs: &settings::DesktopSettings, data_dir: &Path) {
    let system = autostart::current();
    match (prefs.autostart, system) {
        (false, autostart::AutostartState::Enabled) => {
            tracing::info!("系统里已有自启项，偏好文件说关；以系统为准改偏好为开");
            let adopted = settings::DesktopSettings {
                autostart: true,
                ..*prefs
            };
            if let Err(error) = adopted.save(data_dir) {
                tracing::warn!(%error, "把自启偏好改成开失败");
            }
        }
        (true, autostart::AutostartState::Disabled) => {
            tracing::info!("偏好说开机自启，但系统里没有；重新写入");
            if let Err(error) = autostart::set_enabled(true) {
                tracing::warn!(%error, "重写自启项失败，偏好改回关");
                let disabled = settings::DesktopSettings {
                    autostart: false,
                    ..*prefs
                };
                if let Err(save_error) = disabled.save(data_dir) {
                    tracing::warn!(%save_error, "把自启偏好改回关也失败了");
                }
            }
        }
        _ => {}
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
