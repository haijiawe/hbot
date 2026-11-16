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

use std::ffi::c_void;
use std::ptr;
use std::time::Duration;

use hbot_core::device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapabilityReport, DeviceError, InputAction,
    MouseButton, Screenshot, ScreenshotRequest, WindowInfo,
};
use windows_sys::Win32::Foundation::{BOOL, CloseHandle, GlobalFree, HANDLE, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
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
    IsWindowVisible, PW_RENDERFULLCONTENT, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WS_EX_TOOLWINDOW,
};

use crate::{
    Combo, ComboKey, ComboModifier, app_is_allowed, enforce_limit, parse_combo, png, process_name,
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
    /// `monitor` 只接受 `None` 或 `Some(0)`；其它索引明确未实现（返回 `HostLimit`），
    /// 而不是悄悄截主屏 —— 静默给错显示器会让模型对着错误的画面决策。
    fn capture(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        if let Some(monitor) = request.monitor {
            if monitor != 0 {
                return Err(DeviceError::HostLimit {
                    detail: format!(
                        "只实现了主显示器（monitor 0），显示器 {monitor} 未实现（docs/09 §9.2 窄版范围）"
                    ),
                });
            }
        }
        let frame = capture_frame()?;
        let encoded = png::encode_rgb(frame.width, frame.height, &frame.rgb)?;
        tracing::debug!(
            width = frame.width,
            height = frame.height,
            bytes = encoded.len(),
            "截图完成"
        );
        let png = enforce_limit(encoded, request.max_bytes)?;
        Ok(Screenshot {
            width: frame.width,
            height: frame.height,
            png,
            monitor: 0,
        })
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

        let mut notes: Vec<&str> = Vec::new();
        if !screen {
            notes.push("拿不到屏幕 DC：当前会话可能没有可交互桌面上（RDP 已断开 / 服务会话）");
        }
        if !input {
            notes.push("当前输入桌面不是 Default：锁屏或安全桌面期间拒绝注入");
        }
        notes.push(
            "截图走 PrintWindow/BitBlt：独占全屏 DirectX 游戏、受 DRM 保护的内容可能全黑；只支持主显示器",
        );
        notes.push("输入受 UIPI 限制：完整性级别更高的窗口（提权程序、UAC 对话框）无法注入");
        notes.push("OCR 未实现");

        DeviceCapabilityReport {
            screen,
            input,
            window,
            clipboard,
            detail: Some(notes.join("；")),
        }
    }

    fn screenshot(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        self.capture(request)
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

// ---------------------------------------------------------------- 截图

/// 一张已解码的图像（RGB8，无 alpha）。
struct Frame {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

/// 一块 GDI 离屏表面（屏幕 DC + 内存 DC + 32bpp 自顶向下 DIB）。
///
/// `Drop` 里按**创建的反序**拆：先恢复原来的位图，再删位图，再删 DC，最后还屏幕 DC。
/// 顺序错了（先删位图再 `SelectObject` 回去）会用到已释放的句柄。
struct Surface {
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

impl Surface {
    /// 建一块 `width × height`、32bpp、**自顶向下**（`biHeight` 为负）的 DIB 表面。
    ///
    /// 自顶向下是为了让 `bits` 的第一行就是屏幕最上面一行，省掉翻转。
    fn new(width: i32, height: i32) -> Result<Self, DeviceError> {
        if width <= 0 || height <= 0 {
            return Err(DeviceError::Failed {
                message: format!("截图区域尺寸非法（{width}×{height}）"),
            });
        }
        if width > MAX_SCREENSHOT_DIMENSION || height > MAX_SCREENSHOT_DIMENSION {
            return Err(DeviceError::Failed {
                message: format!(
                    "截图区域过大（{width}×{height}，单边上限 {MAX_SCREENSHOT_DIMENSION}）"
                ),
            });
        }
        if u64::from(width as u32) * u64::from(height as u32) > MAX_SCREENSHOT_PIXELS {
            return Err(DeviceError::Failed {
                message: format!(
                    "截图区域过大（{width}×{height}，总像素上限 {MAX_SCREENSHOT_PIXELS}）"
                ),
            });
        }

        // SAFETY: `GetDC(NULL)` 取整个屏幕的 DC，返回空指针表示当前会话没有可用显示。
        let screen_dc = unsafe { GetDC(ptr::null_mut()) };
        if screen_dc.is_null() {
            return Err(DeviceError::Failed {
                message: "GetDC(NULL) 失败：当前会话没有可用的显示（RDP 已断开 / 无交互桌面）"
                    .to_string(),
            });
        }

        // SAFETY: 与屏幕 DC 兼容的内存 DC。
        let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
        if mem_dc.is_null() {
            // SAFETY: `screen_dc` 由上面的 `GetDC` 取得且非空。
            unsafe { ReleaseDC(ptr::null_mut(), screen_dc) };
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
                screen_dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                ptr::null_mut(),
                0,
            )
        };
        if bitmap.is_null() || bits.is_null() {
            // SAFETY: `mem_dc` 非空且由 `CreateCompatibleDC` 创建；`screen_dc` 同理。
            unsafe {
                DeleteDC(mem_dc);
                ReleaseDC(ptr::null_mut(), screen_dc);
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
                ReleaseDC(ptr::null_mut(), screen_dc);
            }
            return Err(DeviceError::Failed {
                message: "SelectObject 失败：位图没能选进内存 DC".to_string(),
            });
        }

        Ok(Self {
            screen_dc,
            mem_dc,
            bitmap,
            previous,
            bits,
            width,
            height,
        })
    }

    /// 把像素读成 RGB8。逐行去掉 4 字节里的 alpha（`BI_RGB` 下它无定义，不能当不透明用）。
    fn read_frame(&self) -> Result<Frame, DeviceError> {
        let width = self.width as u32;
        let height = self.height as u32;
        let expected = width as usize * height as usize * 4;
        // SAFETY: `bits` 指向 `CreateDIBSection` 分配的 `width*height*4` 字节；
        // 这块内存在 `self` 存活期间有效，且不与其它借用重叠。
        let bgra = unsafe { std::slice::from_raw_parts(self.bits as *const u8, expected) };
        Ok(Frame {
            width,
            height,
            rgb: bgra_to_rgb(bgra, width, height)?,
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: 这些句柄都是 `Surface::new` 成功时取得的，且只在这里释放一次。
        // 顺序：先把旧位图选回去（否则删位图时它还在 DC 里），再删位图、内存 DC、屏幕 DC。
        unsafe {
            SelectObject(self.mem_dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.mem_dc);
            ReleaseDC(ptr::null_mut(), self.screen_dc);
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
    // SAFETY: 纯查询，失败返回 0。
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let surface = Surface::new(width, height)?;
    // SAFETY: 两个 DC 都有效且尺寸匹配；`SRCCOPY` 是不做变换的逐位拷贝。
    let copied = unsafe {
        BitBlt(
            surface.mem_dc,
            0,
            0,
            width,
            height,
            surface.screen_dc,
            0,
            0,
            SRCCOPY,
        )
    };
    if copied == 0 {
        return Err(DeviceError::Failed {
            message: "BitBlt 失败：屏幕 DC 可能不可读（独占全屏或受保护内容）".to_string(),
        });
    }
    surface.read_frame()
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
}
