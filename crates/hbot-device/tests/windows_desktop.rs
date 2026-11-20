//! Windows 真机测试：**只验证"不打扰用户"的那些性质**。
//!
//! CI 里绝不移动鼠标 —— 那会让正在用机器的人莫名其妙。逐条说明这些测试在碰什么：
//!
//! - 能力报告（`GetDC` 一下、查一次输入桌面名、枚举显示器、惰性的 OCR 引擎探测）；
//! - 窗口枚举（只读）；
//! - 空白名单拒绝（在碰 `SendInput` 之前就返回了）；
//! - 剪贴板**读**（空剪贴板也是合法的成功）；
//! - 显示器清单 + 按序号真截屏（`monitor: Some(i)` 只抓那一块屏，不碰鼠标）；
//! - 不存在的显示器序号 → 明确 `HostLimit`（连截图都不开始）；
//! - **按窗口 OCR**：本进程自建一个可见窗口（GDI 画字）→ 真的识别出窗口里的文字；
//!   最小化 / 无标题的窗口 → 明确 `AppNotAllowed`（绝不返回空串冒充成功）。
//!
//! 真截屏的两条本机可以跑（它们只抓自己的屏，不改用户状态）；但为了不打扰"正在用机器的人"
//! （截图会读整个桌面内容），仍标 `#[ignore]`，手动跑：`cargo test -p hbot-device -- --ignored`。
//!
//! **真 OCR**（画字进 DIB 再识别、语言清单、中文实测）在 `src/windows.rs` 的单测里，那里能直接
//! 拿到 `Surface` 与内部识别函数；这里只从公开入口验证"按窗口"那条链路。

#![cfg(windows)]

use std::sync::{Mutex, MutexGuard};

use hbot_device::{
    ClipboardOp, DeviceBackend, DeviceError, DeviceTarget, InputAction, MouseButton, OcrRequest,
    ScreenshotRequest, WindowsBackend, process_name,
};

/// 串行化所有会创建测试窗口的用例。
///
/// **必须串行**：按窗口 OCR 是按**进程映像名**找窗口的，而这些用例都在同一个进程里跑。
/// 并行时"最小化窗口"用例可能看见另一个用例刚建好的可见窗口，于是得到"能读"的结果 ——
/// 那不是代码错了，是夹具互相干扰。
static WINDOW_TEST_LOCK: Mutex<()> = Mutex::new(());

fn window_test_guard() -> MutexGuard<'static, ()> {
    WINDOW_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// PNG 文件头（8 字节签名）。
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 当前测试进程的映像名（小写 basename），按窗口 OCR 就用它当 `process`。
fn own_process_name() -> String {
    let path = std::env::current_exe().expect("取当前测试进程的路径");
    let name = process_name(&path.to_string_lossy());
    assert!(!name.is_empty(), "当前测试进程的映像名不该是空串");
    name
}

/// 只用于比较的归一化：去空白 + 大写（OCR 的空白与大小写不可靠）。
fn squashed(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_uppercase()
}

#[test]
fn capabilities_report_the_four_implemented_sub_capabilities() {
    // 需要有交互式桌面会话（锁屏/RDP 断开时 `input` 会是 false —— 那是诚实的行为，
    // 不是 bug，但这条测试要的是"正常桌面会话下四项都成立"）。
    let report = WindowsBackend::new().capabilities();
    assert!(report.screen, "Windows 桌面会话应当能截图：{report:?}");
    assert!(report.input, "Windows 桌面会话应当能注入：{report:?}");
    assert!(report.window, "窗口枚举是无条件支持的");
    assert!(report.clipboard, "剪贴板是无条件支持的");
    let detail = report.detail.expect("能力报告必须说明边界（已知限制）");
    assert!(
        detail.contains("UIPI"),
        "能力报告要写明 UIPI 这条硬墙：{detail}"
    );
    assert!(
        detail.contains("OCR"),
        "能力报告要写明 OCR 的边界（不是「没做」，是「只认已安装语言 + 整屏/按窗口」）：{detail}"
    );
    // 多显示器的清单必须在 notes 里能看到（模型据此才知道 `monitor` 能给哪些序号）。
    assert!(
        detail.contains("显示器"),
        "能力报告要写明显示器块数与序号规则：{detail}"
    );
    // `ocr` 字段按"引擎能不能建出来"填（没装 OCR 语言包的机器上是 false），说明必须与它一致：
    // 说支持就得写清"只认已安装语言"，说不支持就得写清原因 —— 两者不许打架。
    if report.ocr {
        assert!(
            detail.contains("语言包"),
            "报告说 OCR 可用，就必须写明它只认系统已安装的语言包：{detail}"
        );
    } else {
        assert!(
            detail.contains("OCR 不可用"),
            "报告说 OCR 不可用，就必须写明原因（本机大概率没装 OCR 语言包）：{detail}"
        );
    }
}

