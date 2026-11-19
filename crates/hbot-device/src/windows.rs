//! Windows 实现：**PrintWindow/BitBlt 截图、SendInput 注入、EnumWindows 枚举、剪贴板读写**。
//!
//! 设计选型与真实代价写在 crate 文档（[`crate`]）里，这里只记"为什么这么写"以及坑。
//!
//! # 为什么截图用 `PrintWindow` 而不是 DXGI Desktop Duplication
//!
//! `docs/09` §6.1 列了三个候选（BitBlt+PrintWindow / Windows.Graphics.Capture / DXGI
//! Desktop Duplication），明确说"先做实测成本最低的那个"。`PrintWindow` 无新依赖、
//! 代码量小、能只抓**单个窗口**；代价是**独占全屏 DirectX / DRM 内容可能全黑**（已知限制，
//! 不是 bug）。DXGI 要自己管 D3D11 设备、输出复制、每次分辨率变化重建，代码量是这个的数倍，
//! 而窄版的出口标准只是"链路能通"。
//!
//! 抓窗口失败（`GetForegroundWindow` 为空、`GetWindowRect` 失败、`PrintWindow` 返回 0）
//! 时退化成**主显示器整屏 `BitBlt`** —— 保证"至少有一张图"，同时 `tracing::debug` 记下原因。
//!
//! # 为什么所有句柄都用 RAII 包着
//!
//! GDI 对象泄漏到一定数量（每进程默认 10000 个）会让**整个进程**的 `CreateDIBSection`
//! 开始失败，而且表现成"截图突然坏了"，极难归因。所以 HDC/HBITMAP 一律进 [`Surface`]，
//! 失败路径也走 `Drop`。
//!
//! # 输入路径的三道闸（顺序不可调换）
//!
//! 1. **空白名单 = 全拒**，且必须在**任何 Win32 调用之前**返回（单测钉住了这条：
//!    `empty_allow_list_is_rejected_before_any_win32_call`）；
//! 2. **锁屏/安全桌面**：`OpenInputDesktop` 拿到的桌面名不是 `Default` 就拒绝；
//! 3. **前台进程白名单**：拿不到进程名也拒绝（取不到就说明可能是提权进程）。
//!
//! 三道都过之后才碰 `SendInput`。`SendInput` 只送进一部分事件也当失败（不重试：UIPI 拦住的
//! 重试一万次也一样，反而让"按了一半"的状态更难收拾）。
//!
//! # DPI：实测数字、声明前后各是什么，以及本 crate 做不到什么
//!
//! 这一节回答那个反复被问的问题："声明 Per-Monitor V2 之后，`GetMonitorInfoW` 的
//! `rcMonitor` 是物理像素还是逻辑像素？`CreateDCW` 抓到的位图尺寸等于 `rcMonitor` 吗？"
//!
//! **先给结论，再给数字。**
//!
//! * 进程是 **DPI-Unaware**（没有 manifest、也没调运行时 API）时，系统对**本进程**虚拟化：
//!   `rcMonitor` 是逻辑像素（`物理 / 缩放比`），`CreateDCW(szDevice)` 建出来的 DC 也被钉成
//!   `LOGPIXELSX = 96`，`BitBlt` 抓出来的位图与 `rcMonitor` **一样大**（也一起被虚拟化）。
//!   所以"截图宽高 = rcMonitor 宽高"这条**恒成立**，但两个都不是物理像素 —— 在 150% 缩放下
//!   1920 宽的屏会报 1280、抓出 1280；把这张 1280 宽的图按 1280 归一化去点 1920 宽的物理屏，
//!   点就落在 `x * 1920/1280 = 1.5x` 的位置。**上一批 `device-win2` 的跨屏点击偏差就是这个。**
//! * 进程是 **Per-Monitor V2** 时，`rcMonitor` 是**物理像素**，`GetDeviceCaps(DESKTOPHORZRES)`
//!   与它相等，`CreateDCW` 抓到的位图也等于它 —— 三者对齐，归一化坐标与像素坐标是同一个东西。
//!
//! 本机实测（Windows 11 26300，笔记本单屏 1920×1080，**100% 缩放**，`GetDpiForSystem() = 96`）：
//!
//! | 量 | 声明前（DPI-Unaware） | 声明后（Per-Monitor V2） |
//! |---|---|---|
//! | `GetDpiForSystem()` | 96 | 96 |
//! | `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` | 96 | 96 |
//! | `rcMonitor` | 1920×1080 | 1920×1080 |
//! | `GetDeviceCaps(DESKTOPHORZRES/VERTRES)` | 1920×1080 | 1920×1080 |
//! | `GetDeviceCaps(LOGPIXELSX)` | 96 | 96 |
//! | 截图位图 | 1920×1080 | 1920×1080 |
//!
//! **这张表不能证明虚拟化存在 —— 它证明的是本机 100% 缩放，所以没有虚拟化可做。** 上表四个
//! 尺寸在两种档位下全等，正是"100% 缩放下两种档位等价"的实证；150% 缩放下声明前的
//! `rcMonitor`/位图会变成 1280×720（1920/1.5），声明后仍是 1920×1080。这条是**系统文档
//! 保证的行为**（DPI Virtualization），本机没有 150% 缩放可测，如实标注为未实测。
//!
//! 复现命令（真机、无需 GUI）：`cargo test -p hbot-device --test windows_dpi -- --nocapture`
//! —— 它会把"声明前/声明后"两组数字打成表，并断言每块屏的 `scale_percent > 0`、
//! `dpi_x`/`dpi_y` 在 96..=480、`display_metrics()` 与 `displays()` 一一对应。
//!
//! ## 本 crate 做不到什么（别在这里想办法）
//!
//! 1. **装 manifest**：Per-Monitor V2 的首选声明方式是 PE manifest，那是**可执行文件**的
//!    资源，库改不了调用方的 exe；`hbot-device` 被 `hbot-rpc`/`hbot-cli`/`hbot-desktop`
//!    三方共用，给其中一个装都会污染另外两个。运行时 API 那半套也必须在**宿主的最早期**
//!    调用（窗口/WebView 创建之前），所以它属于壳 —— `crates/hbot-desktop/src/dpi.rs`。
//! 2. **替宿主声明**：库里调用 `SetProcessDpiAwarenessContext` 会让"谁改了我的进程"变成
//!    无法追查的隐式副作用（而且顺序取决于库的加载时机）。本 crate 只**读**当前档位的后果
//!    （[`crate::display_metrics`] 的 `size_delta()`），不改变它。
//! 3. **混合 DPI 下的跨屏拼图**：`screenshot(Some(i))` 每次只抓一块屏，跨屏拼图要调用方自己
//!    按 `DisplayInfo` 的 `(x, y, width, height)` 摆放；当两块屏缩放比不同时，**逻辑坐标与
//!    物理坐标没有统一的换算比**，任何"拼一张大图"的方案都得先选定基准坐标空间。本 crate
//!    不替调用方选（选错了比不拼更糟）。
//! 4. **WebView 内部**：Tauri 的 WebView2 有自己的 DPI 处理，本 crate 与壳的声明只影响
//!    Win32 这一侧（`rcMonitor`/截图/`SendInput` 的坐标）。WebView 里 CSS 像素与屏幕像素的
//!    换算由 WebView2 负责，不受这里影响。

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hbot_core::device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapabilityReport, DeviceError, DeviceTarget,
    InputAction, MouseButton, OcrRequest, OcrText, Screenshot, ScreenshotRequest, WindowCapture,
    WindowCaptureRequest, WindowInfo,
};
// WinRT（`Windows.Media.Ocr`）。**是 `windows` crate，不是 `windows-sys`**：后者只有裸 ABI
// 声明，没有 `OcrEngine` / `SoftwareBitmap` 这些投影出来的类型。
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows_sys::Win32::Foundation::{BOOL, CloseHandle, GlobalFree, HANDLE, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDCW, CreateDIBSection,
    DESKTOPHORZRES, DESKTOPVERTRES, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetDeviceCaps, GetMonitorInfoW, GetWindowDC, HBITMAP, HDC, HGDIOBJ, HMONITOR,
    LOGPIXELSX, MONITORINFO, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
// WinRT 的异步对象要求线程先初始化公寓（`RoInitialize` 就是 COM 公寓初始化的薄封装）。
use windows_sys::Win32::System::Com::{
    CO_MTA_USAGE_COOKIE, COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_F1,
    VK_LEFT, VK_LWIN, VK_MENU, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP, VkKeyScanW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, MONITORINFOF_PRIMARY, PW_RENDERFULLCONTENT, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WS_EX_TOOLWINDOW,
};

use crate::{
    Combo, ComboKey, ComboModifier, DisplayInfo, DisplayMetrics, app_is_allowed, enforce_limit,
    parse_combo, png, process_name,
};

/// 窗口枚举上限（`docs/09` §6.3：列表是给人/模型看的，几百个窗口没有意义）。
const MAX_WINDOWS: usize = 200;

/// 截图边长上限。超过它基本可以断定是显示器拓扑异常，而不是真有一块 6 万像素的屏。
const MAX_SCREENSHOT_DIMENSION: i32 = 16384;

/// 截图总像素上限（约 1 亿像素）。防止一个畸形 `GetWindowRect` 让我们尝试分配 20 GB。
const MAX_SCREENSHOT_PIXELS: u64 = 100_000_000;

/// 一次 `SendInput` 送多少个 UTF-16 码元（每个码元两个事件）。32 是个保守值：
/// 太大会让单个 `SendInput` 超过系统输入队列的处理速度，失败得毫无线索。
const TEXT_CHUNK_UNITS: usize = 32;

/// 读剪贴板时的码元上限（4 Mi 个 UTF-16 码元 ≈ 8 MB）。防止没有终止符的脏数据
/// 让我们扫到进程边界外。
const MAX_CLIPBOARD_UNITS: usize = 4 * 1024 * 1024;

/// `OpenClipboard` 的重试次数与间隔。
///
/// 剪贴板是**全系统共享**的单一资源，别的进程正拿着它（Explorer、Office、输入法都很常见）
/// 时 `OpenClipboard` 直接失败。这不是"设备不可用"，等 100ms 通常就好了，所以重试
/// 而不是立刻报错。
const CLIPBOARD_OPEN_ATTEMPTS: usize = 5;
const CLIPBOARD_OPEN_RETRY_DELAY: Duration = Duration::from_millis(20);

/// OCR 识别的等待上限。
///
/// `OcrEngine::RecognizeAsync` 是 WinRT 异步操作，本后端在**调用线程**上同步等它（trait 的契约
/// 是同步的，调用方负责 `spawn_blocking` 与超时）。1080p 整屏的识别实测在 100ms 量级，
/// 20 秒是"引擎卡死"和"机器很慢"之间的分界：超过它就别再假装还能出结果了。
const OCR_TIMEOUT: Duration = Duration::from_secs(20);

/// 轮询 `AsyncStatus` 的间隔。50ms 会让小图多睡几十毫秒，1ms 又白烧 CPU。
const OCR_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// `Windows.Foundation.AsyncStatus::Started`。
///
/// `AsyncStatus` 在 `windows` 0.62 里的定义是 `pub struct AsyncStatus(pub i32)`（异步类型搬到了
/// 独立的 `windows-future` crate，那个 crate **没有**被 `windows` re-export，所以本文件不引用
/// 它的类型，只用 `.Status()` 拿到的值比较字段）。`Started=0 / Completed=1 / Canceled=2 / Error=3`。
const ASYNC_STATUS_STARTED: i32 = 0;

/// `CoInitializeEx` 在这个线程上已经初始化过（引用计数 +1，返回值是 S_FALSE 而不是 S_OK）。
const S_FALSE_HRESULT: i32 = 1;

/// `RPC_E_CHANGED_MODE`：当前线程已经在**另一个**公寓里（宿主是 STA 时很常见）。
///
/// 这不是"不能用 WinRT"—— 公寓已经存在，激活与轮询都不需要本线程待在自己的 MTA 里。
/// 代价是我们不能替宿主还任何引用（反正我们也从不 [`CoInitializeEx`] 的配对里还，
/// 见 [`APARTMENT`] 那段注释）。
const RPC_E_CHANGED_MODE: i32 = -2_147_417_850; // 0x80010106

/// OCR 引擎惰性探测结果：`Ok(())` = 本机能建出 `OcrEngine`（装了 OCR 语言包、会话没被限制），
/// `Err` 里是**人话原因**（含 `HRESULT`）。
///
/// 用 [`OnceLock`] 而不是每次现建：建引擎要扫一遍用户配置的语言列表，而 `capabilities()` 是
/// 面板/模型都会反复调的方法。缓存的是"能不能建"，不是引擎对象本身 —— 引擎对象不跨调用复用
/// （它不是 `Send`，而且识别一次性的比揣着一个对象更不容易踩公寓/线程问题）。
static OCR_ENGINE_PROBE: OnceLock<Result<(), String>> = OnceLock::new();

/// 测试用的"我们碰了几次 Win32"计数器。
///
/// 存在的唯一理由是需求里的那条断言：**`allowed_apps` 为空时一个 Win32 调用都不许有**。
/// 光看返回值不足以证明"提前返回了"（也可能是先查了桌面、查了前台进程、最后才拒绝），
/// 所以用一个计数器把它变成可观测的。
///
/// 这**不是**产品代码里的全局可变状态：`#[cfg(not(test))]` 分支里 [`bump`] 是空函数，
/// 计数器在正式构建里根本不存在。
mod win32_probe {
    #[cfg(test)]
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(test)]
    static WIN32_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// 记一次"要走 Win32 的路径"。
    #[cfg(test)]
    pub(super) fn bump() {
        WIN32_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// 正式构建：什么也不做（没有全局状态）。
    #[cfg(not(test))]
    pub(super) fn bump() {}

    #[cfg(test)]
    pub(super) fn reset() {
        WIN32_CALLS.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn count() -> usize {
        WIN32_CALLS.load(Ordering::Relaxed)
    }
}

use win32_probe::bump;

/// Windows 设备后端。
///
/// 零字段 —— 没有要维护的状态，所以：
///
/// - `Send + Sync` 是**自动**满足的（零字段结构体天然满足），**没有** `unsafe impl`；
/// - 没有全局可变状态，也没有懒加载的单例；
/// - 每次调用都重新探测（能力、前台窗口、桌面），因此"用户刚插上显示器""刚锁屏"
///   这类变化下一次调用就能看到，不需要重启。
///
/// 每次调用都重新探测是有意的：`docs/09` §4.3 要求后端每次都反映当前真实状态，
/// 缓存快照会让"一键撤销"只对下一次构造生效。
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsBackend;

impl WindowsBackend {
    /// 造一个后端。永不失败 —— 探测发生在 [`DeviceBackend::capabilities`]。
    pub fn new() -> Self {
        Self
    }

    /// 截图并编码成 PNG，然后按 `max_bytes` 检查（**不截断**）。
    ///
    /// `monitor` 的语义（见 [`DisplayInfo`] 与 crate 文档）：
    ///
    /// - `None` 是**旧路径**：先试 `PrintWindow` 抓前台窗口，失败退主屏 `BitBlt`。这是历史上
    ///   "快速看一眼当前在干什么"的语义，保持不动；
    /// - `Some(i)` 是**第 i 块显示器的整屏**（`Some(0)` = 主屏）：显式点名一块屏时不做
    ///   "前台窗口优先"，否则"截副屏"会变成"截主屏上那个窗口"，与参数承诺不符；
    /// - 不存在的序号返回 `HostLimit` 并列出可用数量，**绝不静默退回主屏** —— 静默给错显示器
    ///   会让模型对着错误的画面决策。
    fn capture(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        let displays = enumerate_displays()?;
        let monitor = request.monitor.unwrap_or(0);
        let Some(target) = displays.get(monitor as usize) else {
            return Err(DeviceError::HostLimit {
                detail: format!(
                    "显示器序号 {monitor} 不存在：本机只有 {} 块显示器（可用序号 0..={}，0 是主屏）",
                    displays.len(),
                    displays.len().saturating_sub(1)
                ),
            });
        };
        // `None`（没点名）= 旧路径"先试前台窗口，失败退主屏"；显式点名任一块屏（含 0）时
        // 抓那块屏的整屏 —— 参数已经说清是哪块屏，再掺"前台窗口优先"就与承诺不符。
        let frame = match request.monitor {
            None => capture_frame()?,
            Some(_) => blit_display(target)?.read_frame()?,
        };
        let encoded = png::encode_rgb(frame.width, frame.height, &frame.rgb)?;
        tracing::debug!(
            width = frame.width,
            height = frame.height,
            bytes = encoded.len(),
            monitor,
            device = %target.device,
            "截图完成"
        );
        let png = enforce_limit(encoded, request.max_bytes)?;
        Ok(Screenshot {
            width: frame.width,
            height: frame.height,
            png,
            monitor,
        })
    }

    /// 按窗口截图：句柄 → 位图 → PNG → 按 `max_bytes` 检查（**不截断**）。
    ///
    /// 复用 [`capture_window_for_ocr`]：那条路径已经处理了"`PrintWindow` 返回成功但位图
    /// 全零"这个实测坑，并有窗口 DC 的 `BitBlt` 退路。截图与 OCR 要的是同一份像素，
    /// 分成两份实现只会让其中一个先腐烂。
    ///
    /// 两条前置拒绝（都在抓图之前，且都报 `HostLimit` —— 调用方重试同样的句柄
    /// 也不会成功，补救动作是换句柄或先把窗口开出来）：
    ///
    /// 1. 句柄不存在（窗口已关闭）：报 `HostLimit` 并提示重新枚举；
    /// 2. 窗口最小化（`IsIconic`）：`GetWindowRect` 给的是 `-32000` 那一带的坐标，位图要么
    ///    全零要么是别的窗口的内容 —— 抓出来的东西与"这个窗口长什么样"无关。
    ///
    /// 抓到的位图**尺寸为 0**（窗口被折叠、客户区为空）同样拒绝：给一张 0×0 的 PNG
    /// 会让调用方以为成功。
    fn capture_window_frame(
        &self,
        request: &WindowCaptureRequest,
    ) -> Result<WindowCapture, DeviceError> {
        let hwnd: HWND = request.window as HWND;
        // SAFETY: 整数转句柄不做任何解引用；`IsWindow` / `IsIconic` 对任意句柄都安全
        // （包括伪造的、已销毁的），它们只读系统表并返回 0。
        if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
            return Err(DeviceError::HostLimit {
                detail: format!(
                    "窗口句柄 {} 不存在或已经关闭：先调 ctx.device.windows() 拿当前句柄",
                    request.window
                ),
            });
        }
        // SAFETY: 同上，`IsIconic` 对任意句柄安全。
        if unsafe { IsIconic(hwnd) } != 0 {
            return Err(DeviceError::HostLimit {
                detail: format!(
                    "窗口 {} 已最小化：最小化窗口的矩形是 -32000 那一带的占位值，抓不到它的内容",
                    request.window
                ),
            });
        }

        let frame = capture_window_for_ocr(hwnd)?.read_frame()?;
        if frame.width == 0 || frame.height == 0 {
            return Err(DeviceError::Failed {
                message: format!(
                    "窗口 {} 的可视区域是 0×0（被折叠或没有客户区），没有可截的像素",
                    request.window
                ),
            });
        }
        let encoded = png::encode_rgb(frame.width, frame.height, &frame.rgb)?;
        tracing::debug!(
            width = frame.width,
            height = frame.height,
            bytes = encoded.len(),
            window = request.window,
            "按窗口截图完成"
        );
        let png = enforce_limit(encoded, request.max_bytes)?;
        Ok(WindowCapture {
            width: frame.width,
            height: frame.height,
            png,
            window: request.window,
        })
    }

    /// 抓目标区域 → WinRT OCR → 按 `max_chars` 截断。
    ///
    /// 两种目标：
    ///
    /// - [`DeviceTarget::None`]：主屏整屏（`BitBlt` 主屏 DC）；
    /// - [`DeviceTarget::Window{process}`]：那个进程的**一个可读窗口**（查找规则见
    ///   [`find_window_for_process`]），用 `PrintWindow` 抓它，失败退 `GetWindowDC` + `BitBlt`。
    ///
    /// **为什么读不到窗口返回 `AppNotAllowed{process}` 而不是 `HostLimit`**：`HostLimit` 的契约是
    /// "当前宿主没有这个能力"（Wayland、无头、无交互桌面），而这里能力是有的，只是**这个进程
    /// 此刻没有可读的窗口**（没启动 / 只有最小化或无标题窗口）—— 说成 `HostLimit` 会让调用方
    /// 永久放弃按窗口 OCR。`AppNotAllowed` 至少带上了**具体是哪个进程**，模型能据此换目标或提示
    /// 用户把窗口开出来。代价是它的展示文案（`DeviceError` 的 `Display`）写的是"不在白名单里"，
    /// 用在"没有窗口"这个场景上措辞不准 —— 这是已知文案债，改文案要改 `hbot-core`（另一个
    /// crate，不在本次范围内），先记在这里。
    ///
    /// 空字符串是**合法结果**（窗口里真没字），但"引擎没建出来"和"识别过程中出错"都是错误，
    /// 不会降级成空串 —— 把失败说成"没识别到字"会让调用方以为画面里真的没有字。
    fn recognize_text(&self, request: &OcrRequest) -> Result<OcrText, DeviceError> {
        let (width, height, bgra) = match &request.target {
            // 主屏整屏，不复用 `capture_frame()`：后者的首选是"前台窗口"（`PrintWindow`），
            // 那是**截图**语义。OCR 要的是屏幕上真显示的东西，用 `BitBlt` 主屏那份。
            DeviceTarget::None => blit_primary_screen()?.read_bgra()?,
            DeviceTarget::Window { process } => {
                let Some(hwnd) = find_window_for_process(process) else {
                    tracing::debug!(process, "这个进程没有可读的窗口");
                    return Err(DeviceError::AppNotAllowed {
                        process: process.clone(),
                    });
                };
                capture_window_for_ocr(hwnd)?.read_bgra()?
            }
        };
        let recognized = recognize_bgra(width, height, &bgra)?;
        tracing::debug!(
            width,
            height,
            chars = recognized.chars().count(),
            "OCR 完成"
        );
        Ok(truncate_ocr_text(recognized, request.max_chars))
    }

    /// 移动并点击（`MOUSEEVENTF_MOVE|ABSOLUTE` + down + up，**一次** `SendInput`）。
    ///
    /// 三个事件必须一次送进去：分三次调用的话，别的程序插进来的鼠标移动会让"按下"和
    /// "抬起"落在不同位置，用户看到的是拖拽。
    fn click(&self, x: i32, y: i32, button: MouseButton) -> Result<(), DeviceError> {
        let (origin_x, origin_y, width, height) = virtual_screen_bounds()?;
        let (normalized_x, normalized_y) =
            absolute_mouse_coords(x, y, origin_x, origin_y, width, height);
        let (down, up) = mouse_button_flags(button);
        let mut inputs: [INPUT; 3] = zeroed_inputs();
        // 写联合体成员本身是安全操作（读才需要 unsafe）；这里的不变量是"成员的 名字
        // 必须与 `r#type` 对应"，所以两者紧挨着写，别拆开。
        inputs[0].r#type = INPUT_MOUSE;
        inputs[0].Anonymous.mi = MOUSEINPUT {
            dx: normalized_x,
            dy: normalized_y,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
            time: 0,
            dwExtraInfo: 0,
        };
        inputs[1].r#type = INPUT_MOUSE;
        inputs[1].Anonymous.mi = MOUSEINPUT {
            dx: normalized_x,
            dy: normalized_y,
            mouseData: 0,
            dwFlags: down,
            time: 0,
            dwExtraInfo: 0,
        };
        inputs[2].r#type = INPUT_MOUSE;
        inputs[2].Anonymous.mi = MOUSEINPUT {
            dx: normalized_x,
            dy: normalized_y,
            mouseData: 0,
            dwFlags: up,
            time: 0,
            dwExtraInfo: 0,
        };
        send_inputs(&inputs, "click")
    }

    /// 逐字符注入文本（`KEYEVENTF_UNICODE`），**不碰剪贴板**。
    ///
    /// 用 `KEYEVENTF_UNICODE` 而不是"找虚拟键"：后者要按键盘布局映射，中文/emoji/符号
    /// 各种布局下都对不上；UNICODE 路线是"直接告诉系统要打这个码元"，与布局无关。
    /// 代理对（emoji）按 UTF-16 码元逐半送出，Windows 会把它们合成一个字符。
    fn type_text(&self, text: &str) -> Result<(), DeviceError> {
        let units: Vec<u16> = text.encode_utf16().collect();
        if units.is_empty() {
            return Ok(());
        }
        for chunk in units.chunks(TEXT_CHUNK_UNITS) {
            let mut inputs: Vec<INPUT> = Vec::with_capacity(chunk.len() * 2);
            for &unit in chunk {
                inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE));
                inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            send_inputs(&inputs, "text")?;
        }
        Ok(())
    }

    /// 按下组合键：修饰键落下 → 目标键落下抬起 → 修饰键逆序抬起。
    fn press_combo(&self, combo: &Combo) -> Result<(), DeviceError> {
        let (virtual_key, needs_shift) = virtual_key_of(combo.key)?;
        let mut modifiers = combo.modifiers.clone();
        // 形如 `!` 的字符在多数布局上要 Shift 才打得出；用户没给 Shift 就替他补上，
        // 否则注入的是 `1` 而不是 `!`。
        if needs_shift && !modifiers.contains(&ComboModifier::Shift) {
            modifiers.push(ComboModifier::Shift);
        }
        let modifier_keys: Vec<VIRTUAL_KEY> = modifiers
            .iter()
            .map(|modifier| modifier_virtual_key(*modifier))
            .collect();
        let mut inputs: Vec<INPUT> = Vec::with_capacity(modifier_keys.len() * 2 + 2);
        for &key in &modifier_keys {
            inputs.push(keyboard_input(key, 0, 0));
        }
        inputs.push(keyboard_input(virtual_key, 0, 0));
        inputs.push(keyboard_input(virtual_key, 0, KEYEVENTF_KEYUP));
        // 逆序抬起：与按下顺序相反，避免残留修饰键（比如 Win 键卡住会把后续点击吃掉）。
        for &key in modifier_keys.iter().rev() {
            inputs.push(keyboard_input(key, 0, KEYEVENTF_KEYUP));
        }
        send_inputs(&inputs, "combo")
    }
}