#[test]
fn the_display_list_is_non_empty_and_lists_the_primary_first() {
    // 从 crate 外面走公开函数（`hbot_device::displays()`）—— 这正是"不改 trait 也能列出显示器"
    // 的交付点，所以必须在集成测试里断言，而不是只在 crate 内部断言。
    let displays = hbot_device::displays();
    eprintln!("[display] 本机显示器清单：{displays:?}");
    assert!(
        !displays.is_empty(),
        "在有交互桌面的 Windows 上必须至少枚举出一块显示器"
    );
    assert_eq!(
        hbot_device::display_count(),
        displays.len(),
        "display_count() 必须等于 displays().len()"
    );
    for (position, display) in displays.iter().enumerate() {
        assert_eq!(
            display.index, position as u32,
            "序号必须是连续下标（第 {position} 个元素的 index 是 {}）：{displays:?}",
            display.index
        );
        assert!(
            display.width > 0 && display.height > 0,
            "分辨率必须非零：{display:?}"
        );
        assert!(
            !display.device.trim().is_empty(),
            "设备名不该为空（抓图要用它建 DC）：{display:?}"
        );
    }
    assert!(
        displays[0].primary,
        "序号 0 必须是主屏（`ScreenshotRequest::monitor` 的文档承诺 0 = 主屏）：{displays:?}"
    );
    assert_eq!(
        displays.iter().filter(|display| display.primary).count(),
        1,
        "有且只有一块主屏：{displays:?}"
    );
}

#[test]
fn a_nonexistent_monitor_index_is_a_host_limit_listing_the_count() {
    let displays = hbot_device::displays();
    if displays.is_empty() {
        eprintln!("[display] 没有可交互桌面（枚举为空）→ 跳过");
        return;
    }
    let missing = displays.len() as u32 + 6;
    let error = WindowsBackend::new()
        .screenshot(&ScreenshotRequest {
            monitor: Some(missing),
            max_bytes: 8 * 1024 * 1024,
        })
        .expect_err("不存在的显示器序号必须被拒绝，不许静默退回主屏");
    match error {
        DeviceError::HostLimit { detail } => {
            assert!(
                detail.contains(&displays.len().to_string()),
                "错误信息要列出可用数量 {}：{detail}",
                displays.len()
            );
            assert!(
                detail.contains(&missing.to_string()),
                "错误信息要点出被拒的序号 {missing}：{detail}"
            );
            eprintln!("[display] 序号 {missing} 的错误文案：{detail}");
        }
        other => panic!("应当是 HostLimit，实际 {other:?}"),
    }
}

#[test]
fn every_enumerated_monitor_can_be_captured_by_index() {
    // 0 号（主屏）与 1 号（若有第二块屏）都要真的抓出来；没有第二块屏就断言"枚举里确实只有一块"。
    // 这条不 `#[ignore]`：它只抓自己的屏、不移动鼠标、不改用户状态，而且它是本次改动的**主证据**。
    let displays = hbot_device::displays();
    if displays.is_empty() {
        eprintln!("[display] 没有可交互桌面（枚举为空）→ 跳过");
        return;
    }
    let backend = WindowsBackend::new();
    let limit = 64 * 1024 * 1024;
    for display in &displays {
        let shot = backend
            .screenshot(&ScreenshotRequest {
                monitor: Some(display.index),
                max_bytes: limit,
            })
            .expect("枚举出来的显示器必须都能按序号抓图（枚举与抓图用的是同一份清单）");
        assert_eq!(
            shot.monitor, display.index,
            "返回值里的 monitor 必须是实际截的那一块"
        );
        assert_eq!(
            (shot.width, shot.height),
            (display.width, display.height),
            "截出来的像素尺寸必须等于枚举到的分辨率（序号 {}：{}）",
            display.index,
            display.device
        );
        assert_eq!(
            &shot.png[..8],
            &PNG_SIGNATURE,
            "输出必须是 PNG（序号 {}）",
            display.index
        );
        assert_eq!(&shot.png[12..16], b"IHDR", "IHDR 必须紧跟签名");
        assert!(shot.png.len() <= limit, "不许超过 max_bytes");
        eprintln!(
            "[display] 显示器 {}（{}）抓图 {}×{}，PNG {} 字节",
            display.index,
            display.device,
            shot.width,
            shot.height,
            shot.png.len()
        );
    }
    // 本机只有一块屏时，1 号必须是 HostLimit（文案带数量）——那是"第二块屏"的对照用例。
    if displays.len() == 1 {
        let error = backend
            .screenshot(&ScreenshotRequest {
                monitor: Some(1),
                max_bytes: limit,
            })
            .expect_err("本机只有一块显示器时，序号 1 必须被拒绝");
        match error {
            DeviceError::HostLimit { detail } => {
                assert!(
                    detail.contains('1') && detail.contains("显示器"),
                    "错误信息要写清只有几块显示器：{detail}"
                );
                eprintln!("[display] 本机只有一块屏，序号 1 的错误文案：{detail}");
            }
            other => panic!("应当是 HostLimit，实际 {other:?}"),
        }
    }
}

#[test]
fn window_ocr_without_a_readable_window_is_an_app_not_allowed_error() {
    // 只走 trait 的公开入口，而且窗口查找发生在抓图与建引擎**之前** —— 所以这条在任何机器上
    // 都成立（没装语言包的机器也一样），也不会去读屏幕、不打扰正在用机器的人。
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
            "没有可读窗口的进程必须返回 AppNotAllowed（不静默截全屏、不空串），实际：{other:?}"
        ),
    }
}

#[test]
fn windows_are_enumerated_with_lowercase_process_names() {
    let windows = WindowsBackend::new().windows().expect("枚举窗口不该失败");
    assert!(
        windows.len() <= 200,
        "窗口列表上限是 200，实际 {}",
        windows.len()
    );
    for window in &windows {
        assert!(!window.process.is_empty(), "process 不能是空串：{window:?}");
        assert_eq!(
            &window.process,
            &process_name(&window.process),
            "process 必须是已小写的 basename"
        );
        assert!(!window.title.contains('\0'), "标题里不该有 NUL：{window:?}");
    }
    assert!(
        windows.iter().filter(|window| window.focused).count() <= 1,
        "最多只有一个窗口是前台"
    );
    // 我们自己的窗口不该出现在列表里（模型点自己的窗口是死循环）。
    let own = own_process_name();
    assert!(
        windows.iter().all(|window| window.process != own),
        "自己的进程（{own}）不该出现在可操作窗口列表里"
    );
}

#[test]
fn input_with_an_empty_allow_list_is_refused() {
    // 空名单 = 全拒，而且是在任何 Win32 调用之前（单测里用计数器钉住了后者）。
    // 这里从 crate 外面再确认一遍返回值形状。
    let result = WindowsBackend::new().input(
        &InputAction::Click {
            x: 5,
            y: 5,
            button: MouseButton::Left,
        },
        &[],
    );
    assert_eq!(
        result,
        Err(DeviceError::AppNotAllowed {
            process: "<empty list>".to_string()
        })
    );
}

#[test]
fn typing_with_an_empty_allow_list_is_refused_before_any_keystroke() {
    for action in [
        InputAction::Text {
            text: "hello".to_string(),
        },
        InputAction::Combo {
            keys: vec!["ctrl".to_string(), "c".to_string()],
        },
    ] {
        let result = WindowsBackend::new().input(&action, &[]);
        assert!(
            matches!(
                result,
                Err(DeviceError::AppNotAllowed { ref process }) if process == "<empty list>"
            ),
            "{action:?} 在空白名单下必须被拒，实际 {result:?}"
        );
    }
}

#[test]
fn clipboard_read_returns_a_value_even_when_the_clipboard_is_empty() {
    // 读空剪贴板是**成功**（返回空串），不是错误。这条断言不依赖剪贴板里现在有什么：
    // 有内容也只是返回那段内容。
    let value = WindowsBackend::new()
        .clipboard(&ClipboardOp::Read)
        .expect("读剪贴板不该失败");
    // 无法断言内容（可能是用户刚复制的东西），只断言"拿到了一个字符串值"。
    let _text: String = value.text;
}