impl DeviceBackend for WindowsBackend {
    fn capabilities(&self) -> DeviceCapabilityReport {
        // 屏幕：能拿到屏幕 DC 就认为能截图（真正的失败会以 `Failed` 出现）。
        let screen_dc = unsafe { GetDC(ptr::null_mut()) };
        let screen = !screen_dc.is_null();
        if screen {
            // SAFETY: `screen_dc` 是刚由 `GetDC(NULL)` 取得、且非空的 DC；配对的
            // `ReleaseDC(NULL, …)` 是唯一合法的释放方式（不能用 `DeleteDC`）。
            unsafe { ReleaseDC(ptr::null_mut(), screen_dc) };
        }

        // 输入：当前输入桌面必须是 `Default`（锁屏 / 安全桌面时拿不到或不叫这个名字）。
        let desktop_name = input_desktop_name();
        let input = desktop_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("default"));

        // 窗口枚举与剪贴板没有"探测"这一步：前者最多返回空列表，后者失败会以
        // `Failed` 出现（探测剪贴板等于去抢一次全系统共享资源，不值得）。
        let window = true;
        let clipboard = true;

        // OCR 没有"每次调用现建引擎"的探测：`OcrEngine::TryCreateFromUserProfileLanguages`
        // 要扫一遍用户配置的语言，`capabilities()` 会被面板与模型反复调，所以探测结果缓存在
        // 一个 `OnceLock` 里（见 `ocr_engine_probe`）。
        let ocr = ocr_engine_probe().is_ok();