#[test]
fn an_absurdly_small_byte_limit_is_enforced() {
    // 1 字节的上限：任何一张真 PNG 都放不进去，所以必然 `TooLarge`。
    // 这条会真的截一次屏，但只在被调用时才截，且不会移动鼠标。
    let result = WindowsBackend::new().screenshot(&ScreenshotRequest {
        monitor: None,
        max_bytes: 1,
    });
    match result {
        Err(DeviceError::TooLarge { limit_bytes }) => assert_eq!(limit_bytes, 1),
        // 没有可交互桌面时（CI/无头）可能是 HostLimit —— 同样是"没有糊弄过去"。
        Err(DeviceError::HostLimit { .. }) => {}
        other => panic!("1 字节上限必须拒绝，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------- 自建窗口（按窗口 OCR 的夹具）

/// 自建窗口里画的那行字。挑 ASCII + 数字：本机一定有英文字体，不会因为缺字体假失败。
const WINDOW_TEXT: &str = "HBOT WIN OCR 7788";

/// 窗口过程：`WM_PAINT` / `WM_PRINTCLIENT` 都往给定的 DC 上画同一行字。
///
/// `WM_PRINTCLIENT` 是 `PrintWindow` 真正走的那条路（`PW_RENDERFULLCONTENT` 由 DWM 帮忙），
/// 所以两个分支都要画 —— 否则 `PrintWindow` 抓到的是一张空白窗口。
unsafe extern "system" fn ocr_window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, WM_ERASEBKGND, WM_PAINT, WM_PRINTCLIENT,
    };
    match message {
        WM_PAINT => {
            // SAFETY: `PAINTSTRUCT` 是 POD，全零合法；`BeginPaint` 只在成功时写入。
            let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            // SAFETY: `hwnd` 由系统给出；`&mut paint` 指向栈上有效内存。
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            // SAFETY: `hdc` 是刚拿到的有效绘制 DC（BeginPaint 保证）。
            unsafe { paint_window_text(hwnd, hdc) };
            // SAFETY: 与上面的 BeginPaint 成对。
            unsafe { EndPaint(hwnd, &paint) };
            0
        }
        WM_PRINTCLIENT => {
            // SAFETY: 按 Win32 契约，`WM_PRINTCLIENT` 的 `wparam` 就是要画的 HDC。
            unsafe { paint_window_text(hwnd, wparam as windows_sys::Win32::Graphics::Gdi::HDC) };
            0
        }
        WM_ERASEBKGND => 1, // 已经自己填了底，别让系统再刷一次（会闪）
        // SAFETY: 其余消息交给默认过程（与任何自定义窗口过程的契约一致）。
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// 夹具字号的候选（像素高度，负值 = 字符高度，见 `CreateFontW` 的 `cHeight`），从大到小。
///
/// **为什么要一串候选而不是一个常量**：字号决定文本宽度，文本宽度决定窗口宽度，
/// 而窗口只要有一角超出屏幕，超出的那部分就不会被合成进 `PrintWindow` 的位图 ——
/// OCR 读到的是被裁过的文字（2026-11-20 的 CI 就是这样红的：本机屏幕够宽，
/// `1121` 宽的窗口全绿；runner 的虚拟屏幕只有 `1024` 宽，末尾两个字符被裁成
/// `"HBOT WIN OCR 77"`，且 5 次重试稳定复现）。所以按**当前屏幕**取第一个放得下的字号。
const FONT_HEIGHT_CANDIDATES: [i32; 7] = [-96, -80, -72, -64, -48, -36, -28];

/// 文本左上角在客户区里的位置。
const TEXT_LEFT: i32 = 40;
const TEXT_TOP: i32 = 100;

/// 文本之外至少留这么多像素。
///
/// 不为零是因为 OCR 对贴边的字符不可靠（引擎要看到完整的字形边界）；
/// 留一段白边比"刚好放下"稳得多。
const TEXT_MARGIN: i32 = 120;

/// 建夹具字体：`font_height` 像素高的粗体 Segoe UI。
///
/// **画字与量字必须用同一份字体**：窗口开多大是由 [`text_extent`] 量出来的宽度决定的，
/// 两处字体不一致就会退回"宽度靠猜"。
///
/// SAFETY: 返回的 `HFONT` 由调用方 `DeleteObject` 释放。
unsafe fn window_font(font_height: i32) -> windows_sys::Win32::Graphics::Gdi::HFONT {
    use windows_sys::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
        FF_DONTCARE, FW_BOLD, OUT_TT_PRECIS,
    };

    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    // SAFETY: 字体名以 NUL 结尾、调用期间存活；其余参数都是按 Win32 契约填的常量。
    let font = unsafe {
        CreateFontW(
            font_height,
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
        )
    };
    assert!(!font.is_null(), "CreateFontW 失败");
    font
}

/// 量出 [`WINDOW_TEXT`] 在 `font_height` 号夹具字体下的像素尺寸。
///
/// **为什么要量而不是写死窗口宽度**：96px 粗体的文本宽度随字体与 DPI 变化。
/// 2026-11-20 的 CI（`windows-latest`）就是这样红的 —— 画进去的是
/// `"HBOT WIN OCR 7788"`，OCR 只认到 `"HBOT WIN OCR 77"`，最后两个字符落在
/// 写死的 1100×320 客户区之外被裁掉了；而同一份代码在本机（字体度量略窄）全绿。
/// 这类"本机绿、CI 红"的夹具问题比业务 bug 更难查，所以窗口尺寸改成按实测宽度开。
fn text_extent(font_height: i32) -> (i32, i32) {
    use windows_sys::Win32::Foundation::SIZE;
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    };

    let units: Vec<u16> = WINDOW_TEXT.encode_utf16().collect();
    // SAFETY: `GetDC(NULL)` 取屏幕 DC，返回空表示没有可用显示（无交互桌面）。
    let dc = unsafe { GetDC(std::ptr::null_mut()) };
    if dc.is_null() {
        // 没有桌面时这些用例本来就会在 `interactive_desktop()` 里跳过；这里给一个保守值，
        // 让窗口还能建出来，不要因为"量不了"就 panic。
        return (900, 120);
    }
    // SAFETY: `dc` 非空；字体在 SelectObject 之后才用，用完换回并删除。
    let (width, height) = unsafe {
        let font = window_font(font_height);
        let previous = SelectObject(dc, font);
        // SAFETY: `SIZE` 是 POD；`GetTextExtentPoint32W` 只在成功时写入。
        let mut size: SIZE = std::mem::zeroed();
        let ok = GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
        SelectObject(dc, previous);
        DeleteObject(font);
        assert_ne!(ok, 0, "GetTextExtentPoint32W 失败（量不出夹具文本的宽度）");
        (size.cx, size.cy)
    };
    // SAFETY: 与上面的 GetDC 成对；`GetDC(NULL)` 只能用 `ReleaseDC(NULL, …)` 释放。
    unsafe { ReleaseDC(std::ptr::null_mut(), dc) };
    (width, height)
}

/// 一次夹具窗口的布局：选定的字号、文本位置、客户区尺寸。
#[derive(Clone, Copy, Debug)]
struct Layout {
    font_height: i32,
    text_left: i32,
    text_top: i32,
    width: i32,
    height: i32,
}

/// 当前屏幕的分辨率（像素）。拿不到时给一个常见的小屏尺寸。
fn screen_extent() -> (i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    // SAFETY: 两个查询都只读系统指标，没有指针参数。
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if width <= 0 || height <= 0 {
        (1024, 768)
    } else {
        (width, height)
    }
}

/// 选一个**在当前屏幕上放得下**的布局。
///
/// 判据是「窗口整个落在屏幕内」，不是「文本量得出来」：窗口超出屏幕的部分不会被合成进
/// `PrintWindow` 的位图，OCR 读到的是被裁过的文字。所以从大字号往小试，第一个
/// `宽高都不超过屏幕` 的就是答案；画字与建窗都用它，两者不会错位。
fn current_layout() -> Layout {
    let (screen_width, screen_height) = screen_extent();
    for font_height in FONT_HEIGHT_CANDIDATES {
        let (text_width, text_height) = text_extent(font_height);
        let width = TEXT_LEFT + text_width + TEXT_MARGIN;
        let height = TEXT_TOP + text_height + TEXT_MARGIN;
        if width <= screen_width && height <= screen_height {
            return Layout {
                font_height,
                text_left: TEXT_LEFT,
                text_top: TEXT_TOP,
                width,
                height,
            };
        }
    }
    // 连最小字号都放不下（屏幕小到不合常理）：仍用最小字号，并把窗口与文本位置都夹进屏幕，
    // 让"窗口被裁"这件事至少不会额外发生。这时 OCR 可能仍然认不全，失败信息里会带出布局。
    let font_height = *FONT_HEIGHT_CANDIDATES.last().expect("字号候选列表非空");
    let (text_width, text_height) = text_extent(font_height);
    let text_left = TEXT_LEFT.min((screen_width / 8).max(1));
    let text_top = TEXT_TOP.min((screen_height / 8).max(1));
    Layout {
        font_height,
        text_left,
        text_top,
        width: (text_left + text_width + TEXT_MARGIN).min(screen_width),
        height: (text_top + text_height + TEXT_MARGIN).min(screen_height),
    }
}

/// 往 `hdc` 上画：[`WINDOW_TEXT`]（黑字、白底、粗体）。
///
/// SAFETY: 调用方保证 `hwnd`/`hdc` 有效。
unsafe fn paint_window_text(
    hwnd: windows_sys::Win32::Foundation::HWND,
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateSolidBrush, DeleteObject, FillRect, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT, TextOutW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    // SAFETY: `RECT` 是 POD；`GetClientRect` 只在成功时写入。
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: `hwnd` 有效；`&mut rect` 指向栈上有效内存。
    unsafe { GetClientRect(hwnd, &mut rect) };
    let units: Vec<u16> = WINDOW_TEXT.encode_utf16().collect();
    // 用**这个窗口建出来时选定的**字号与位置，而不是常量：两者必须一致，
    // 否则"按 A 号字量出的窗口"里画的是 B 号字，要么贴边要么留白过多。
    let layout = current_layout();
    // SAFETY: `hdc` 有效；画笔画完即删；字体由 `window_font` 建、用完即删；`units` 调用期间存活。
    unsafe {
        let brush = CreateSolidBrush(0x00FF_FFFF); // COLORREF：白
        assert!(!brush.is_null(), "CreateSolidBrush 失败");
        let _ = FillRect(hdc, &rect, brush);
        DeleteObject(brush);

        let font = window_font(layout.font_height);
        let previous = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, 0x0000_0000); // COLORREF：黑
        TextOutW(
            hdc,
            layout.text_left,
            layout.text_top,
            units.as_ptr(),
            units.len() as i32,
        );
        SelectObject(hdc, previous);
        DeleteObject(font);
    }
}