        // 显示器清单：**探测结果不是缓存**（刚插上/拔掉一块屏，下一次调用就能看到）。
        // 探测失败（无交互桌面）时是空清单，那时 `screen` 多半也已经是 `false`。
        let displays = enumerate_displays().unwrap_or_default();
        let display_note = if displays.is_empty() {
            "显示器清单为空：当前会话可能没有可交互桌面".to_string()
        } else {
            let list = displays
                .iter()
                .map(|item| {
                    format!(
                        "[{}] {} {}×{}{}",
                        item.index,
                        item.device,
                        item.width,
                        item.height,
                        if item.primary { "（主屏）" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("，");
            format!(
                "显示器 {} 块，序号 0 是主屏（其余按虚拟桌面左上角排序）：{list}",
                displays.len()
            )
        };

        let mut notes: Vec<String> = Vec::new();
        if !screen {
            notes.push(
                "拿不到屏幕 DC：当前会话可能没有可交互桌面上（RDP 已断开 / 服务会话）".into(),
            );
        }
        if !input {
            notes.push("当前输入桌面不是 Default：锁屏或安全桌面期间拒绝注入".into());
        }
        notes.push(format!(
            "截图走 PrintWindow/BitBlt：独占全屏 DirectX 游戏、受 DRM 保护的内容可能全黑；\
             {display_note}；monitor 给序号时抓那块屏的整屏，None 时优先抓前台窗口"
        ));
        notes.push(
            "按窗口截图走 PrintWindow（失败或位图全零则退窗口 DC 的 BitBlt）：窗口须存在、\
             未最小化；独占全屏、受 DRM 保护的内容会抓不到（报 HBOT_DEVICE_FAILED 而不是给全黑图）"
                .into(),
        );
        notes.push("输入受 UIPI 限制：完整性级别更高的窗口（提权程序、UAC 对话框）无法注入".into());
        match ocr_engine_probe() {
            Ok(()) => {
                // 语言清单是"能不能认中文"的实测口径，直接写进 notes：模型看到 `zh-*`
                // 才知道可以读中文界面（只装英文包的机器上中文会识别成空串）。
                let languages = match available_ocr_languages() {
                    Ok(tags) if tags.is_empty() => "（空）".to_string(),
                    Ok(tags) => tags.join("、"),
                    Err(reason) => format!("（取不到：{reason}）"),
                };
                notes.push(format!(
                    "OCR 走 Windows.Media.Ocr：只识别系统已安装语言包的文字，本机已装：{languages}\
                     （中文/英文各要装对应语言包），图标、艺术字、旋转超 45° 的文字识别不了；\
                     支持主屏整屏（None）与按窗口（Window{{process}}：该进程有标题、可见、\
                     未最小化且 Z 序最前的窗口，前台窗口优先）"
                ));
            }
            Err(reason) => notes.push(format!("OCR 不可用：{reason}")),
        }

        // 按窗口截图与整屏截图共用同一条判据（都要一个可交互桌面 + 窗口渲染路径）：
        // 拿不到屏幕 DC 的会话同样抓不到窗口。
        DeviceCapabilityReport {
            screen,
            input,
            window,
            window_capture: screen,
            clipboard,
            ocr,
            detail: Some(notes.join("；")),
        }
    }

    fn screenshot(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        self.capture(request)
    }

    fn capture_window(&self, request: &WindowCaptureRequest) -> Result<WindowCapture, DeviceError> {
        self.capture_window_frame(request)
    }

    fn ocr(&self, request: &OcrRequest) -> Result<OcrText, DeviceError> {
        self.recognize_text(request)
    }

    fn input(&self, action: &InputAction, allowed_apps: &[String]) -> Result<(), DeviceError> {
        // 第 0 步：空名单 = 全拒。**必须在任何 Win32 调用之前**（`docs/09` §4.2）。
        // 理由不只是效率：空名单是"配置错了"，此时去查询桌面/进程只会产生一堆无关的
        // 错误信息，掩盖真正的原因。
        if allowed_apps.is_empty() {
            return Err(DeviceError::AppNotAllowed {
                process: "<empty list>".to_string(),
            });
        }

        // 第 1 步：锁屏 / 安全桌面。
        ensure_default_input_desktop()?;

        // 第 2 步：前台进程白名单。取不到进程名同样拒绝 —— "不知道是谁"不能当成"允许"。
        let Some(process) = foreground_process_name() else {
            return Err(DeviceError::AppNotAllowed {
                process: "<unknown>".to_string(),
            });
        };
        if !app_is_allowed(allowed_apps, &process) {
            return Err(DeviceError::AppNotAllowed { process });
        }

        // 第 3 步：动作分发。
        match action {
            InputAction::Click { x, y, button } => self.click(*x, *y, *button),
            InputAction::Text { text } => self.type_text(text),
            InputAction::Combo { keys } => {
                let combo = parse_combo(keys)?;
                self.press_combo(&combo)
            }
        }
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError> {
        let mut context = EnumContext {
            windows: Vec::new(),
            own_pid: unsafe { GetCurrentProcessId() },
            foreground: unsafe { GetForegroundWindow() },
            truncated: false,
        };
        // SAFETY: `EnumWindows` 是同步的，回调在此期间持有 `context` 的可变借用；
        // 我们把 `&mut context` 转成 `LPARAM`（isize）传进去，回调里再转回来。
        // 回调只在本次调用内被调用，且不保存这个指针。
        let ok = unsafe {
            EnumWindows(
                Some(enum_window_proc),
                &mut context as *mut EnumContext as LPARAM,
            )
        };
        if ok == 0 && !context.truncated {
            return Err(DeviceError::Failed {
                message: "EnumWindows 失败：取不到窗口列表".to_string(),
            });
        }
        if context.truncated {
            tracing::warn!(
                limit = MAX_WINDOWS,
                "可见窗口超过上限，只返回前 {MAX_WINDOWS} 个"
            );
        }
        Ok(context.windows)
    }

    fn clipboard(&self, op: &ClipboardOp) -> Result<ClipboardValue, DeviceError> {
        let _session = open_clipboard()?;
        match op {
            ClipboardOp::Read => read_clipboard_text(),
            ClipboardOp::Write { text } => write_clipboard_text(text),
        }
    }
}

// ---------------------------------------------------------------- 显示器清单

impl WindowsBackend {
    /// 本机显示器清单，按 [`DisplayInfo::index`] 排序（0 = 主屏，见 [`DisplayInfo`] 的说明）。
    ///
    /// 探测失败（当前会话没有可交互桌面）时返回空 `Vec`：这个函数的契约是"清单"，不是
    /// "诊断" —— 需要错误码的路径是 `screenshot`（它会区分 `Failed` 与 `HostLimit`）。
    pub fn displays() -> Vec<DisplayInfo> {
        match enumerate_displays() {
            Ok(displays) => displays,
            Err(error) => {
                tracing::debug!(error = %error, "枚举显示器失败，清单返回空");
                Vec::new()
            }
        }
    }

    /// 本机每块屏的 DPI / 缩放 / 虚拟化诊断指标（见 [`crate::DisplayMetrics`]）。
    ///
    /// 与 [`Self::displays`] 同一套枚举与排序（同一次 `EnumDisplayMonitors`、同样的三条
    /// 排序键），所以两个列表的下标是**一一对应**的；区别是这里额外量了每块屏的物理像素
    /// （`CreateDCW` + `GetDeviceCaps(DESKTOPHORZRES)`）与 DC 的 `LOGPIXELSX`。
    ///
    /// 探测失败时返回空 `Vec`（契约同 [`Self::displays`]：这是清单/诊断，不是错误码路径）。
    /// 单块屏拿不到 DC 或 DPI 时**不整体失败**，只退化成 `rcMonitor` 尺寸 / 进程 DPI 并留
    /// debug 日志 —— 一块屏量不准不该让整份诊断没有输出。
    pub fn display_metrics() -> Vec<DisplayMetrics> {
        match enumerate_monitor_facts() {
            Ok(facts) => facts
                .iter()
                .enumerate()
                .map(|(index, fact)| metrics_of(fact, index as u32))
                .collect(),
            Err(error) => {
                tracing::debug!(error = %error, "枚举显示器失败，诊断指标返回空");
                Vec::new()
            }
        }
    }

    /// 进程级 DPI（`GetDpiForSystem()`；100% = 96、150% = 144）。
    ///
    /// 这是**进程**的属性，与每块屏的 [`DisplayInfo::dpi_x`] 不同：DPI-Unaware 进程这里恒为
    /// 96，Per-Monitor V2 进程拿到主屏的有效 DPI。
    pub fn system_dpi() -> u32 {
        process_dpi()
    }
}

/// `EnumDisplayMonitors` 回调交回来的原始事实：`MONITORINFOEXW` 与它对应的 `HMONITOR`。
///
/// **为什么连 `HMONITOR` 一起留着**：`GetDpiForMonitor` 只认 `HMONITOR`，而 `MONITORINFOEXW`
/// 里没有句柄字段；排序又是按 `rcMonitor`/设备名做的，排完就找不到"哪块屏是哪个句柄"了。
/// 所以在回调里顺手把句柄存下来，排序时一起搬 —— 之后 `DisplayInfo`（加 DPI）与
/// `DisplayMetrics`（再加物理尺寸）都从同一份事实出，不会出现"清单和指标各枚举一次、
/// 序号对不上"的分叉。
struct MonitorFacts {
    /// 系统给的显示器信息（`rcMonitor`、`dwFlags`、`szDevice`）。
    info: MONITORINFOEXW,
    /// 这块屏的句柄（`GetDpiForMonitor` 的入参）。
    monitor: HMONITOR,
}

impl MonitorFacts {
    /// 是不是主显示器。
    fn primary(&self) -> bool {
        self.info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0
    }

    /// `rcMonitor`（虚拟桌面坐标系；是否被虚拟化取决于进程 DPI 档位）。
    fn rect(&self) -> RECT {
        self.info.monitorInfo.rcMonitor
    }

    /// 设备名（`\\.\DISPLAY1`）。
    fn device(&self) -> String {
        device_name(&self.info.szDevice)
    }

    /// `rcMonitor` 的宽（负宽度按 0 处理：畸形拓扑不 panic）。
    fn width(&self) -> u32 {
        let rect = self.rect();
        rect.right.saturating_sub(rect.left).max(0) as u32
    }

    /// `rcMonitor` 的高。
    fn height(&self) -> u32 {
        let rect = self.rect();
        rect.bottom.saturating_sub(rect.top).max(0) as u32
    }
}

/// `EnumDisplayMonitors` 的回调：把每块屏的 `MONITORINFOEXW` + `HMONITOR` 收集起来。
///
/// 回调返回 `1`（TRUE）表示**继续枚举**；这里永远继续（过滤交给调用方，排序更与枚举顺序无关）。
///
/// SAFETY: 契约要求 `hwnd` 由系统给出、`lparam` 是调用方传的那个 `&mut Vec<MonitorFacts>`。
unsafe extern "system" fn monitor_enum_proc(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: `EnumDisplayMonitors` 是同步调用，`lparam` 指向调用栈上的 `Vec`，回调期间有效。
    let collected = unsafe { &mut *(lparam as *mut Vec<MonitorFacts>) };
    // SAFETY: `MONITORINFOEXW` 是 POD，全零合法；`cbSize` 必须写对，否则系统拒绝。
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // `MONITORINFOEXW` 的第一个字段就是 `MONITORINFO`，`GetMonitorInfoW` 只写那一部分，
    // 所以把整个结构体的地址按 `*mut MONITORINFO` 传进去是合法的（`repr(C)` 保证布局）。
    let ok = unsafe {
        GetMonitorInfoW(
            monitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
    };
    if ok != 0 {
        collected.push(MonitorFacts { info, monitor });
    }
    1
}

/// 枚举显示器并给出**稳定的序号**，返回排好序的原始事实（[`MonitorFacts`]）。
///
/// 排序规则（三条，全部只看系统给的静态属性，不掺枚举顺序）：
///
/// 1. **主显示器排第一** —— `monitor: None` 的既有语义是"主屏"，让 `Some(0)` 与 `None`
///    指向同一块屏，向后兼容；
/// 2. 其余按 `rcMonitor` 左上角的 `(left, top)` 排 —— 也就是虚拟桌面上"从左到右、从上到下"，
///    这与用户在显示设置里看到的排布一致（副屏在主屏左边时是负坐标，自然排在最前）；
/// 3. 坐标完全相同（**克隆模式**：两块物理屏镜像同一个逻辑屏，`rcMonitor` 一模一样）时按
///    `szDevice` 字典序 —— 没有这一条，两块屏的相对次序就只能听枚举回调的（系统不保证）。
fn enumerate_monitor_facts() -> Result<Vec<MonitorFacts>, DeviceError> {
    let mut collected: Vec<MonitorFacts> = Vec::new();
    // SAFETY: `EnumDisplayMonitors` 同步调用回调；`&mut collected` 在调用期间有效且
    // 回调不保存这个指针。`NULL` hdc + `NULL` 裁剪矩形 = 枚举全部显示器。
    let ok = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(monitor_enum_proc),
            &mut collected as *mut Vec<MonitorFacts> as LPARAM,
        )
    };
    if ok == 0 {
        return Err(DeviceError::Failed {
            message: "EnumDisplayMonitors 失败：当前会话没有可用的显示（RDP 已断开 / 无交互桌面）"
                .to_string(),
        });
    }
    if collected.is_empty() {
        return Err(DeviceError::Failed {
            message: "EnumDisplayMonitors 成功但一块显示器都没枚举到".to_string(),
        });
    }

    collected.sort_by(|left, right| {
        // 1) 主屏优先（`false < true`，所以用 `b` 与 `a` 的 primary 比，让 true 排前面）。
        right
            .primary()
            .cmp(&left.primary())
            // 2) 虚拟桌面左上角，先左后上。
            .then_with(|| left.rect().left.cmp(&right.rect().left))
            .then_with(|| left.rect().top.cmp(&right.rect().top))
            // 3) 克隆模式下坐标相同，用设备名兜底，保证稳定。
            .then_with(|| left.device().cmp(&right.device()))
    });
    Ok(collected)
}

/// 枚举显示器并映射成 [`DisplayInfo`]（序号在排序后回填）。
fn enumerate_displays() -> Result<Vec<DisplayInfo>, DeviceError> {
    let facts = enumerate_monitor_facts()?;
    Ok(facts
        .iter()
        .enumerate()
        .map(|(index, fact)| display_of(fact, index as u32))
        .collect())
}

/// 一块屏 → [`DisplayInfo`]（含这块屏的 DPI 与缩放比）。
fn display_of(fact: &MonitorFacts, index: u32) -> DisplayInfo {
    let rect = fact.rect();
    let (dpi_x, dpi_y) = monitor_dpi(fact.monitor);
    DisplayInfo {
        index,
        primary: fact.primary(),
        device: fact.device(),
        x: rect.left,
        y: rect.top,
        width: fact.width(),
        height: fact.height(),
        dpi_x,
        dpi_y,
        scale_percent: scale_percent_of(dpi_x),
    }
}

/// 一块屏 → [`DisplayMetrics`]（在 [`display_of`] 之上再量物理像素与 DC 的 `LOGPIXELSX`）。
fn metrics_of(fact: &MonitorFacts, index: u32) -> DisplayMetrics {
    let device = fact.device();
    let (dpi_x, dpi_y) = monitor_dpi(fact.monitor);
    let width = fact.width();
    let height = fact.height();
    let (physical_width, physical_height, dc_log_pixels_x) =
        physical_size_of(&device, width, height);
    DisplayMetrics {
        index,
        device,
        primary: fact.primary(),
        x: fact.rect().left,
        y: fact.rect().top,
        width,
        height,
        dpi_x,
        dpi_y,
        scale_percent: scale_percent_of(dpi_x),
        physical_width,
        physical_height,
        dc_log_pixels_x,
        system_dpi: process_dpi(),
    }
}

/// 量一块屏的物理像素宽高与 DC 的 `LOGPIXELSX`：`(宽, 高, log_pixels_x)`。
///
/// 数据源是 `CreateDCW(szDevice)` + `GetDeviceCaps`：
///
/// - `DESKTOPHORZRES`/`DESKTOPVERTRES`（118/117）是**设备**的像素数，也就是物理分辨率；
/// - `LOGPIXELSX`（88）是这块 DC 的逻辑 DPI —— DPI-Aware 进程拿到的是真值（150% = 144），
///   DPI-Unaware 进程被系统钉成 96。**这一项是虚拟化的直接证据**，比推算可靠。
///
/// 拿不到 DC / `GetDeviceCaps` 返回非正数时退化成 `rcMonitor` 的宽高（并留 debug 日志）：
/// 诊断 API 的契约是"永远给得出数字"，不是"拿不到就报错"。
fn physical_size_of(device: &str, fallback_width: u32, fallback_height: u32) -> (u32, u32, u32) {
    let Ok(source) = SourceDc::device(device) else {
        // `SourceDc::device` 自己会给出 `CreateDCW 失败` 的错误文案；这里只记退化的后果。
        tracing::debug!(device, "拿不到显示器 DC，物理尺寸退化为 rcMonitor");
        return (fallback_width, fallback_height, 0);
    };
    // SAFETY: `source` 刚刚建成且尚未释放，`raw()` 给出的 DC 在 `release()` 之前一直有效；
    // `GetDeviceCaps` 只读这个 DC 的能力表，不改状态。
    let (horz, vert, log_pixels) = unsafe {
        (
            GetDeviceCaps(source.raw(), DESKTOPHORZRES as i32),
            GetDeviceCaps(source.raw(), DESKTOPVERTRES as i32),
            GetDeviceCaps(source.raw(), LOGPIXELSX as i32),
        )
    };
    // SAFETY: DC 由本函数创建、尚未释放，这里用完就还 —— 满足 `release` 的"只调用一次"契约。
    unsafe { source.release() };
    let positive = |value: i32, fallback: u32| {
        if value > 0 { value as u32 } else { fallback }
    };
    (
        positive(horz, fallback_width),
        positive(vert, fallback_height),
        positive(log_pixels, 0),
    )
}

/// 一块屏的有效 DPI `(横轴, 纵轴)`。
///
/// 首选 `GetDpiForMonitor(MDT_EFFECTIVE_DPI)`（Win8.1+，混合缩放时两块屏可以不同）；
/// 调用失败时退化到进程 DPI（`GetDpiForSystem`），再不行按 96 算 —— 退化的原因记在
/// `tracing::debug`，不静默编数字。
fn monitor_dpi(monitor: HMONITOR) -> (u32, u32) {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    // SAFETY: `monitor` 来自 `EnumDisplayMonitors`（调用期间有效）；两个输出参数指向栈上
    // 的 `u32`，`GetDpiForMonitor` 只在成功时写它们。
    let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if hr >= 0 && dpi_x > 0 && dpi_y > 0 {
        return (dpi_x, dpi_y);
    }
    let fallback = process_dpi();
    tracing::debug!(
        hr,
        fallback,
        "GetDpiForMonitor 失败，每屏 DPI 退化为进程 DPI"
    );
    (fallback, fallback)
}

/// 进程级 DPI（`GetDpiForSystem()`）。
///
/// 老系统（Win10 1607 之前）没有这个 API，`windows-sys` 的静态导入会直接让进程加载失败 ——
/// 这不是本函数能兜的，本 crate 的目标就是 Windows 10/11（见 crate 文档的平台矩阵）。
/// 运行期返回 0（理论上不会）时按 96 算。
fn process_dpi() -> u32 {
    // SAFETY: 无参数、无副作用，只读系统设置。
    let dpi = unsafe { GetDpiForSystem() };
    if dpi > 0 { dpi } else { 96 }
}

/// DPI → 缩放百分比（相对 96 的四舍五入）。
///
/// 96→100、120→125、144→150、168→175、192→200。用四舍五入而不是截断：Windows 的"自定义
/// 缩放"允许 10% 步进，截断会把 115% 报成 114%，看着像算错了。
fn scale_percent_of(dpi: u32) -> u32 {
    if dpi == 0 {
        return 0;
    }
    ((u64::from(dpi) * 100 + 48) / 96) as u32
}

/// 把 `MONITORINFOEXW::szDevice`（UTF-16，未终止时也要能读）转成 `String`。
fn device_name(raw: &[u16]) -> String {
    let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

// ---------------------------------------------------------------- 截图

/// 一张已解码的图像（RGB8，无 alpha）。
struct Frame {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

/// 抓图用的**源 DC**，以及它的释放方式。
///
/// 三种 DC 的释放 API 不一样，混用会泄漏 GDI 对象（每进程上限 10000 个，泄漏到顶之后
/// 整个进程的 `CreateDIBSection` 开始失败，表现成"截图突然坏了"）：
///
/// - `GetDC(NULL)`（主屏）→ `ReleaseDC(NULL, dc)`；
/// - `CreateDCW(设备名)`（某块显示器）→ `DeleteDC(dc)`；
/// - `GetWindowDC(hwnd)`（单个窗口）→ `ReleaseDC(hwnd, dc)`。
///
/// 把"DC + 怎么还"绑在一起，是为了让 [`Surface`] 的 `Drop` 不可能还错。
enum SourceDc {
    /// `GetDC(NULL)`：只覆盖**主显示器**。
    Screen(HDC),
    /// `CreateDCW(szDevice)`：一整块显示器自己的 DC，原点在这块屏的左上角。
    Device(HDC),
    /// `GetWindowDC(hwnd)`：整个窗口（含非客户区）的 DC，原点在窗口左上角。
    Window(HWND, HDC),
}

impl SourceDc {
    /// 取 `GetDC(NULL)`（主屏）。
    fn screen() -> Result<Self, DeviceError> {
        // SAFETY: `GetDC(NULL)` 取整个屏幕的 DC，返回空指针表示当前会话没有可用显示。
        let dc = unsafe { GetDC(ptr::null_mut()) };
        if dc.is_null() {
            return Err(DeviceError::Failed {
                message: "GetDC(NULL) 失败：当前会话没有可用的显示（RDP 已断开 / 无交互桌面）"
                    .to_string(),
            });
        }
        Ok(Self::Screen(dc))
    }

    /// 取某块显示器自己的 DC。
    ///
    /// 为什么不用 `GetDC(NULL)` + 偏移：`GetDC(NULL)` 的 DC 只覆盖主显示器，用它去
    /// `BitBlt` 副屏坐标拿到的是**主屏在那些坐标上的像素**（常见是全黑），不是副屏内容。
    /// `CreateDCW(szDevice)` 直接给出这块屏自己的绘制表面，原点就在它左上角。
    fn device(device: &str) -> Result<Self, DeviceError> {
        let name: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` 以 NUL 结尾且在调用期间有效；其余三个参数为 NULL = 用设备名建 DC。
        let dc = unsafe { CreateDCW(ptr::null(), name.as_ptr(), ptr::null(), ptr::null()) };
        if dc.is_null() {
            return Err(DeviceError::Failed {
                message: format!("CreateDCW 失败：建不出显示器 {device} 的 DC"),
            });
        }
        Ok(Self::Device(dc))
    }

    /// 取一个窗口（含边框与标题栏）的 DC。
    fn window(hwnd: HWND) -> Result<Self, DeviceError> {
        // SAFETY: `hwnd` 由调用方保证是有效窗口句柄（来自 EnumWindows / GetForegroundWindow）。
        let dc = unsafe { GetWindowDC(hwnd) };
        if dc.is_null() {
            return Err(DeviceError::Failed {
                message: "GetWindowDC 失败：窗口可能已经销毁".to_string(),
            });
        }
        Ok(Self::Window(hwnd, dc))
    }

    /// 底层句柄（`BitBlt` 的源）。
    fn raw(&self) -> HDC {
        match self {
            Self::Screen(dc) | Self::Device(dc) | Self::Window(_, dc) => *dc,
        }
    }

    /// 按各自的 API 释放。**只调用一次**（由 [`Surface::drop`] 负责）。
    ///
    /// SAFETY: 句柄必须来自本枚举的构造器，且尚未释放。
    unsafe fn release(self) {
        // SAFETY: 见本方法的契约；三种释放 API 与构造器一一对应。
        unsafe {
            match self {
                Self::Screen(dc) => {
                    ReleaseDC(ptr::null_mut(), dc);
                }
                Self::Device(dc) => {
                    DeleteDC(dc);
                }
                Self::Window(hwnd, dc) => {
                    ReleaseDC(hwnd, dc);
                }
            }
        }
    }
}

/// 一块 GDI 离屏表面（源 DC + 内存 DC + 32bpp 自顶向下 DIB）。
///
/// `Drop` 里按**创建的反序**拆：先恢复原来的位图，再删位图，再删 DC，最后还源 DC。
/// 顺序错了（先删位图再 `SelectObject` 回去）会用到已释放的句柄。
struct Surface {
    /// 源 DC。`Option` 只为让 [`Drop`] 能**取走**它再按各自 API 释放（`release` 拿所有权）；
    /// `Surface` 存活期间它恒为 `Some`。
    source: Option<SourceDc>,
    mem_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

impl Surface {
    /// 建一块 `width × height`、32bpp、**自顶向下**（`biHeight` 为负）的 DIB 表面，源 DC 是主屏。
    ///
    /// 自顶向下是为了让 `bits` 的第一行就是屏幕最上面一行，省掉翻转。
    fn new(width: i32, height: i32) -> Result<Self, DeviceError> {
        let source = SourceDc::screen()?;
        Self::with_source(source, width, height)
    }

    /// 同上，但源 DC 由调用方给（某块显示器 / 某个窗口）。
    ///
    /// 源 DC 的**所有权转进来**：任何失败路径都由这里按 [`SourceDc::release`] 还掉，
    /// 成功路径由 [`Surface::drop`] 还 —— 不会有两处都还或两处都不还。
    fn with_source(source: SourceDc, width: i32, height: i32) -> Result<Self, DeviceError> {
        if width <= 0 || height <= 0 {
            // SAFETY: `source` 刚建、未释放。
            unsafe { source.release() };
            return Err(DeviceError::Failed {
                message: format!("截图区域尺寸非法（{width}×{height}）"),
            });
        }
        if width > MAX_SCREENSHOT_DIMENSION || height > MAX_SCREENSHOT_DIMENSION {
            // SAFETY: 同上。
            unsafe { source.release() };
            return Err(DeviceError::Failed {
                message: format!(
                    "截图区域过大（{width}×{height}，单边上限 {MAX_SCREENSHOT_DIMENSION}）"
                ),
            });
        }
        if u64::from(width as u32) * u64::from(height as u32) > MAX_SCREENSHOT_PIXELS {
            // SAFETY: 同上。
            unsafe { source.release() };
            return Err(DeviceError::Failed {
                message: format!(
                    "截图区域过大（{width}×{height}，总像素上限 {MAX_SCREENSHOT_PIXELS}）"
                ),
            });
        }

        // SAFETY: 与源 DC 兼容的内存 DC。
        let mem_dc = unsafe { CreateCompatibleDC(source.raw()) };
        if mem_dc.is_null() {
            // SAFETY: `source` 有效且未释放。
            unsafe { source.release() };
            return Err(DeviceError::Failed {
                message: "CreateCompatibleDC 失败：GDI 句柄可能已耗尽".to_string(),
            });
        }

        // SAFETY: `BITMAPINFO` 是 POD，全零合法；随后逐字段写满需要的值。
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 负数 = 自顶向下。
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: `&info` 在调用期间有效；`&mut bits` 指向栈上的局部变量；
        // 传 `NULL` 段 + `DIB_RGB_COLORS` 表示"自己分配像素内存"。
        let bitmap = unsafe {
            CreateDIBSection(
                source.raw(),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                ptr::null_mut(),
                0,
            )
        };
        if bitmap.is_null() || bits.is_null() {
            // SAFETY: `mem_dc` 非空且由 `CreateCompatibleDC` 创建；`source` 未释放。
            unsafe {
                DeleteDC(mem_dc);
                source.release();
            }
            return Err(DeviceError::Failed {
                message: "CreateDIBSection 失败：内存不足或 GDI 句柄耗尽".to_string(),
            });
        }

        // SAFETY: 两个句柄都有效；`SelectObject` 返回被替换下来的旧对象（可能就是 NULL）。
        let previous = unsafe { SelectObject(mem_dc, bitmap) };
        if previous.is_null() {
            // SAFETY: 句柄都有效，且 `bitmap` 还没被选进 DC，直接删是安全的。
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(mem_dc);
                source.release();
            }
            return Err(DeviceError::Failed {
                message: "SelectObject 失败：位图没能选进内存 DC".to_string(),
            });
        }

        Ok(Self {
            source: Some(source),
            mem_dc,
            bitmap,
            previous,
            bits,
            width,
            height,
        })
    }

    /// 源 DC 的裸句柄（`BitBlt` 的源）。只在 `Surface` 存活期间调用，所以必然是 `Some`。
    fn source_dc(&self) -> HDC {
        self.source
            .as_ref()
            .map(SourceDc::raw)
            .unwrap_or(ptr::null_mut())
    }

    /// 把像素读成一份**自有**的 BGRA 拷贝（不做任何转换）。
    ///
    /// 两处共用：截图要的是 RGB（[`bgra_to_rgb`] 丢 alpha），OCR 要的正是 BGRA 本体
    /// （`SoftwareBitmap` 的 `Bgra8`）。抓图只有这一份 —— `Surface` 里的 `bits` 是 DIB 的
    /// 映射内存，`Surface` 一 drop 就没了，所以必须拷出来。
    fn read_bgra(&self) -> Result<(u32, u32, Vec<u8>), DeviceError> {
        let width = self.width as u32;
        let height = self.height as u32;
        let expected = width as usize * height as usize * 4;
        // SAFETY: `bits` 指向 `CreateDIBSection` 分配的 `width*height*4` 字节；
        // 这块内存在 `self` 存活期间有效，且不与其它借用重叠。
        let bgra = unsafe { std::slice::from_raw_parts(self.bits as *const u8, expected) };
        Ok((width, height, bgra.to_vec()))
    }

    /// 把像素读成 RGB8。逐行去掉 4 字节里的 alpha（`BI_RGB` 下它无定义，不能当不透明用）。
    fn read_frame(&self) -> Result<Frame, DeviceError> {
        let (width, height, bgra) = self.read_bgra()?;
        Ok(Frame {
            width,
            height,
            rgb: bgra_to_rgb(&bgra, width, height)?,
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: 这些句柄都是 `Surface::with_source` 成功时取得的，且只在这里释放一次。
        // 顺序：先把旧位图选回去（否则删位图时它还在 DC 里），再删位图、内存 DC，最后还源 DC。
        unsafe {
            SelectObject(self.mem_dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.mem_dc);
        }
        // SAFETY: `source` 只在 `with_source` 里建过一次，且只在这里释放（取走后 `source`
        // 恒为 `None`，重复 drop 是不可能的 —— `Drop::drop` 只跑一次）。
        if let Some(source) = self.source.take() {
            unsafe { source.release() };
        }
    }
}

/// 抓一帧：**先试前台窗口的 `PrintWindow`，失败退化成主显示器 `BitBlt`**。
fn capture_frame() -> Result<Frame, DeviceError> {
    match capture_foreground_window() {
        Ok(frame) => Ok(frame),
        Err(error) => {
            tracing::debug!(
                error = %error,
                "PrintWindow 抓前台窗口失败，退化为主显示器 BitBlt"
            );
            capture_primary_screen()
        }
    }
}

/// 用 `PrintWindow(PW_RENDERFULLCONTENT)` 抓前台窗口的**整窗**（含标题栏与边框）。
fn capture_foreground_window() -> Result<Frame, DeviceError> {
    // SAFETY: 无参数，返回前台窗口句柄（可能为空）。
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return Err(DeviceError::Failed {
            message: "没有前台窗口，不知道要截哪个窗口".to_string(),
        });
    }
    // SAFETY: `RECT` 是 POD，全零合法；`GetWindowRect` 只在成功时写入。
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: `hwnd` 是刚取得的前台窗口句柄；`&mut rect` 指向栈上有效内存。
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(DeviceError::Failed {
            message: "GetWindowRect 失败：前台窗口可能已经销毁".to_string(),
        });
    }
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    let surface = Surface::new(width, height)?;
    // SAFETY: `hwnd` 有效；`surface.mem_dc` 是有效的内存 DC 且已选入同尺寸位图。
    // `PW_RENDERFULLCONTENT` 让 DWM 内容也参与渲染（对部分 UWP/合成窗口是必需的）。
    let printed = unsafe { PrintWindow(hwnd, surface.mem_dc, PW_RENDERFULLCONTENT) };
    if printed == 0 {
        return Err(DeviceError::Failed {
            message: "PrintWindow 返回 0：窗口可能拒绝渲染或被 DWM 保护".to_string(),
        });
    }
    surface.read_frame()
}

/// 退化路径：抓主显示器整屏。
///
/// 主显示器的原点就是虚拟桌面的 (0, 0)，所以 `SM_CXSCREEN/SM_CYSCREEN` 配 `BitBlt(…,0,0)`
/// 就够了，不需要 `SM_XVIRTUALSCREEN` 那套偏移（那套只在 click 的坐标归一化里用）。
fn capture_primary_screen() -> Result<Frame, DeviceError> {
    blit_primary_screen()?.read_frame()
}

/// 把主显示器整屏 `BitBlt` 进一张自带像素内存的 DIB，**原样保留 BGRA**。
///
/// 截图与 OCR 共用这一段：截图关心的是像素内容（`read_frame` 丢 alpha 转 RGB），OCR 关心的是
/// 同一份 BGRA（`read_bgra`）。以前这段逻辑直接长在 `capture_primary_screen` 里，为了 OCR
/// 再抄一遍 `BitBlt` 是错的 —— 抓图口径只有一份。
fn blit_primary_screen() -> Result<Surface, DeviceError> {
    // SAFETY: 纯查询，失败返回 0。
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let surface = Surface::new(width, height)?;
    blit_into(
        &surface,
        width,
        height,
        "BitBlt 失败：屏幕 DC 可能不可读（独占全屏或受保护内容）",
    )?;
    Ok(surface)
}

/// 把某块显示器**整屏** `BitBlt` 进一张自带像素内存的 DIB。
///
/// 与 [`blit_primary_screen`] 的区别只在源 DC：这里用 `CreateDCW(szDevice)` 取那块屏
/// 自己的 DC（`GetDC(NULL)` 只覆盖主屏）。抓图口径仍只有一份（[`blit_into`]）。
fn blit_display(display: &DisplayInfo) -> Result<Surface, DeviceError> {
    let width = display.width as i32;
    let height = display.height as i32;
    let source = SourceDc::device(&display.device)?;
    let surface = Surface::with_source(source, width, height)?;
    blit_into(
        &surface,
        width,
        height,
        "BitBlt 失败：显示器 DC 可能不可读（独占全屏或受保护内容）",
    )?;
    Ok(surface)
}

/// 把某个窗口（含边框与标题栏）的像素 `BitBlt` 进 DIB —— `PrintWindow` 失败时的退路。
fn blit_window(hwnd: HWND, width: i32, height: i32) -> Result<Surface, DeviceError> {
    let source = SourceDc::window(hwnd)?;
    let surface = Surface::with_source(source, width, height)?;
    blit_into(
        &surface,
        width,
        height,
        "BitBlt 失败：窗口 DC 可能不可读（窗口已销毁或被保护）",
    )?;
    Ok(surface)
}

/// 从表面的源 DC `(0, 0)` 起拷 `width × height` 到它的 DIB。两条抓图路径共用。
fn blit_into(surface: &Surface, width: i32, height: i32, failure: &str) -> Result<(), DeviceError> {
    // SAFETY: 源 DC 与内存 DC 都有效且尺寸匹配；`SRCCOPY` 是不做变换的逐位拷贝。
    let copied = unsafe {
        BitBlt(
            surface.mem_dc,
            0,
            0,
            width,
            height,
            surface.source_dc(),
            0,
            0,
            SRCCOPY,
        )
    };
    if copied == 0 {
        return Err(DeviceError::Failed {
            message: failure.to_string(),
        });
    }
    Ok(())
}

/// 用 `PrintWindow(PW_RENDERFULLCONTENT)` 把窗口画进一张 DIB（不读屏幕，走窗口自己的绘制）。
fn print_window(hwnd: HWND, width: i32, height: i32) -> Result<Surface, DeviceError> {
    let surface = Surface::new(width, height)?;
    // SAFETY: `hwnd` 有效；`surface.mem_dc` 是有效的内存 DC 且已选入同尺寸位图。
    // `PW_RENDERFULLCONTENT` 让 DWM 内容也参与渲染（对部分 UWP/合成窗口是必需的）。
    let printed = unsafe { PrintWindow(hwnd, surface.mem_dc, PW_RENDERFULLCONTENT) };
    if printed == 0 {
        return Err(DeviceError::Failed {
            message: "PrintWindow 返回 0：窗口可能拒绝渲染或被 DWM 保护".to_string(),
        });
    }
    Ok(surface)
}

// ---------------------------------------------------------------- 按窗口 OCR 的窗口查找

/// 按窗口 OCR 时"可读窗口"的判据，四条（顺序就是筛选顺序，全部必须满足）：
///
/// 1. `IsWindowVisible` —— 不可见的窗口没有屏幕内容可抓；
/// 2. `IsIconic` 为假 —— **最小化**的窗口要么抓出来是黑块，要么抓到的是任务栏按钮那一小块，
///    与其给模型一张错图，不如说"现在没有可读的窗口"；
/// 3. 标题非空 —— 无标题窗口基本是消息窗口、IME 候选框、托盘宿主这类，它们的"内容"不是用户
///    想读的界面；
/// 4. 进程映像名（小写，`QueryFullProcessImageNameW` → [`process_name`]）与请求**不区分大小写**
///    地相等。
///
/// **多块匹配时的次序**：先看 `GetForegroundWindow`，它是就选它（用户正在看的那个窗口最可能
/// 是他要的）；否则取 `EnumWindows` 的**第一个**命中 —— `EnumWindows` 按 Z 序**从上到下**
/// 枚举，所以第一个就是"最上面那个"。
///
/// 与 [`enum_window_proc`] 的一处**故意不同**：这里**不跳过本进程的窗口**。`windows()` 跳过
/// 自己的窗口是为了别让模型点到自己；OCR 是只读的，没有回环风险，而且进程读自己的窗口是
/// 测试"按窗口 OCR 全链路"的唯一可行做法（测试进程必须能读到它自己建的窗口）。
fn find_window_for_process(process: &str) -> Option<HWND> {
    // 只比映像名：调用方可能传 `C:\Windows\notepad.exe`，也可能传 `notepad.exe`。
    let wanted = process_name(process);
    if wanted.is_empty() {
        return None;
    }

    // 1) 前台窗口优先。
    // SAFETY: 无参数，返回前台窗口句柄（可能为空）。
    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.is_null() && window_matches_process(foreground, &wanted) {
        return Some(foreground);
    }

    // 2) 否则 Z 序最前（`EnumWindows` 从上到下）。
    let mut search = WindowSearch {
        wanted,
        found: None,
    };
    // SAFETY: `EnumWindows` 同步调用回调；`&mut search` 在调用期间有效，回调不保存这个指针。
    unsafe {
        EnumWindows(
            Some(find_window_proc),
            &mut search as *mut WindowSearch as LPARAM,
        )
    };
    search.found
}

/// [`find_window_for_process`] 用到的枚举上下文。
struct WindowSearch {
    /// 已经过 [`process_name`] 归一化（小写）的映像名。
    wanted: String,
    /// 第一个命中的窗口（Z 序最上面那个）。
    found: Option<HWND>,
}

/// 单个窗口是否满足"可读窗口"的四条判据（见 [`find_window_for_process`]）。
fn window_matches_process(hwnd: HWND, wanted: &str) -> bool {
    // SAFETY: `hwnd` 由调用方保证来自系统枚举或 `GetForegroundWindow`。
    if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 {
        return false;
    }
    if window_title(hwnd).is_empty() {
        return false;
    }
    // SAFETY: `hwnd` 同上；`&mut pid` 指向栈上变量。
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return false;
    }
    process_name_of_pid(pid).is_some_and(|name| name.eq_ignore_ascii_case(wanted))
}

/// `EnumWindows` 回调：找第一个满足判据的窗口，找到就返回 `0`（FALSE）**停止枚举**。
///
/// SAFETY: 契约要求 `lparam` 是调用方传的 `&mut WindowSearch`，回调期间有效。
unsafe extern "system" fn find_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: 见本函数的契约。
    let search = unsafe { &mut *(lparam as *mut WindowSearch) };
    if search.found.is_some() {
        // 停下来：Z 序第一个命中就是"最上面那个"，再往后只会更靠下。
        return 0;
    }
    // SAFETY: `hwnd` 由系统给出。
    if window_matches_process(hwnd, &search.wanted) {
        search.found = Some(hwnd);
        return 0;
    }
    1
}

/// 抓一个窗口的位图（给 OCR 用）：**先 `PrintWindow`，失败或画出来全黑就退 `GetWindowDC` + `BitBlt`**。
///
/// 为什么留退路（两条都是实测出来的）：
///
/// 1. `PrintWindow` 对部分窗口返回 0（拒绝渲染、被 DWM 保护、纯 GDI 且不处理 `WM_PRINT`）；
/// 2. **更阴的一条：`PrintWindow` 返回 1，但位图一个像素都没画**（新 DIB 是全零，于是整张图纯黑）。
///    实测场景：窗口刚 `ShowWindow` 完、DWM 还没合成第一帧时立刻抓图 —— 返回 1、位图全零。
///    如果只看返回值，这里就会把"没抓到"当成"抓到了纯黑窗口"，然后 OCR 返回空串，
///    调用方无法区分"窗口里没字"和"我们根本没抓到图"——那正是契约禁止的"空串冒充成功"。
///    所以退路判据是**内容**（全零）而不只是返回值。
/// 3. 两条都拿不到内容时**明确失败**（`Failed`），不返回空位图。
fn capture_window_for_ocr(hwnd: HWND) -> Result<Surface, DeviceError> {
    // SAFETY: `RECT` 是 POD，全零合法；`GetWindowRect` 只在成功时写入。
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: `hwnd` 由调用方保证有效；`&mut rect` 指向栈上有效内存。
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(DeviceError::Failed {
            message: "GetWindowRect 失败：窗口可能已经销毁".to_string(),
        });
    }
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);

    let mut printed_nothing = false;
    match print_window(hwnd, width, height) {
        Ok(surface) if !surface_is_blank(&surface) => return Ok(surface),
        Ok(_) => {
            printed_nothing = true;
            tracing::debug!(
                "PrintWindow 返回成功但位图全零（DWM 还没合成这一帧？），退化为窗口 DC 的 BitBlt"
            );
        }
        Err(error) => {
            tracing::debug!(
                error = %error,
                "PrintWindow 抓窗口失败，退化为窗口 DC 的 BitBlt"
            );
        }
    }

    let surface = blit_window(hwnd, width, height)?;
    if surface_is_blank(&surface) {
        return Err(DeviceError::Failed {
            message: format!(
                "窗口位图是空的（{width}x{height} 全零）：PrintWindow 没能渲染这块窗口\
                 （DWM 保护 / DRM 内容 / 窗口尚未合成），从窗口 DC 也读不到像素"
            ),
        });
    }
    if printed_nothing {
        tracing::debug!("窗口 DC 的 BitBlt 拿到了内容（PrintWindow 那条路是空的）");
    }
    Ok(surface)
}

/// 位图是不是"一个像素都没画"（全零 = 纯黑）。
///
/// 只把**纯全零**当空：GDI 新建的 DIB 就是全零，任何真的被渲染过的窗口（哪怕深色主题）
/// 都会有抗锯齿边缘或非零背景色。判"全零"而不是"接近黑"是为了不把深色窗口误判成没抓到。
fn surface_is_blank(surface: &Surface) -> bool {
    match surface.read_bgra() {
        Ok((_, _, bgra)) => bgra.iter().all(|byte| *byte == 0),
        // 读不出像素就没法证明它是空的；交给后面的识别链路去报真正的错误。
        Err(_) => false,
    }
}

/// BGRA → RGB：丢掉 alpha（`BI_RGB` 下它是垃圾值），保留行序。
fn bgra_to_rgb(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DeviceError> {
    let expected = width as usize * height as usize * 4;
    if bgra.len() < expected {
        return Err(DeviceError::Failed {
            message: format!(
                "像素缓冲区长度不对：期望 {expected} 字节，实际 {}",
                bgra.len()
            ),
        });
    }
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in bgra[..expected].chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    Ok(rgb)
}

// ---------------------------------------------------------------- OCR（WinRT Windows.Media.Ocr）

// 本线程的 WinRT 公寓状态：**每线程只初始化一次**。
//
// 这里踩过两个坑，两条都是实测出来的（别"修"回去）：
//
// 1. **绝不能 `CoUninitialize`**。`windows` crate 把 WinRT 的**激活工厂**缓存在进程级静态表里
//    （`factory_cache.rs`：第一次 `RoGetActivationFactory` 之后不再问系统）。缓存的是敏捷工厂，
//    但用它激活出来的对象仍属于某个公寓；把公寓拆掉之后，静态表里那份工厂会变成悬空引用 ——
//    实测：第一次建引擎成功、`CoUninitialize` 之后再建就 `STATUS_ACCESS_VIOLATION`（0xC0000005）。
//
// 2. **光"不清算"还不够，必须显式把 MTA 钉在进程上**（`hold_mta_for_process`）。原因是
//    `CoInitializeEx` 的引用计数是**按线程**的：线程退出时 combase 在 DLL_THREAD_DETACH 里
//    会把它那份还回去，最后一个引用消失就把 MTA 拆了 —— 而工厂缓存是**进程级**的，于是
//    "线程 A 建过引擎、A 退出、线程 B 再建" 必炸。这条不是理论推断：测试里就是 libtest
//    每个测试一个线程，A 跑完 capabilities（建引擎）、B 再 OCR → AV。
//    `CoIncrementMTAUsage` 的计数只有 `CoDecrementMTAUsage` 能还，我们**永不**还它，
//    于是 MTA 活到进程结束，工厂缓存不再悬空。
thread_local! {
    static APARTMENT: Result<(), String> = initialize_apartment();
}

/// 进程级的 MTA 持有：只加一次，且**刻意永不** `CoDecrementMTAUsage`（见上面第 2 条）。
static MTA_HOLD: OnceLock<Result<(), String>> = OnceLock::new();

fn hold_mta_for_process() -> Result<(), String> {
    MTA_HOLD
        .get_or_init(|| {
            let mut cookie: CO_MTA_USAGE_COOKIE = ptr::null_mut();
            // SAFETY: `pcookie` 是合法的出参指针；cookie 由 combase 写入，我们不还它。
            let hr = unsafe { CoIncrementMTAUsage(&mut cookie) };
            if hr < 0 {
                return Err(format!(
                    "WinRT 不可用：CoIncrementMTAUsage 失败（HRESULT 0x{hr:08X}）"
                ));
            }
            Ok(())
        })
        .clone()
}

/// 初始化当前线程的公寓（MTA）。返回的错误是给人看的原因，含 `HRESULT`。
fn initialize_apartment() -> Result<(), String> {
    // 先让 MTA 活到进程结束，再让本线程加入它（顺序反了也"能用"，但就没有那层保护）。
    hold_mta_for_process()?;
    // SAFETY: `pvreserved` 必须是 NULL；`COINIT_MULTITHREADED` 是合法的公寓类型。
    let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED as u32) };
    match hr {
        // S_OK / S_FALSE：本线程现在在 MTA 里。
        0 | S_FALSE_HRESULT => Ok(()),
        // 宿主已经把本线程初始化成 STA 了：公寓是存在的，激活与轮询照样能用。
        RPC_E_CHANGED_MODE => Ok(()),
        other => Err(format!(
            "WinRT 不可用：CoInitializeEx 失败（HRESULT 0x{other:08X}）"
        )),
    }
}

/// 确保当前线程有公寓。失败 → `HostLimit`（这是宿主/会话限制，不是识别失败）。
fn ensure_apartment() -> Result<(), DeviceError> {
    APARTMENT.with(|state| match state {
        Ok(()) => Ok(()),
        Err(reason) => Err(DeviceError::HostLimit {
            detail: reason.clone(),
        }),
    })
}

/// 探测「本机能不能建出 OCR 引擎」，结果缓存（见 [`OCR_ENGINE_PROBE`]）。
fn ocr_engine_probe() -> &'static Result<(), String> {
    OCR_ENGINE_PROBE.get_or_init(|| {
        // 建引擎本身就要激活 WinRT 类，也得有公寓。
        if let Err(reason) = ensure_apartment() {
            return Err(reason.to_string());
        }
        match OcrEngine::TryCreateFromUserProfileLanguages() {
            Ok(_) => Ok(()),
            Err(error) => {
                let message = format!(
                    "建不出 Windows.Media.Ocr 引擎（HRESULT 0x{:08X}）：系统可能没装 OCR 语言包\
                     （设置 → 时间和语言 → 语言 → 添加语言时勾选“光学字符识别”），或当前会话受限",
                    error.code().0
                );
                tracing::debug!(%message, "OCR 引擎探测失败");
                Err(message)
            }
        }
    })
}

/// 建一个 OCR 引擎实例。**每次 OCR 现建**，不缓存对象：`OcrEngine` 不是 `Send`，把实例揣在
/// 静态里会被迫处理跨线程/公寓问题，而建一个引擎只要几毫秒。
fn create_ocr_engine() -> Result<OcrEngine, DeviceError> {
    if let Err(reason) = ocr_engine_probe() {
        return Err(DeviceError::HostLimit {
            detail: reason.clone(),
        });
    }
    let engine = OcrEngine::TryCreateFromUserProfileLanguages();
    engine.map_err(|error| DeviceError::HostLimit {
        detail: format!(
            "建不出 Windows.Media.Ocr 引擎（HRESULT 0x{:08X}）：{error}",
            error.code().0
        ),
    })
}

/// 本机装了哪些 OCR 识别语言（返回 BCP-47 标签，例如 `en-US`、`zh-Hans-CN`）。
///
/// 这是"中文到底能不能识别"的**唯一实测口径**：语言清单里有中文标签，才说明这台机器装了
/// 中文 OCR 语言包；`TryCreateFromUserProfileLanguages` 走的是**用户配置的**语言，与这个
/// 全集不是一回事（用户可以只让引擎认英文）。所以两条都要看：
///
/// - 引擎建不出来 → `Err`（与 [`ocr_engine_probe`] 同源的 `HostLimit` 文案）；
/// - 引擎能建、清单里没有中文 → 引擎能用，只是**认不了中文**（不是错误，是事实）。
fn available_ocr_languages() -> Result<Vec<String>, String> {
    if let Err(reason) = ocr_engine_probe() {
        return Err(reason.clone());
    }
    let languages = OcrEngine::AvailableRecognizerLanguages().map_err(|error| {
        format!(
            "OcrEngine::AvailableRecognizerLanguages 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        )
    })?;
    let count = languages.Size().map_err(|error| {
        format!(
            "IVectorView<Language>::Size 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        )
    })?;
    let mut tags = Vec::with_capacity(count as usize);
    for index in 0..count {
        let language = languages.GetAt(index).map_err(|error| {
            format!(
                "IVectorView<Language>::GetAt({index}) 失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            )
        })?;
        let tag = language.LanguageTag().map_err(|error| {
            format!(
                "Language::LanguageTag 失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            )
        })?;
        tags.push(tag.to_string_lossy());
    }
    Ok(tags)
}

/// 把 BGRA 字节变成 OCR 能吃的 `SoftwareBitmap`。
///
/// `CreateCopyWithAlphaFromBuffer` 收的是 `IBuffer` 而不是 `&[u8]`，所以中间用 `DataWriter`
/// （`WriteBytes` + `DetachBuffer`）转一道 —— 这是 `Storage_Streams` 这个 feature 的唯一用途。
///
/// alpha 用 [`BitmapAlphaMode::Ignore`]：GDI 的 `BI_RGB` DIB **不定义** alpha 字节，
/// 按 `Premultiplied` 解释会把那些垃圾值当成"已经乘过 alpha 的颜色"进而压暗，深色文字容易糊掉。
fn software_bitmap_from_bgra(
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<SoftwareBitmap, DeviceError> {
    let writer = DataWriter::new().map_err(|error| DeviceError::Failed {
        message: format!(
            "DataWriter::new 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        ),
    })?;
    writer
        .WriteBytes(bgra)
        .map_err(|error| DeviceError::Failed {
            message: format!(
                "DataWriter::WriteBytes 失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            ),
        })?;
    let buffer = writer.DetachBuffer().map_err(|error| DeviceError::Failed {
        message: format!(
            "DataWriter::DetachBuffer 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        ),
    })?;
    SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
        BitmapAlphaMode::Ignore,
    )
    .map_err(|error| DeviceError::Failed {
        message: format!(
            "SoftwareBitmap::CreateCopyWithAlphaFromBuffer 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        ),
    })
}

/// 一帧 BGRA8 → WinRT OCR → **未截断**的识别文本。
///
/// 错误分流（这条是硬要求，不许把两者混成一个空串）：
/// - 引擎建不出来（没语言包 / 会话受限）、图像超过引擎上限 → [`DeviceError::HostLimit`]；
/// - 交给引擎之后的任何一步失败（建位图、异步操作超时、取结果）→ [`DeviceError::Failed`]。
///
/// 空字符串是**合法**返回值：那表示"这一帧里没识别到字"。
fn recognize_bgra(width: u32, height: u32, bgra: &[u8]) -> Result<String, DeviceError> {
    if width == 0 || height == 0 {
        return Err(DeviceError::Failed {
            message: format!("OCR 输入尺寸非法：{width}x{height}"),
        });
    }
    let expected = width as usize * height as usize * 4;
    if bgra.len() < expected {
        return Err(DeviceError::Failed {
            message: format!(
                "OCR 像素缓冲区长度不对：期望 {expected} 字节，实际 {}",
                bgra.len()
            ),
        });
    }
    ensure_apartment()?;
    let engine = create_ocr_engine()?;

    // 引擎对图像边长有上限（多数机器 10000+，见 `OcrEngine::MaxImageDimension`）。超了先拒，
    // 而不是让它返回一个说不清的失败 —— 那种失败会被上层当成"识别出错"，排查方向就错了。
    match OcrEngine::MaxImageDimension() {
        Ok(limit) if width > limit || height > limit => {
            return Err(DeviceError::HostLimit {
                detail: format!("OCR 单边上限 {limit} 像素，当前图像 {width}x{height}"),
            });
        }
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(%error, "读不到 OcrEngine::MaxImageDimension，跳过尺寸预检");
        }
    }

    let bitmap = software_bitmap_from_bgra(width, height, &bgra[..expected])?;
    let operation = engine
        .RecognizeAsync(&bitmap)
        .map_err(|error| DeviceError::Failed {
            message: format!(
                "OcrEngine::RecognizeAsync 失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            ),
        })?;

    // 等异步操作完成。`AsyncStatus` 的类型在 windows 0.62 里搬去了 `windows-future` crate
    // （没被 re-export），所以这里只比较 `.0` 与 `Started` 的数值，不命名那个类型。
    let deadline = Instant::now() + OCR_TIMEOUT;
    loop {
        let status = operation.Status().map_err(|error| DeviceError::Failed {
            message: format!(
                "读 OCR 异步操作状态失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            ),
        })?;
        if status.0 != ASYNC_STATUS_STARTED {
            break;
        }
        if Instant::now() >= deadline {
            return Err(DeviceError::Failed {
                message: format!(
                    "OCR 识别超时（{} 秒）：引擎没有在时限内完成",
                    OCR_TIMEOUT.as_secs()
                ),
            });
        }
        std::thread::sleep(OCR_POLL_INTERVAL);
    }

    let result = operation
        .GetResults()
        .map_err(|error| DeviceError::Failed {
            message: format!(
                "取 OCR 结果失败（HRESULT 0x{:08X}）：{error}",
                error.code().0
            ),
        })?;
    let text = result.Text().map_err(|error| DeviceError::Failed {
        message: format!(
            "读 OcrResult::Text 失败（HRESULT 0x{:08X}）：{error}",
            error.code().0
        ),
    })?;
    Ok(text.to_string_lossy())
}

/// 按 **char**（不是字节）截断，并标记是否真的丢掉了东西。
///
/// 按字节截会把中文/emoji 切成半个字符；`max_chars == 0` 时返回空串，且只要有字被丢掉就是
/// `truncated = true`（调用方明确要 0 个字符，那就别假装"没截断"）。
fn truncate_ocr_text(text: String, max_chars: usize) -> OcrText {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    OcrText {
        text: head,
        truncated,
    }
}

// ---------------------------------------------------------------- 输入：桌面与白名单

/// 读当前**输入桌面**的名字（`Default` / `Winlogon` …）。拿不到返回 `None`。
///
/// 锁屏时输入桌面换成 `Winlogon`，安全桌面（UAC 提权框）同理；这两种情况下注入会被系统
/// 静默丢弃，所以必须提前拒绝（`docs/09` §4.3 第 1/2 条）。
fn input_desktop_name() -> Option<String> {
    // SAFETY: 请求 `DESKTOP_READOBJECTS` 只读权限；返回空指针表示当前没有可访问的输入桌面。
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return None;
    }
    let mut buffer = [0u16; 256];
    let mut needed: u32 = 0;
    // SAFETY: `desktop` 有效；缓冲区是栈上 `[u16; 256]`，长度按字节给出；
    // `UOI_NAME` 只读地写入桌面名（UTF-16，含结尾 NUL）。
    let queried = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * std::mem::size_of::<u16>()) as u32,
            &mut needed,
        )
    };
    // SAFETY: `desktop` 由 `OpenInputDesktop` 取得且非空，只关一次。
    unsafe { CloseDesktop(desktop) };
    if queried == 0 {
        return None;
    }
    let units = (needed as usize / std::mem::size_of::<u16>()).min(buffer.len());
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units)
        .min(units);
    Some(String::from_utf16_lossy(&buffer[..end]))
}

/// 确认当前输入桌面是 `Default`，否则拒绝注入。
fn ensure_default_input_desktop() -> Result<(), DeviceError> {
    bump();
    let name = input_desktop_name();
    match name.as_deref() {
        Some(name) if name.eq_ignore_ascii_case("default") => Ok(()),
        Some(name) => {
            tracing::warn!(desktop = %name, "注入被拒：输入桌面不是 Default");
            Err(DeviceError::HostLimit {
                detail: "锁屏或安全桌面，拒绝注入".to_string(),
            })
        }
        None => {
            tracing::warn!("注入被拒：OpenInputDesktop 取不到输入桌面");
            Err(DeviceError::HostLimit {
                detail: "锁屏或安全桌面，拒绝注入".to_string(),
            })
        }
    }
}

/// 取某个进程的映像名（小写 basename）。取不到返回 `None`。
///
/// 用 `PROCESS_QUERY_LIMITED_INFORMATION`（而不是 `PROCESS_QUERY_INFORMATION`）：
/// 前者在 Vista+ 上对"同级别或更低完整性"的进程就够用，且**不要求** `SeDebugPrivilege`。
/// 提权进程仍然可能被拒绝 —— 那就返回 `None`，调用方当成"不在白名单里"。
fn process_name_of_pid(pid: u32) -> Option<String> {
    // SAFETY: 只请求查询权限；返回空指针表示没权限或进程已退出。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // 32768 个 UTF-16 码元是 `MAX_PATH` 放宽后的常见上限；`QueryFullProcessImageNameW`
    // 会在缓冲区不够时把大小写成需要的长度并返回失败，这里当作"取不到"。
    let mut buffer = vec![0u16; 32_768];
    let mut size = buffer.len() as u32;
    // SAFETY: `handle` 有效；缓冲区足够长；`dwFlags = 0` 表示要 Win32 路径格式。
    let queried = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    // SAFETY: `handle` 由 `OpenProcess` 取得且非空，只关一次。
    unsafe { CloseHandle(handle) };
    if queried == 0 || size == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    let name = process_name(&path);
    if name.is_empty() { None } else { Some(name) }
}

/// 前台窗口所属进程的映像名（小写）。取不到返回 `None`。
fn foreground_process_name() -> Option<String> {
    bump();
    // SAFETY: 无参数，返回前台窗口句柄（可能为空）。
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    // SAFETY: `hwnd` 有效；`&mut pid` 指向栈上有效内存。
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return None;
    }
    process_name_of_pid(pid)
}

// ---------------------------------------------------------------- 输入：虚拟键与坐标

/// 把 `InputAction::Combo` 里的键名映射成虚拟键码，外加"是否需要 Shift"。
///
/// 字母/数字直接用自己的 ASCII 值（Windows 的 `VK_*` 对 A–Z、0–9 正好等于大写字母与数字的
/// 编码）；其它字符查 `VkKeyScanW` —— 那是**当前键盘布局**的权威答案，打不出来的字符
/// 会返回 -1，此时明确失败而不是注入一个猜的键。
fn virtual_key_of(key: ComboKey) -> Result<(VIRTUAL_KEY, bool), DeviceError> {
    bump();
    let plain = match key {
        ComboKey::Enter => Some(VK_RETURN),
        ComboKey::Tab => Some(VK_TAB),
        ComboKey::Escape => Some(VK_ESCAPE),
        ComboKey::Space => Some(VK_SPACE),
        ComboKey::Left => Some(VK_LEFT),
        ComboKey::Up => Some(VK_UP),
        ComboKey::Right => Some(VK_RIGHT),
        ComboKey::Down => Some(VK_DOWN),
        ComboKey::Function(number) => Some(VK_F1 + VIRTUAL_KEY::from(number) - 1),
        ComboKey::Char(character) => {
            if character.is_ascii_alphabetic() {
                return Ok((character.to_ascii_uppercase() as VIRTUAL_KEY, false));
            }
            if character.is_ascii_digit() {
                return Ok((character as VIRTUAL_KEY, false));
            }
            if u32::from(character) > u32::from(u16::MAX) {
                return Err(DeviceError::Failed {
                    message: format!(
                        "`{character}` 超出 BMP，键盘无法直接注入（纯文本请用 InputAction::Text）"
                    ),
                });
            }
            // SAFETY: 纯查询当前键盘布局，不改变任何状态。
            let scan = unsafe { VkKeyScanW(character as u16) };
            if scan == -1 {
                return Err(DeviceError::Failed {
                    message: format!(
                        "当前键盘布局打不出 `{character}`（组合键只能注入能映射到虚拟键的字符，纯文本请用 InputAction::Text）"
                    ),
                });
            }
            let scan = scan as u16;
            // 高字节是修饰位：0x01 Shift、0x02 Ctrl、0x04 Alt。
            if scan & 0x06 != 0 {
                return Err(DeviceError::Failed {
                    message: format!(
                        "`{character}` 需要 Ctrl/Alt（AltGr）才能打出，本后端的组合键不支持这种字符"
                    ),
                });
            }
            let virtual_key = scan & 0x00FF;
            if virtual_key == 0 {
                return Err(DeviceError::Failed {
                    message: format!("`{character}` 映射到的虚拟键码是 0，拒绝注入"),
                });
            }
            return Ok((virtual_key, scan & 0x01 != 0));
        }
    };
    match plain {
        Some(virtual_key) => Ok((virtual_key, false)),
        // 不可达：上面的 match 覆盖了所有变体。留着是为了将来加变体时编译器能提醒。
        None => Err(DeviceError::Failed {
            message: "这个键没有对应的虚拟键码".to_string(),
        }),
    }
}

/// 修饰键 → 虚拟键码。
fn modifier_virtual_key(modifier: ComboModifier) -> VIRTUAL_KEY {
    match modifier {
        ComboModifier::Ctrl => VK_CONTROL,
        ComboModifier::Alt => VK_MENU,
        ComboModifier::Shift => VK_SHIFT,
        ComboModifier::Win => VK_LWIN,
    }
}

/// 虚拟桌面的原点与尺寸（物理像素）。
///
/// 多显示器时原点可能是负数（主屏左边的副屏），所以必须用 `SM_*VIRTUALSCREEN`
/// 而不是 `SM_CXSCREEN`（`docs/09` §6.4）。
fn virtual_screen_bounds() -> Result<(i32, i32, i32, i32), DeviceError> {
    // SAFETY: 纯查询。
    let (origin_x, origin_y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        return Err(DeviceError::Failed {
            message: format!("虚拟桌面尺寸非法（{width}×{height}）：可能没有可交互桌面"),
        });
    }
    Ok((origin_x, origin_y, width, height))
}

/// 把物理像素坐标映射成 `SendInput` 的绝对坐标（0..=65535）。
///
/// 公式是 `(坐标 - 虚拟桌面原点) * 65535 / (尺寸 - 1)`：`MOUSEEVENTF_ABSOLUTE` 的坐标系是
/// **归一化到 0..65535 的整个虚拟桌面**，不是像素。用 `i64` 做中间运算避免 `1920 * 65535`
/// 在 `i32` 上溢出（那是 1.25 亿，早就爆了）。
///
/// 超出虚拟桌面的坐标夹到边界：宁可点屏幕边缘，也不要让乘法溢出成一个离谱的位置。
fn absolute_mouse_coords(
    x: i32,
    y: i32,
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    fn normalize(value: i32, origin: i32, extent: i32) -> i32 {
        let span = i64::from(extent) - 1;
        if span <= 0 {
            return 0;
        }
        let relative = i64::from(value) - i64::from(origin);
        let scaled = relative * 65_535 / span;
        scaled.clamp(0, 65_535) as i32
    }
    (
        normalize(x, origin_x, width),
        normalize(y, origin_y, height),
    )
}

/// 鼠标按键 → (按下 flag, 抬起 flag)。
fn mouse_button_flags(button: MouseButton) -> (u32, u32) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

/// 造一个全零的 `INPUT` 数组（`windows-sys` 的 `INPUT` 没实现 `Default`）。
fn zeroed_inputs<const N: usize>() -> [INPUT; N] {
    // SAFETY: `INPUT` 及其联合体成员都是 POD（`repr(C)`，无指针所有权语义），
    // 全零是合法位模式；`r#type = 0`（`INPUT_MOUSE`）与全零的联合体成员互相一致。
    unsafe { std::mem::zeroed() }
}

/// 造一个键盘事件。
fn keyboard_input(virtual_key: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    let mut input: INPUT = zeroed_inputs::<1>()[0];
    input.r#type = INPUT_KEYBOARD;
    // 写联合体成员是安全操作；不变量是"成员的 名字与 `r#type` 对应"，所以两者紧挨着写。
    input.Anonymous.ki = KEYBDINPUT {
        wVk: virtual_key,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

/// 送一批输入事件，并校验**全部**送进去了。
///
/// `SendInput` 返回实际插入输入队列的事件数；被 UIPI 拦住时是 0。部分成功也算失败：
/// 半按下的组合键会让目标应用停在奇怪的状态（比如 Ctrl 按住了没抬起）。
fn send_inputs(inputs: &[INPUT], action: &str) -> Result<(), DeviceError> {
    if inputs.is_empty() {
        return Ok(());
    }
    bump();
    // SAFETY: `inputs.as_ptr()` 指向长度为 `len` 的有效数组，`cbSize` 用
    // `size_of::<INPUT>()` 正好是该结构的实际大小（`SendInput` 会校验这个值）。
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        return Err(DeviceError::Failed {
            message: format!(
                "SendInput 只送进了 {sent}/{} 个事件（{action}）；目标进程可能完整性更高（UIPI：提权窗口、UAC 对话框、任务管理器一律点不动），本后端不尝试 UIAccess 绕过",
                inputs.len()
            ),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- 窗口枚举

/// `EnumWindows` 回调之间共享的可变状态。
struct EnumContext {
    windows: Vec<WindowInfo>,
    own_pid: u32,
    foreground: HWND,
    truncated: bool,
}

/// `EnumWindows` 回调：把"人眼看得见的顶层窗口"收进 `context.windows`。
///
/// 过滤顺序都是先便宜后贵（`IsWindowVisible` 最便宜，查进程名最贵）。
///
/// # Safety
///
/// `lparam` 必须是 `EnumWindows` 调用方传进来的、指向一个存活 `EnumContext` 的指针；
/// 回调不保存它，只在本同步调用期间使用。
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: 见本函数的文档 —— 指针来自同步调用期间的 `&mut EnumContext`。
    let context = unsafe { &mut *(lparam as *mut EnumContext) };
    if context.windows.len() >= MAX_WINDOWS {
        context.truncated = true;
        // 返回 0 = 停止枚举。
        return 0;
    }
    // SAFETY: 每个调用都只查询传入句柄的属性，失败返回 0。
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        if IsIconic(hwnd) != 0 {
            return 1;
        }
    }
    // SAFETY: 同上，只查询。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if ex_style & WS_EX_TOOLWINDOW != 0 {
        // 工具窗口（输入法候选框、托盘辅助窗口）不是"用户眼里的窗口"。
        return 1;
    }
    let mut pid: u32 = 0;
    // SAFETY: `&mut pid` 指向栈上有效内存。
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 || pid == context.own_pid {
        // 自己（宿主/面板）不该出现在可操作窗口列表里：模型点自己的窗口是死循环。
        return 1;
    }
    // 取不到进程名的窗口直接丢弃：白名单按进程名匹配，留着它也无法被授权。
    let Some(process) = process_name_of_pid(pid) else {
        return 1;
    };
    context.windows.push(WindowInfo {
        id: hwnd as usize as u64,
        title: window_title(hwnd),
        process,
        focused: hwnd == context.foreground,
    });
    1
}

/// 取窗口标题（不含结尾 NUL）。取不到返回空串 —— 有些窗口确实没有标题。
fn window_title(hwnd: HWND) -> String {
    // SAFETY: 只查询；返回不含 NUL 的长度。
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    // 长度可能在我们读之前变长，多留一个位置放 NUL。
    let mut buffer = vec![0u16; length as usize + 1];
    // SAFETY: 缓冲区长度为 `buffer.len()`，`GetWindowTextW` 最多写这么多码元。
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

// ---------------------------------------------------------------- 剪贴板

/// 打开的剪贴板会话。`Drop` 负责 `CloseClipboard`。
///
/// 剪贴板是**进程级独占**的（同一时刻只能有一个窗口拿着它），所以任何提前返回的路径都必须
/// 还回去，否则整个系统的复制粘贴会卡住 —— 用户能立刻感觉到。
struct ClipboardSession;

impl Drop for ClipboardSession {
    fn drop(&mut self) {
        // SAFETY: 只有 `open_clipboard` 成功后才构造 `ClipboardSession`，所以这里
        // 一定持有剪贴板且只关一次。
        unsafe { CloseClipboard() };
    }
}

/// 打开剪贴板（最多重试 [`CLIPBOARD_OPEN_ATTEMPTS`] 次）。
fn open_clipboard() -> Result<ClipboardSession, DeviceError> {
    for attempt in 0..CLIPBOARD_OPEN_ATTEMPTS {
        // SAFETY: `NULL` 表示"没有特定窗口拥有它"（用完由当前线程的 `CloseClipboard` 归还）。
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            return Ok(ClipboardSession);
        }
        let is_last = attempt + 1 == CLIPBOARD_OPEN_ATTEMPTS;
        if !is_last {
            std::thread::sleep(CLIPBOARD_OPEN_RETRY_DELAY);
        }
    }
    Err(DeviceError::Failed {
        message: format!(
            "OpenClipboard 试了 {CLIPBOARD_OPEN_ATTEMPTS} 次都失败：通常是别的进程正拿着剪贴板（Explorer/Office/输入法很常见），等一会儿再试"
        ),
    })
}

/// 读 `CF_UNICODETEXT`。**空剪贴板返回空字符串**，不是错误。
fn read_clipboard_text() -> Result<ClipboardValue, DeviceError> {
    // SAFETY: 剪贴板由 `ClipboardSession` 持有；`CF_UNICODETEXT` 的类型是 `u16`，
    // 本函数要转换的正是 `u32` 参数。
    let handle: HANDLE = unsafe { GetClipboardData(CF_UNICODETEXT as u32) };
    if handle.is_null() {
        // 剪贴板里没有文本（或者是空的）：这是正常状态，不是失败。
        return Ok(ClipboardValue {
            text: String::new(),
        });
    }
    // SAFETY: `handle` 是剪贴板持有的内存句柄（可能用 `GMEM_MOVEABLE` 分配），
    // 必须先 `GlobalLock` 才能读；解锁后指针失效，所以下面只在这一段里用。
    let pointer = unsafe { GlobalLock(handle) } as *const u16;
    if pointer.is_null() {
        return Err(DeviceError::Failed {
            message: "GlobalLock 失败：剪贴板里的文本句柄锁定不了".to_string(),
        });
    }
    let mut units: Vec<u16> = Vec::new();
    let mut truncated = false;
    // SAFETY: 从 `pointer` 起逐码元读到 NUL；上限 `MAX_CLIPBOARD_UNITS` 防止脏数据
    // 让我们越过分配边界（这里假设剪贴板数据以 NUL 结尾 —— `CF_UNICODETEXT` 要求如此）。
    unsafe {
        for index in 0..MAX_CLIPBOARD_UNITS {
            let unit = *pointer.add(index);
            if unit == 0 {
                break;
            }
            if index + 1 == MAX_CLIPBOARD_UNITS {
                truncated = true;
            }
            units.push(unit);
        }
        GlobalUnlock(handle);
    }
    if truncated {
        tracing::warn!(limit = MAX_CLIPBOARD_UNITS, "剪贴板文本超过上限，已截断");
    }
    Ok(ClipboardValue {
        text: String::from_utf16_lossy(&units),
    })
}

/// 写 `CF_UNICODETEXT`。
///
/// # 所有权：`SetClipboardData` 成功之后**绝对不要再 `GlobalFree`**
///
/// 这是剪贴板最常见的崩溃/泄漏点：`SetClipboardData` 成功意味着**系统接管了这块内存**
/// 的所有权，它会在下一次 `EmptyClipboard` 或进程退出时释放。我们再释放一次就是双重释放
/// （堆损坏，症状是几天后莫名其妙的崩溃）。
///
/// 反过来，`SetClipboardData` **失败**时所有权仍在我们手里，必须自己 `GlobalFree`，
/// 否则每失败一次泄漏一份文本。
fn write_clipboard_text(text: &str) -> Result<ClipboardValue, DeviceError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: 剪贴板由 `ClipboardSession` 持有；清空是写入的前置条件
    // （不清空的话格式列表里旧数据还在，粘贴行为会打架）。
    if unsafe { EmptyClipboard() } == 0 {
        return Err(DeviceError::Failed {
            message: "EmptyClipboard 失败：剪贴板可能刚被别的进程抢走了".to_string(),
        });
    }

    let bytes = (units.len() + 1) * std::mem::size_of::<u16>();
    // SAFETY: `GMEM_MOVEABLE` 是剪贴板要求的分配方式（系统要靠移动句柄管理内存）。
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if handle.is_null() {
        return Err(DeviceError::Failed {
            message: format!("GlobalAlloc 失败：分配 {bytes} 字节给剪贴板时内存不足"),
        });
    }
    // SAFETY: `handle` 刚分配成功、大小至少 `bytes`；写入 `units` 与结尾的 NUL。
    let pointer = unsafe { GlobalLock(handle) } as *mut u16;
    if pointer.is_null() {
        // SAFETY: 分配成功但锁定失败，所有权仍在我们手里。
        unsafe { GlobalFree(handle) };
        return Err(DeviceError::Failed {
            message: "GlobalLock 失败：刚分配的剪贴板内存锁定不了".to_string(),
        });
    }
    // SAFETY: 目标缓冲区有 `bytes` 字节 = `units.len()+1` 个码元，源有 `units.len()` 个。
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), pointer, units.len());
        *pointer.add(units.len()) = 0;
        GlobalUnlock(handle);
    }

    // SAFETY: 剪贴板由 `ClipboardSession` 持有；`handle` 是刚写好的全局内存。
    let stored = unsafe { SetClipboardData(CF_UNICODETEXT as u32, handle) };
    if stored.is_null() {
        // 失败：所有权还在我们手里，必须自己释放（否则泄漏）。
        // SAFETY: `handle` 仍然有效且未被系统接管。
        unsafe { GlobalFree(handle) };
        return Err(DeviceError::Failed {
            message: "SetClipboardData 失败：系统拒绝了这块内存（可能刚被别的进程抢走剪贴板）"
                .to_string(),
        });
    }
    // 成功：**不要** `GlobalFree(handle)` —— 所有权已经转移给系统，由它在下一次
    // `EmptyClipboard` 时释放。见本函数文档。

    Ok(ClipboardValue {
        text: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// 串行化所有会走输入路径的单测：计数器是进程级的，并行跑会互相干扰。
    static INPUT_CALL_LOCK: Mutex<()> = Mutex::new(());

    /// `scale_percent_of` 是纯计算，但它决定了 `DisplayInfo::scale_percent` 的语义，
    /// 而本机是 100% 缩放（只有 96 → 100 这一档能被真机覆盖），所以非 100% 的三档只能钉在这里。
    ///
    /// 断言的是「四舍五入到最近的整百分比」：Windows 设置页给的是 125%/150%/175%/200%，
    /// 而 DPI 的真实取值就是 120/144/168/192 —— 差 1 个百分点都会让上层把 150% 判成 149%。
    #[test]
    fn scale_percent_rounds_dpi_to_the_nearest_whole_percent() {
        assert_eq!(scale_percent_of(96), 100, "96 DPI 必须报 100%");
        assert_eq!(scale_percent_of(120), 125, "120 DPI → 125%，不是 124%");
        assert_eq!(scale_percent_of(144), 150, "144 DPI → 150%");
        assert_eq!(scale_percent_of(168), 175, "168 DPI → 175%");
        assert_eq!(scale_percent_of(192), 200, "192 DPI → 200%");
        // 非整档：Windows 允许自定义缩放（如 110% → 105.6 DPI 取整 106），四舍五入即可。
        assert_eq!(scale_percent_of(106), 110, "106 DPI → 110%");
        // 退化值：拿不到 DPI 时 `monitor_dpi` 会兜底成 96，这里的 0 分支只为不 panic。
        assert_eq!(
            scale_percent_of(0),
            0,
            "0 DPI 只能是「拿不到」，不能谎报 100%"
        );
    }

    fn input_call_guard() -> MutexGuard<'static, ()> {
        // 某个测试 panic 会毒化锁；这里恢复内部值继续跑，避免级联失败。
        INPUT_CALL_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn empty_allow_list_is_rejected_before_any_win32_call() {
        let _guard = input_call_guard();
        win32_probe::reset();
        let result = WindowsBackend::new().input(
            &InputAction::Click {
                x: 10,
                y: 10,
                button: MouseButton::Left,
            },
            &[],
        );
        assert_eq!(
            result,
            Err(DeviceError::AppNotAllowed {
                process: "<empty list>".to_string()
            }),
            "空白名单必须全拒"
        );
        assert_eq!(
            win32_probe::count(),
            0,
            "空白名单必须在任何 Win32 调用之前返回（不许先查桌面/前台进程）"
        );
    }

    #[test]
    fn a_non_matching_allow_list_is_rejected_after_checking_the_foreground_window() {
        let _guard = input_call_guard();
        win32_probe::reset();
        // 这个名字永远不可能匹配，所以下面无论当前前台是什么程序都会被拒 ——
        // 不会真的注入任何东西。
        let allow = vec!["hbot-device-never-a-real-process.exe".to_string()];
        let result = WindowsBackend::new().input(
            &InputAction::Click {
                x: 10,
                y: 10,
                button: MouseButton::Left,
            },
            &allow,
        );
        match result {
            Err(DeviceError::AppNotAllowed { process }) => {
                assert_ne!(process, "<empty list>", "非空名单不该走空名单分支");
            }
            Err(DeviceError::HostLimit { .. }) => {
                // 锁屏 / 无交互桌面（CI 上很常见）：同样是合法的拒绝。
            }
            other => panic!("非空且不匹配的白名单必须被拒，实际是 {other:?}"),
        }
        assert!(
            win32_probe::count() > 0,
            "非空名单必须真的去查了桌面与前台进程"
        );
    }

    #[test]
    fn absolute_mouse_coords_maps_the_virtual_screen_onto_0_65535() {
        // 单显示器 1920×1080，原点 (0,0)。
        assert_eq!(absolute_mouse_coords(0, 0, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(
            absolute_mouse_coords(1919, 1079, 0, 0, 1920, 1080),
            (65_535, 65_535)
        );
        let (middle_x, middle_y) = absolute_mouse_coords(960, 540, 0, 0, 1920, 1080);
        // 正中点约等于 65535/2。注意**不是严格 32767**：公式把 0..1919 映射到 0..65535，
        // 960/1919*65535 ≈ 32784（整数除法向下取整），偏一点是正常的。
        assert!(
            (middle_x - 32_767).abs() <= 64 && (middle_y - 32_767).abs() <= 64,
            "正中点应当接近 32767（±0.1%），实际 ({middle_x}, {middle_y})"
        );
        // 副屏在主屏左边：原点 -1920，宽 3840。
        assert_eq!(
            absolute_mouse_coords(-1920, 0, -1920, 0, 3840, 1080),
            (0, 0),
            "虚拟桌面最左边是 0"
        );
        assert_eq!(
            absolute_mouse_coords(1919, 0, -1920, 0, 3840, 1080),
            (65_535, 0),
            "虚拟桌面最右边是 65535"
        );
        // 越界坐标夹到边界，不溢出。
        assert_eq!(
            absolute_mouse_coords(100_000, 100_000, 0, 0, 1920, 1080),
            (65_535, 65_535)
        );
        assert_eq!(
            absolute_mouse_coords(-100_000, -100_000, 0, 0, 1920, 1080),
            (0, 0)
        );
        // 退化的尺寸不 panic。
        assert_eq!(absolute_mouse_coords(5, 5, 0, 0, 0, 0), (0, 0));
    }

    #[test]
    fn frame_from_bgra_drops_the_alpha_byte() {
        // 2×1：一个不透明红 (BGRA = 00 00 FF FF)，一个半透明绿 (BGRA = 00 FF 00 80)。
        // alpha 在 `BI_RGB` 下无定义，绝不能让它影响输出。
        let bgra = [0x00u8, 0x00, 0xFF, 0xFF, 0x00, 0xFF, 0x00, 0x80];
        let rgb = bgra_to_rgb(&bgra, 2, 1).expect("长度正确应当成功");
        assert_eq!(rgb, vec![0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00]);
        assert!(
            !rgb.contains(&0xFF_u8) || rgb.len() == 6,
            "alpha 字节不该出现在输出里"
        );
        // 长度不够要明确失败，而不是越界读。
        let error = bgra_to_rgb(&bgra[..6], 2, 1).expect_err("长度不够必须失败");
        assert!(matches!(error, DeviceError::Failed { .. }));
    }

    #[test]
    fn combo_keys_map_to_their_virtual_key_codes() {
        assert_eq!(
            virtual_key_of(ComboKey::Char('c')),
            Ok((u16::from(b'C'), false)),
            "字母映射到大写 ASCII（VK 码）"
        );
        assert_eq!(
            virtual_key_of(ComboKey::Char('7')),
            Ok((u16::from(b'7'), false))
        );
        assert_eq!(
            virtual_key_of(ComboKey::Function(5)),
            Ok((VK_F1 + 4, false)),
            "F5 = VK_F1 + 4"
        );
        assert_eq!(virtual_key_of(ComboKey::Enter), Ok((VK_RETURN, false)));
        assert_eq!(virtual_key_of(ComboKey::Space), Ok((VK_SPACE, false)));
        assert_eq!(modifier_virtual_key(ComboModifier::Win), VK_LWIN);
        // 超出 BMP 的字符没有虚拟键，必须明确失败。
        assert!(matches!(
            virtual_key_of(ComboKey::Char('\u{1D11E}')),
            Err(DeviceError::Failed { .. })
        ));
    }

    #[test]
    fn the_windows_backend_satisfies_the_send_sync_contract() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        assert_send_sync(&WindowsBackend::new());
    }

    // ------------------------------------------------------------ OCR（真识别，不 #[ignore]）

    use windows_sys::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
        FF_DONTCARE, FW_BOLD, GetTextFaceW, OUT_TT_PRECIS, SetBkMode, SetTextColor, TRANSPARENT,
        TextOutW,
    };

    /// 本机能不能真的跑 OCR。`None` = 建不出引擎（没语言包/受限会话），调用方打印原因后跳过。
    ///
    /// 这条分支是需求里的硬要求：**"引擎不可用"必须能被区分出来**，否则同一个 `assert` 在
    /// 没装语言包的机器上会把"这台机器没装 OCR"报成"我们的代码坏了"。
    fn ocr_engine_or_skip(test: &str) -> Option<()> {
        match ocr_engine_probe() {
            Ok(()) => {
                eprintln!("[{test}] 分支：引擎可用 → 跑真识别");
                Some(())
            }
            Err(reason) => {
                eprintln!("[{test}] 分支：引擎不可用 → 跳过（不是测试失败）。原因：{reason}");
                None
            }
        }
    }

    /// 用 GDI 把一段文字画进一张 1600x240 的白底 DIB，**黑字、大字**（96px）。
    ///
    /// 画布必须比文字宽：这张图要放 `HBOT OCR TEST 12345` 十九个 96px 粗体字符（约 1.1k 像素），
    /// 窄了会把结尾的字符切掉，测试就会以"识别不全"的样子失败，看起来像 OCR 的问题。
    ///
    /// 走的是与 OCR 完全相同的 `Surface`（同一个 `CreateDIBSection`），所以这条测试测的是
    /// "BGRA → SoftwareBitmap → OcrEngine" 的真实链路，不是另造一份夹具。
    fn surface_with_text(text: &str) -> Surface {
        const WIDTH: i32 = 1600;
        const HEIGHT: i32 = 240;
        let surface = Surface::new(WIDTH, HEIGHT)
            .expect("建 DIB 失败：当前会话大概没有可交互桌面（服务会话/RDP 已断开），GDI 画不出字");
        // 白底：BGRA 四字节全 0xFF（BI_RGB 不定义 alpha，但填满能避免 alpha 字节是随机值）。
        // 直接写 DIB 的映射内存，省一次 GDI 填充。
        let bytes = (WIDTH as usize) * (HEIGHT as usize) * 4;
        // SAFETY: `bits` 指向 `CreateDIBSection` 分配的 `WIDTH*HEIGHT*4` 字节可写映射内存。
        unsafe { std::ptr::write_bytes(surface.bits, 0xFF, bytes) };

        let units: Vec<u16> = text.encode_utf16().collect();
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        // SAFETY: `mem_dc` 来自刚建的 `Surface`（有效且已选入同尺寸位图）；字体名以 NUL 结尾；
        // `units` 在调用期间存活；`CreateFontW` 返回的字体在这里创建、选入、用完即删。
        unsafe {
            let font = CreateFontW(
                -96, // 字高 96px（负值 = 字符高度，不含内部行距）
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_TT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                face.as_ptr(),
            );
            assert!(!font.is_null(), "CreateFontW 失败：拿不到 GDI 字体句柄");
            let previous = SelectObject(surface.mem_dc, font);
            SetBkMode(surface.mem_dc, TRANSPARENT as i32);
            SetTextColor(surface.mem_dc, 0x0000_0000); // COLORREF：黑
            TextOutW(surface.mem_dc, 30, 50, units.as_ptr(), units.len() as i32);
            SelectObject(surface.mem_dc, previous);
            DeleteObject(font);
        }
        surface
    }

    /// 只用于比较的归一化：去空白 + 大写（OCR 的空白与大小写不可靠）。
    fn squashed(text: &str) -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .to_uppercase()
    }

    /// 在**指定字体**下把文字画进一张白底 DIB，并回读 GDI 实际选中的字体名。
    ///
    /// 为什么必须回读字体名：`CreateFontW` 对不存在的字体名**不报错**，它静默做字体映射
    /// （落回某个系统字体）。中文用例里如果本机没有中文字体，`Microsoft YaHei` 会被映射成
    /// 一个画不出汉字的字体，字会变成方框或空白 —— 那时断言"识别出中文"就是假通过。
    /// 所以先回读 `GetTextFaceW`，再由调用方自检像素，两条都过才敢说"中文真的画出来并被认出来"。
    fn surface_with_face(text: &str, face: &str, width: i32, height_px: i32) -> (Surface, String) {
        let surface = Surface::new(width, height_px)
            .expect("建 DIB 失败：当前会话大概没有可交互桌面（服务会话/RDP 已断开），GDI 画不出字");
        let bytes = (width as usize) * (height_px as usize) * 4;
        // SAFETY: `bits` 指向 `CreateDIBSection` 分配的 `width*height*4` 字节可写映射内存。
        unsafe { std::ptr::write_bytes(surface.bits, 0xFF, bytes) };

        let units: Vec<u16> = text.encode_utf16().collect();
        let face_spec: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
        let mut actual_face: Vec<u16> = Vec::new();
        // SAFETY: `mem_dc` 来自刚建的 `Surface`；字体名以 NUL 结尾；`units`/`buffer` 调用期间存活。
        unsafe {
            let font = CreateFontW(
                -96,
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_TT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                face_spec.as_ptr(),
            );
            assert!(!font.is_null(), "CreateFontW 失败：拿不到 GDI 字体句柄");
            let previous = SelectObject(surface.mem_dc, font);
            let mut buffer = [0u16; 64];
            let length = GetTextFaceW(surface.mem_dc, buffer.len() as i32, buffer.as_mut_ptr());
            if length > 0 {
                actual_face.extend_from_slice(&buffer[..(length as usize).min(buffer.len())]);
            }
            SetBkMode(surface.mem_dc, TRANSPARENT as i32);
            SetTextColor(surface.mem_dc, 0x0000_0000); // COLORREF：黑
            TextOutW(surface.mem_dc, 30, 60, units.as_ptr(), units.len() as i32);
            SelectObject(surface.mem_dc, previous);
            DeleteObject(font);
        }
        (surface, String::from_utf16_lossy(&actual_face))
    }

    /// 白底上"暗像素"的个数：用来证明字**真的画出来了**（不是空白画布，也不是方框雨）。
    ///
    /// 阈值取 2000：96px 粗体的几个汉字/十几个字母远远超过这个数，而抗锯齿边缘噪声远低于它。
    fn dark_pixel_count(bgra: &[u8]) -> usize {
        bgra.chunks_exact(4)
            .filter(|pixel| pixel[0] < 128 && pixel[1] < 128 && pixel[2] < 128)
            .count()
    }

    /// 「本机装了哪些 OCR 语言」的**非跳过**断言：引擎能建出来就至少有一种语言。
    ///
    /// 这条不 `#[ignore]`：它是"OCR 真的可用"的最小钉子。引擎建不出来时由
    /// [`ocr_engine_or_skip`] 打印原因并跳过（那属于宿主限制，不是代码坏了）。
    #[test]
    fn ocr_language_list_is_not_empty() {
        if ocr_engine_or_skip("ocr_language_list_is_not_empty").is_none() {
            return;
        }
        let languages = available_ocr_languages().expect("引擎可用时必须能列出识别语言");
        eprintln!(
            "[ocr] 本机 OCR 语言清单（{} 种）：{languages:?}",
            languages.len()
        );
        assert!(
            !languages.is_empty(),
            "引擎能建出来却一种识别语言都没有，这不合理"
        );
        for tag in &languages {
            assert!(!tag.trim().is_empty(), "语言标签不该是空串：{languages:?}");
        }
    }

    /// 中文实测：装了中文语言包 + 本机有中文字体时，`你好，世界` 必须被认出来。
    ///
    /// **运行时判定，不用 `#[ignore]`**：条件不全时打印"本机不可测"并返回（诚实跳过），
    /// 条件齐了就必须真认出来。为什么不用 `#[ignore]`：`#[ignore]` 会被无条件跳过，
    /// 而"这台机器到底能不能认中文"正是要实测的东西 —— 用运行时判定才能让结果随机器说话。
    ///
    /// 三道自检，缺一道就可能假通过：
    /// 1. 语言清单里有 `zh*`（没装中文包时中文必然认不出，这不算代码问题）；
    /// 2. GDI 实际选中的字体确实是中文字体（`Microsoft YaHei`，回退时说明无中文字体）；
    /// 3. 画布上有足够多的暗像素（字体在但字形画不出来时是空白）。
    #[test]
    fn chinese_text_is_recognized_when_a_chinese_language_pack_is_installed() {
        if ocr_engine_or_skip(
            "chinese_text_is_recognized_when_a_chinese_language_pack_is_installed",
        )
        .is_none()
        {
            return;
        }
        let languages = available_ocr_languages().unwrap_or_default();
        eprintln!("[ocr/zh] 语言清单：{languages:?}");
        let has_chinese_pack = languages
            .iter()
            .any(|tag| tag.to_ascii_lowercase().starts_with("zh"));
        if !has_chinese_pack {
            eprintln!(
                "[ocr/zh] 本机没装中文 OCR 语言包（清单里没有 zh-*）→ 中文识别本机不可测，跳过"
            );
            return;
        }

        const DRAWN: &str = "你好，世界";
        let (surface, actual_face) = surface_with_face(DRAWN, "Microsoft YaHei", 1200, 260);
        eprintln!("[ocr/zh] 请求字体 Microsoft YaHei，GDI 实际选中：{actual_face:?}");
        let lowered = actual_face.to_ascii_lowercase();
        if !(lowered.contains("yahei")
            || lowered.contains("simsun")
            || lowered.contains("simhei")
            || actual_face.contains("雅黑")
            || actual_face.contains("宋体")
            || actual_face.contains("黑体"))
        {
            eprintln!(
                "[ocr/zh] 本机没有中文字体（回退到 {actual_face:?}）→ 中文画不出来，本机不可测，跳过"
            );
            return;
        }

        let (width, height, bgra) = surface.read_bgra().expect("读回 BGRA 像素");
        let dark = dark_pixel_count(&bgra);
        eprintln!("[ocr/zh] 画布 {width}×{height}，暗像素 {dark} 个");
        assert!(
            dark > 2000,
            "字体选对了但画布上几乎没有墨迹（{dark} 个暗像素）：中文没画出来，不能拿识别结果说事"
        );

        let recognized = recognize_bgra(width, height, &bgra)
            .expect("引擎可用时识别不该失败（失败要查日志里的 HRESULT）");
        eprintln!("[ocr/zh] 画进去的是 {DRAWN:?}，识别结果是 {recognized:?}");
        let normalized: String = recognized
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert!(
            normalized.contains("你好") || normalized.contains("世界"),
            "装了中文语言包、字也画出来了，却没认出一个中文词：画的是 {DRAWN:?}，认出来是 {recognized:?}"
        );
    }

    /// 真的识出文字：GDI 画 `HBOT OCR TEST 12345` → 走 `recognize_bgra` → 断言认出来了。
    #[test]
    fn ocr_recognizes_text_drawn_into_a_dib() {
        const DRAWN: &str = "HBOT OCR TEST 12345";
        if ocr_engine_or_skip("ocr_recognizes_text_drawn_into_a_dib").is_none() {
            return;
        }
        let surface = surface_with_text(DRAWN);
        let (width, height, bgra) = surface.read_bgra().expect("读回 BGRA 像素");
        let recognized = recognize_bgra(width, height, &bgra)
            .expect("引擎可用时识别不该失败（失败要查日志里的 HRESULT）");
        eprintln!(
            "[ocr] 画进去的是 {DRAWN:?}，识别结果是 {recognized:?}（{} 个字符）",
            recognized.chars().count()
        );
        assert!(
            squashed(&recognized).contains(&squashed(DRAWN)),
            "识别结果里没有画进去的文字：画的是 {DRAWN:?}，认出来是 {recognized:?}"
        );
        // 同一条真实结果再验一次按 char 截断：截断真的走在这条链路上，而不只是纯函数。
        let limited = truncate_ocr_text(recognized.clone(), 5);
        assert!(
            limited.truncated,
            "上限 5 个字符时必须标记截断：{recognized:?}"
        );
        assert!(limited.text.chars().count() <= 5);
    }

    /// `max_chars` 按 **char** 截断，不是字节。
    ///
    /// 这条**不需要** OCR 引擎（截断是纯函数），所以任何机器上都会真跑：
    /// 否则"引擎装没装"会决定"截断逻辑有没有被测过"，那不算测过。
    #[test]
    fn ocr_truncation_counts_chars_not_bytes() {
        let text = "中文AB🙂".to_string(); // 5 个 char，其中 3 个是多字节
        let kept = truncate_ocr_text(text.clone(), 3);
        assert_eq!(kept.text, "中文A", "按字节截会把中文切成半个字符");
        assert!(kept.truncated);
        assert_eq!(kept.text.chars().count(), 3);

        let exact = truncate_ocr_text(text.clone(), 5);
        assert_eq!(exact.text, text, "正好等于上限时不该丢字");
        assert!(!exact.truncated, "正好等于上限不算截断");

        let zero = truncate_ocr_text(text, 0);
        assert_eq!(zero.text, "", "上限 0 就是不要字");
        assert!(zero.truncated, "有字被丢掉就必须标记截断");

        let already_empty = truncate_ocr_text(String::new(), 0);
        assert!(!already_empty.truncated, "本来就是空文本：没有东西被截掉");
    }

    /// 找不到可读窗口时必须是 `AppNotAllowed`，**不能**退回截主屏、也不能返回空串。
    ///
    /// 这条不依赖 OCR 引擎：窗口查找发生在建引擎之前，所以没装语言包的机器上也必须成立
    /// （而且查找失败时根本不会去建引擎）。
    #[test]
    fn window_ocr_without_a_readable_window_is_an_app_not_allowed_error() {
        let process = "hbot-device-never-a-real-window.exe";
        let result = WindowsBackend::new().ocr(&OcrRequest {
            target: DeviceTarget::Window {
                process: process.to_string(),
            },
            max_chars: 100,
        });
        match result {
            Err(DeviceError::AppNotAllowed { process: reported }) => {
                assert_eq!(reported, process, "要把被拒的进程原样回给调用方");
            }
            other => panic!(
                "没有可读窗口的进程必须返回 AppNotAllowed（不许静默截主屏，也不许空串）：{other:?}"
            ),
        }
    }

    /// 回归测试：**在别的线程建过引擎之后，本线程还能再建**（曾经这里必崩 `0xC0000005`）。
    ///
    /// 崩因（已修，见 [`hold_mta_for_process`] 上方那两条）：`CoInitializeEx` 的引用计数按线程算，
    /// 线程退出时 combase 会还回它那一份；最后一个引用消失就把 MTA 拆了，而 `windows` crate 的
    /// 激活工厂缓存是**进程级**的 —— 于是"A 线程建过、A 退出、B 线程再建"拿到悬空工厂 → AV。
    ///
    /// 为什么必须钉住：`DeviceBackend` 是 `Send + Sync` 的，`ocr()` 可以被任何线程调用，
    /// 而 libtest 本身每个测试一个线程 —— 这条链路必须跨线程成立，不是"测试环境的怪癖"。
    #[test]
    fn ocr_engine_survives_an_engine_created_on_another_thread() {
        if ocr_engine_or_skip("ocr_engine_survives_an_engine_created_on_another_thread").is_none() {
            return;
        }
        // 先在一个**马上就会退出**的线程里建一次引擎并 drop（就是崩的那个场景）。
        std::thread::spawn(|| {
            let _ = ensure_apartment();
            let _ = OcrEngine::TryCreateFromUserProfileLanguages();
        })
        .join()
        .expect("探测线程不该 panic");

        // 回到本线程再建一次：修好之前这里会 STATUS_ACCESS_VIOLATION。
        let _engine = create_ocr_engine().expect("别的线程建过引擎之后，本线程仍必须能建引擎");
        let limit = OcrEngine::MaxImageDimension().expect("建出来的引擎要能用");
        assert!(limit > 0, "引擎报的图像上限必须为正：{limit}");
        eprintln!("[ocr] 跨线程后重建引擎成功，引擎单边上限 {limit} 像素");
    }
}