/// 一个真的可见的测试窗口（Drop 时销毁）。
struct TestWindow {
    hwnd: windows_sys::Win32::Foundation::HWND,
}

/// 类名必须唯一：同一进程里多次 `RegisterClassW` 同名会失败（`ERROR_CLASS_ALREADY_EXISTS`）。
static CLASS_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl TestWindow {
    /// 建一个顶层窗口；`visible` 为真时立刻显示（`SW_SHOWNOACTIVATE`：**不抢前台**）。
    ///
    /// 不抢前台是刻意的：CI 上抢焦点会打断正在用机器的人，而且"前台窗口优先"这条判据
    /// 会让本用例变成"测前台"，不是"测按窗口抓图"。
    fn create(title: &str) -> Self {
        use windows_sys::Win32::Graphics::Gdi::UpdateWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow,
            WNDCLASSW, WS_EX_NOACTIVATE, WS_POPUP,
        };

        let sequence = CLASS_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let class_name: Vec<u16> = format!("HbotDeviceOcrTest{}_{sequence}\0", std::process::id())
            .encode_utf16()
            .collect();
        let title_units: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        // **客户区按"当前屏幕上放得下的实测文本尺寸"开，不写死**：文本宽度随字体、DPI
        // 与字号变化，而窗口只要有一角在屏幕外，超出的部分就不会被合成进 `PrintWindow`
        // 的位图。2026-11-20 的 CI（`windows-latest`）两轮都是这样红的：第一轮写死
        // 1100×320，末尾两个字符被裁；第二轮按文本量出 1121 宽，仍然比 runner 的
        // 1024 宽屏幕大，于是 5 次重试稳定认成 `"HBOT WIN OCR 77"`。
        let layout = current_layout();

        // SAFETY: 所有指针都指向调用期间存活的、以 NUL 结尾的 UTF-16 缓冲区。
        unsafe {
            let instance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            assert!(!instance.is_null(), "GetModuleHandleW 失败");
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(ocr_window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            let atom = RegisterClassW(&class);
            assert_ne!(atom, 0, "RegisterClassW 失败（拿不到窗口类）");

            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE, // 显示但不激活：不抢前台
                class_name.as_ptr(),
                title_units.as_ptr(),
                WS_POPUP, // 无标题栏：PrintWindow 抓到的就是纯客户区
                0,        // 贴左上角：屏幕越小，能用的区域越应该留给窗口本身
                0,
                layout.width,
                layout.height,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW 失败（拿不到窗口）");
            // 先显示再 UpdateWindow：同步走一次 WM_PAINT，保证窗口里真的有内容。
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(hwnd);
            let window = Self { hwnd };
            window.settle(); // 等 DWM 合成第一帧（见 `settle` 的注释）
            window
        }
    }

    /// 最小化（`IsIconic` 立刻为真）。
    fn minimize(&self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_MINIMIZE, ShowWindow};
        // SAFETY: `hwnd` 由本结构持有，构造时已校验非空。
        unsafe { ShowWindow(self.hwnd, SW_MINIMIZE) };
    }

    /// 窗口句柄（"已关闭"用例要在 `Drop` 之前留一份，事后用它断言真的销毁了）。
    fn handle(&self) -> windows_sys::Win32::Foundation::HWND {
        self.hwnd
    }

    /// 抽一会儿消息，让 DWM 把这一帧真的合成出来。
    ///
    /// **这一步是实测出来的必需品**，不是偷懒的 sleep：窗口刚 `ShowWindow` 完、DWM 还没合成
    /// 第一帧时，`PrintWindow(PW_RENDERFULLCONTENT)` **返回 1 但位图全零**，`GetWindowDC`
    /// 的 `BitBlt` 读到的也还是窗口出现之前的像素 —— 两条路都拿不到内容，OCR 只能返回空串。
    /// 真实的被测窗口早就在屏幕上放了很久，不存在这个问题；只有"创建后立刻抓"的夹具才需要等。
    fn settle(&self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            // SAFETY: `MSG` 是 POD；`PeekMessageW` 只在返回非零时写入。
            unsafe {
                let mut message: MSG = std::mem::zeroed();
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
        // SAFETY: `hwnd` 由本结构持有；DestroyWindow 只调用一次（Drop 只跑一次）。
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// 本机有没有可交互桌面（没有就别建窗口，直接跳过 —— 那是宿主限制，不是代码问题）。
fn interactive_desktop() -> bool {
    let report = WindowsBackend::new().capabilities();
    if !report.screen {
        eprintln!("[window-ocr] 当前没有可交互桌面（screen=false）→ 跳过：{report:?}");
        return false;
    }
    true
}

/// **按窗口 OCR 的真机证据**：自建一个可见窗口，GDI 画字，按进程名让后端去读它。
///
/// 这条覆盖三件事：① 按映像名找人 → ② PrintWindow 抓窗口位图 → ③ WinRT OCR 出文字。
/// 走的是 `DeviceBackend::ocr` 这个公开入口，不是内部夹具。
#[test]
fn window_ocr_reads_text_from_a_real_window() {
    let _guard = window_test_guard();
    if !interactive_desktop() {
        return;
    }
    let window = TestWindow::create("HBOT 按窗口 OCR 夹具");
    let own = own_process_name();
    let expected = squashed(WINDOW_TEXT);

    // **最多试几次**：这条测的是"按窗口 OCR 能读到自建窗口里的字"，不是"DWM 第一帧就合成好了"。
    // 实测（2026-11-20）这条会偶发拿到空串 —— 窗口刚 `ShowWindow`、DWM 还没合成第一帧时
    // `PrintWindow` 返回成功但位图全零（见 `TestWindow::settle` 的注释），重试一次就好。
    // 断言本身没有放松：仍然必须认出**完整**的 `WINDOW_TEXT`，只是不再要求"第一次就必须成功"。
    let mut last: Option<String> = None;
    for attempt in 0..5 {
        let result = WindowsBackend::new().ocr(&OcrRequest {
            target: DeviceTarget::Window {
                process: own.clone(),
            },
            max_chars: 200,
        });
        match result {
            Ok(recognized) => {
                eprintln!(
                    "[window-ocr] 第 {} 次：进程 {own} 的窗口，画进去的是 {WINDOW_TEXT:?}，识别结果是 {:?}（truncated={}）",
                    attempt + 1,
                    recognized.text,
                    recognized.truncated
                );
                if squashed(&recognized.text).contains(&expected) {
                    drop(window);
                    return;
                }
                last = Some(recognized.text);
            }
            Err(DeviceError::HostLimit { detail }) => {
                // 没装 OCR 语言包 / 受限会话：引擎建不出来。这是宿主限制，不是按窗口抓图失败 ——
                // 打印原因后跳过（"引擎不可用"与"抓不到窗口"必须能区分开，见 `recognize_text` 注释）。
                eprintln!("[window-ocr] 本机 OCR 引擎不可用，跳过真识别：{detail}");
                drop(window);
                return;
            }
            Err(error) => panic!("自建可见窗口必须能被按窗口 OCR 读到，实际：{error:?}"),
        }
        window.settle(); // 再抽一会儿消息，让 DWM 把这一帧合成出来
    }
    panic!(
        "窗口里画的字没认全：画的是 {WINDOW_TEXT:?}，认出来是 {last:?}；屏幕 {screen:?}、布局 {layout:?}",
        screen = screen_extent(),
        layout = current_layout(),
    );
}

/// **最小化窗口不是可读窗口**：必须是明确错误，绝不返回空串冒充"窗口里没字"。
///
/// 空串在契约里是"这一帧没识别到字"的合法结果，所以拿空串当失败信号会让调用方无法区分
/// "窗口不可读"和"窗口真的是空的"。
#[test]
fn a_minimized_window_is_not_window_ocr_readable() {
    let _guard = window_test_guard();
    if !interactive_desktop() {
        return;
    }
    let window = TestWindow::create("HBOT 最小化夹具");
    window.minimize();
    let own = own_process_name();
    let result = WindowsBackend::new().ocr(&OcrRequest {
        target: DeviceTarget::Window {
            process: own.clone(),
        },
        max_chars: 100,
    });
    match result {
        Err(DeviceError::AppNotAllowed { process }) => {
            assert_eq!(process, own, "要把被拒的进程原样回给调用方");
            eprintln!("[window-ocr] 最小化窗口按预期被拒：{process}");
        }
        Err(DeviceError::HostLimit { detail }) => {
            eprintln!("[window-ocr] 本机 OCR 引擎不可用（HostLimit），跳过：{detail}");
        }
        other => {
            panic!("最小化的窗口必须返回明确的 AppNotAllowed（不许空串伪装成功），实际：{other:?}")
        }
    }
    drop(window);
}

/// **无标题窗口不是可读窗口**：模型没有标题可确认，读它没有意义 → 明确 `AppNotAllowed`。
///
/// 这条同时也是变异验证的钉子：把 `window_matches_process` 里"标题不能为空"那条删掉，
/// 这个用例就会读到窗口文字而变成 `Ok`，测试随之失败。
#[test]
fn a_window_without_a_title_is_not_window_ocr_readable() {
    let _guard = window_test_guard();
    if !interactive_desktop() {
        return;
    }
    let window = TestWindow::create("");
    let own = own_process_name();
    let result = WindowsBackend::new().ocr(&OcrRequest {
        target: DeviceTarget::Window {
            process: own.clone(),
        },
        max_chars: 100,
    });
    match result {
        Err(DeviceError::AppNotAllowed { process }) => {
            assert_eq!(process, own, "要把被拒的进程原样回给调用方");
            eprintln!("[window-ocr] 无标题窗口按预期被拒：{process}");
        }
        Err(DeviceError::HostLimit { detail }) => {
            eprintln!("[window-ocr] 本机 OCR 引擎不可用（HostLimit），跳过：{detail}");
        }
        other => panic!("无标题的窗口必须返回明确的 AppNotAllowed，实际：{other:?}"),
    }
    drop(window);
}

/// **已经关掉的窗口读不到**：句柄销毁后既不在 `EnumWindows` 里，也拿不到位图 →
/// 必须是明确的 `AppNotAllowed`（而不是 `Ok(OcrText { text: "" })`）。
///
/// 这条与"没有窗口"那条不同：这里窗口**曾经真的存在过、也画过字**，销毁之后才读；
/// 它能钉住"抓到一个死句柄也照样返回空串"这种退化。
#[test]
fn a_closed_window_is_not_window_ocr_readable() {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;

    let _guard = window_test_guard();
    if !interactive_desktop() {
        return;
    }
    let window = TestWindow::create("HBot 已关闭窗口夹具");
    let handle = window.handle();
    let own = own_process_name();
    drop(window); // DestroyWindow 在这里发生
    // SAFETY: `handle` 是刚被销毁的句柄；`IsWindow` 对无效句柄只返回 0，不解引用。
    assert_eq!(
        unsafe { IsWindow(handle) },
        0,
        "夹具窗口应当已经真的销毁（否则这条用例证明不了'关闭后的窗口'）"
    );

    let result = WindowsBackend::new().ocr(&OcrRequest {
        target: DeviceTarget::Window {
            process: own.clone(),
        },
        max_chars: 100,
    });
    match result {
        Err(DeviceError::AppNotAllowed { process }) => {
            assert_eq!(process, own, "要把被拒的进程原样回给调用方");
            eprintln!("[window-ocr] 已关闭窗口按预期被拒：{process}");
        }
        Err(DeviceError::HostLimit { detail }) => {
            eprintln!("[window-ocr] 本机 OCR 引擎不可用（HostLimit），跳过：{detail}");
        }
        other => panic!("关闭的窗口必须返回明确的 AppNotAllowed，实际：{other:?}"),
    }
}

#[test]
#[ignore = "需要真实桌面会话，且会读整屏内容"]
fn screenshot_captures_the_primary_monitor_as_a_valid_png() {
    let limit = 32 * 1024 * 1024;
    let shot = WindowsBackend::new()
        .screenshot(&ScreenshotRequest {
            monitor: None,
            max_bytes: limit,
        })
        .expect("真机截图应当成功");
    assert_eq!(shot.monitor, 0);
    assert!(shot.width > 0 && shot.height > 0, "宽高必须非零");
    assert!(shot.png.len() <= limit, "不许超过 max_bytes");
    assert_eq!(&shot.png[..8], &PNG_SIGNATURE, "输出必须是 PNG");
    assert_eq!(
        &shot.png[12..16],
        b"IHDR",
        "IHDR 必须紧跟签名（说明结构合法）"
    );
    let width = u32::from_be_bytes([shot.png[16], shot.png[17], shot.png[18], shot.png[19]]);
    let height = u32::from_be_bytes([shot.png[20], shot.png[21], shot.png[22], shot.png[23]]);
    assert_eq!(
        (width, height),
        (shot.width, shot.height),
        "IHDR 里的尺寸要与返回值一致"
    );
}

#[test]
#[ignore = "会改写用户剪贴板"]
fn clipboard_write_then_read_round_trips_unicode_and_restores_nothing() {
    // 手动跑：这个测试**会覆盖用户剪贴板内容**（原内容不备份，明确写在这里）。
    let text = "hbot-device 测试 ✓ 𝄞";
    let written = WindowsBackend::new()
        .clipboard(&ClipboardOp::Write {
            text: text.to_string(),
        })
        .expect("写剪贴板应当成功");
    assert_eq!(written.text, text);
    let read_back = WindowsBackend::new()
        .clipboard(&ClipboardOp::Read)
        .expect("读剪贴板应当成功");
    assert_eq!(read_back.text, text, "写进去的 Unicode 文本要能原样读回");
}
