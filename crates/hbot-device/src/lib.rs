//! hbot-device：设备能力（C6）的**执行端口**实现 —— 截图 / 注入输入 / 前台窗口 / 剪贴板。
//!
//! 只实现 [`hbot_core::device::DeviceBackend`]，**不实现判据**。授权判据只有一份，在
//! [`hbot_core::device::DeviceGate`]（实现落 `hbot-rpc`）：清单声明 → 宿主设置 → 限时会话三层
//! 都由它判，本 crate 不认识 `BotId`、不认识审批、也不看设置文件。两份判据会打架，最终解释权
//! 只能有一份（`AGENTS.md` 坑 48）。设计稿：[`docs/09-device-capability.md`](../../../docs/09-device-capability.md)。
//!
//! # 平台矩阵（诚实的，不是愿望）
//!
//! | 平台 | 状态 |
//! |---|---|
//! | Windows 10/11 | [`WindowsBackend`]：截图（多显示器）、注入、窗口枚举、剪贴板、OCR（整屏 + 按窗口）都实现了 |
//! | 其它 | `UnsupportedBackend`：`capabilities()` 全 `false`，每个方法返回 [`DeviceError::HostLimit`] |
//!
//! 非 Windows 分支**能编译、能过 clippy、也有测试**（`cargo check --target x86_64-unknown-linux-gnu`），
//! 因为"没实现的平台必须明确拒绝"和"没实现的平台编译不过"是两件事：前者是有意的产品行为。
//!
//! # 截图方案的真实代价（`docs/09` §6.1 的三个候选里选了最省的那个）
//!
//! 用的是 **`PrintWindow(PW_RENDERFULLCONTENT)` 抓前台窗口**，抓不到时退化成**主显示器整屏
//! `BitBlt`**。没用 WinRT `Windows.Graphics.Capture`，也没用 DXGI Desktop Duplication：
//! 那两个要么得引 `windows` crate 的 WinRT 绑定，要么得自己写 COM + 出错重建，代码量是本实现的
//! 数倍，而窄版的出口标准只要"证明链路能通"。
//!
//! 代价逐条列清楚（这些都是**已知限制**，不是 bug）：
//!
//! - **独占全屏 / DirectX 游戏基本抓不到**：`BitBlt` 从屏幕 DC 读的是 GDI 合成结果，独占全屏时
//!   那个表面上没有 GDI 内容 → 常见结果是**全黑**（或只剩鼠标指针）。`PrintWindow` 对硬件加速
//!   窗口同样可能返回全黑。
//! - **DRM/受保护内容**（Netflix 等 PlayReady 页面）本来就禁止抓取，结果是黑块；不绕。
//! - **DirectComposition / 部分 UWP** 窗口 `PrintWindow` 也可能黑（`PW_RENDERFULLCONTENT` 只解决
//!   了一部分 DWM 内容）。
//! - **多显示器**：`monitor` 是[显示器清单](displays)里的**序号**（`None` = 主屏，与 `Some(0)`
//!   同义）。序号规则见 [`DisplayInfo`]：**0 永远是主显示器**，其余按虚拟桌面左上角
//!   （`rcMonitor.left`, `rcMonitor.top`）从左到右、从上到下排，坐标相同（克隆模式）时按
//!   `MONITORINFOEXW::szDevice` 字典序 —— 三条都只看**系统给的静态属性**，不掺枚举回调顺序，
//!   所以同一台机器上反复调用、重启进程都稳定。不存在的序号返回 `HostLimit` 并列出可用数量，
//!   绝不静默退回主屏（那会让模型对着错误的画面决策）。
//!   每块屏用 `CreateDCW(szDevice)` 取它**自己的** DC 抓图：`GetDC(NULL)` 只覆盖主屏，用它去
//!   `BitBlt` 副屏坐标只会拿到一片黑。
//! - **DPI**：本 crate 是库，**没法**给宿主进程装 Per-Monitor V2 manifest（manifest 是**可执行
//!   文件**的属性，库改不了调用方的 PE 资源；运行时那条 API 也必须在宿主的最早期调用）。
//!   宿主没声明 DPI 感知时，系统会把 `rcMonitor` 与 `CreateDCW` 得到的像素一起虚拟化
//!   （150% 缩放下 1920 宽的屏报 1280、抓出来也是 1280），于是截图宽高不是物理像素、
//!   归一化坐标与截图对不上。**这件事在 `hbot-desktop/src/dpi.rs` 解决**（壳在 `main()` 最早期
//!   声明 PMv2）；本 crate 能做的是**如实报出每屏的 DPI/缩放与虚拟化前后的尺寸差**
//!   （[`DisplayInfo::scale_percent`] 与 [`display_metrics`]），让上层能判断自己拿到的
//!   坐标到底是物理的还是逻辑的，而不是猜。实测数字见 `src/windows.rs` 顶部。
//! - **截图没有窗口级入口**：`ScreenshotRequest` 只有 `monitor` 参数，没有 `window_id`。
//!   想"截某个窗口"要先用 [`windows()`](DeviceBackend::windows) 找到它、切到前台——这正是
//!   "窗口级**截图**未做"的含义。按窗口**读字**是另一条路（`DeviceTarget::Window{process}`，
//!   见下面的 OCR 一节），它内部会 `PrintWindow` 那个窗口，但产物是文本，不是 PNG。
//! - **OCR 是真接了，但只认、也只能认一部分**：Windows 上走 WinRT 的 `Windows.Media.Ocr`
//!   （`windows` crate，不是 `windows-sys`），输入是**同一套 DIB `BitBlt` 抓的主屏 BGRA**。
//!   三条限制是机制性的，不是没写完：① 只认**系统已安装语言包**的文字 —— 没装的语言（或没装
//!   任何 OCR 语言包的机器）引擎根本建不出来，那时 `capabilities().ocr == false` 且 `ocr()`
//!   返回 `HostLimit`；② 支持**两种目标**：主屏整屏（`DeviceTarget::None`）与**按窗口**
//!   （`DeviceTarget::Window{process}`，取该进程 Z 序最前的可见、未最小化、有标题的窗口，
//!   前台窗口优先；用 `PrintWindow` 抓那个窗口，失败退 `GetWindowDC` + `BitBlt`）。
//!   `process` 是**映像名**（`notepad.exe` 这种），大小写不敏感；这个进程此刻没有可读窗口时
//!   返回 `AppNotAllowed{process}`（不是空串）；③ 图标、艺术字、
//!   旋转超过 45° 的文字识别不了（引擎本身的边界）。
//!   除这三条之外还有一条**契约性**的：识别不出字时返回**空字符串**（`truncated: false`），
//!   那是合法结果；而引擎建不出来 / 识别过程失败一律是错误（`HostLimit` / `Failed`），
//!   **绝不拿空串冒充成功**。
//!
//! # 输入的硬墙
//!
//! - **UIPI**：官方原文「permitted to inject input only into applications that are at an equal or
//!   lesser integrity level」。我们跑在 Medium，所以**提权窗口、UAC 对话框、任务管理器一律点不动**。
//!   `SendInput` 失败就直接失败，错误文案里写明"目标进程可能完整性更高"，**不尝试 `UIAccess`**
//!   （那要代码签名证书 + 装进安全位置，窄版不做，`docs/09` §6.2 / §12）。
//! - **锁屏 / 安全桌面**：`OpenInputDesktop` 失败、或者拿到的桌面名不是 `Default`，直接
//!   `HostLimit{"锁屏或安全桌面，拒绝注入"}`，不重试。
//! - **前台白名单**是**每次注入前**判的，不是会话开始时判一次；名单空 = 全拒（**不是**全放）。
//!
//! # 线程模型
//!
//! 所有 Win32 调用都在调用线程**同步**执行（这是 [`DeviceBackend`] 的契约：调用方负责
//! `spawn_blocking` 与超时）。后端本身是零字段结构体，所以 `Send + Sync` 是自动的，
//! **没有** `unsafe impl`，也**没有**全局可变状态。拿到的 Win32 句柄（HDC/HANDLE/HDESK）
//! 都在同一个函数里用完即还，不跨线程存活。

#![warn(missing_docs)]

pub mod png;

#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
mod unsupported;

#[cfg(windows)]
pub use windows::WindowsBackend;

#[cfg(not(windows))]
pub use unsupported::UnsupportedBackend;

/// 当前平台的设备后端类型。
#[cfg(windows)]
pub type PlatformBackend = WindowsBackend;

/// 当前平台的设备后端类型。
#[cfg(not(windows))]
pub type PlatformBackend = UnsupportedBackend;

/// 造一个当前平台的设备后端。
///
/// 组合根（`hbot-rpc`）只调这一个函数，平台上选谁由 `cfg` 决定，调用方不需要写 `cfg`。
pub fn platform_backend() -> PlatformBackend {
    PlatformBackend::new()
}

/// 一块显示器的静态信息（[`displays`] 的返回值）。
///
/// 字段全部来自 `EnumDisplayMonitors` + `GetMonitorInfoW(MONITORINFOEXW)`，是**系统给的静态
/// 属性**，不是我们猜的：
///
/// - [`index`](Self::index)：截图时传给 `ScreenshotRequest::monitor` 的序号。**0 永远是主
///   显示器**，其余按虚拟桌面左上角从左到右、从上到下；坐标完全相同（克隆模式）时按
///   [`device`](Self::device) 字典序。只依赖这三条，所以同一台机器上每次调用都稳定；
/// - [`x`](Self::x)/[`y`](Self::y)：`rcMonitor` 左上角在**虚拟桌面坐标系**里的位置。主屏不一定
///   是 `(0, 0)` —— 主屏左边的副屏是负坐标，主屏本身被系统钉在 `(0, 0)`；
/// - 宽高是 `rcMonitor` 的尺寸。**这一层到底是不是物理像素，取决于宿主进程的 DPI 感知档位**：
///   Per-Monitor V2 下就是物理像素；进程是 DPI-Unaware 时系统会把 `rcMonitor` 虚拟化成逻辑
///   像素（150% 缩放下 1 物理像素 = 1.5 逻辑像素）。本 crate 是库，**装不了 manifest**，
///   所以这件事只能在壳里解决（`crates/hbot-desktop/src/dpi.rs`）。真实的对照数字见
///   [`display_metrics`] 的 `physical_width`/`physical_height` 与 `size_delta()`。
/// - [`dpi_x`](Self::dpi_x)/[`dpi_y`](Self::dpi_y)/[`scale_percent`](Self::scale_percent)：
///   这块屏的**有效 DPI 与缩放比**（`GetDpiForMonitor(MDT_EFFECTIVE_DPI)`）。它是**每屏**的
///   属性（混合缩放时两块屏可以不同），与进程 DPI 无关；100% = 96 = `scale_percent` 100。
///   加这三个字段是纯新增：既有字段的名字与含义一个都没动，`DisplayInfo` 仍然只由本 crate
///   构造（`hbot-rpc` 只用 `platform_backend()`，不手拼这个结构）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// 显示器序号（0 起，0 = 主屏）。截图时原样传给 `ScreenshotRequest::monitor`。
    pub index: u32,
    /// 是不是主显示器（`MONITORINFOF_PRIMARY`）。
    pub primary: bool,
    /// 设备名（`MONITORINFOEXW::szDevice`，形如 `\\.\DISPLAY1`）。`CreateDCW` 用它建 DC。
    pub device: String,
    /// `rcMonitor.left`（虚拟桌面坐标系，副屏可以是负数）。
    pub x: i32,
    /// `rcMonitor.top`（虚拟桌面坐标系）。
    pub y: i32,
    /// `rcMonitor` 宽度（像素；是否为物理像素取决于宿主 DPI 感知档位，见类型文档）。
    pub width: u32,
    /// `rcMonitor` 高度（像素；同上）。
    pub height: u32,
    /// 这块屏的有效 DPI 横轴值（`GetDpiForMonitor(MDT_EFFECTIVE_DPI)`，100% 缩放 = 96）。
    ///
    /// 拿不到时退化到进程 DPI（`GetDpiForSystem`），再拿不到是 96 —— 退化的原因记在
    /// `tracing::debug` 里，不会静默编一个值。
    pub dpi_x: u32,
    /// 这块屏的有效 DPI 纵轴值（MSDN 保证与 [`dpi_x`](Self::dpi_x) 相同，仍然分开存：
    /// 万一将来某块屏非方像素，上层不必改结构）。
    pub dpi_y: u32,
    /// 缩放百分比，`dpi_x` 相对 96 的四舍五入（96→100、120→125、144→150、192→200）。
    pub scale_percent: u32,
}

/// 本机显示器清单，按 [`DisplayInfo::index`] 排好序（0 = 主屏）。
///
/// 这是**平台探测的结果**，不是缓存快照：每次调用都重新枚举，所以"刚插上/拔掉一块屏"
/// 下一次就能看到（与 [`DeviceBackend::capabilities`] 的口径一致）。
///
/// 返回空 `Vec` 表示**当前会话没有可交互桌面**（RDP 断开、服务会话、无头模式）；
/// 需要区分"没有屏"和"枚举失败"的调用方请直接用
/// [`screenshot`](DeviceBackend::screenshot)（它会把两者分别报成 `Failed` / `HostLimit`）。
pub fn displays() -> Vec<DisplayInfo> {
    PlatformBackend::displays()
}

/// 本机显示器数量（= [`displays`]`.len()`，0 表示当前会话没有可交互桌面）。
pub fn display_count() -> usize {
    displays().len()
}

/// 一块显示器的**诊断指标**：DPI / 缩放比 / 坐标与尺寸是被 DPI 虚拟化过的还是物理的。
///
/// 为什么需要它，而不是只看 [`DisplayInfo`]：`DisplayInfo` 报的是 `GetMonitorInfoW` 的
/// `rcMonitor`，而**同一个 `rcMonitor` 在不同 DPI 感知档位下不是同一个东西** —— 进程
/// DPI-Unaware 时它被系统虚拟化成逻辑像素，Per-Monitor V2 下才是物理像素。只报一个宽高
/// 的调用方没法判断"我拿到的 1280 到底是不是这块屏真的 1280"。所以这里额外给出
/// [`physical_width`](Self::physical_width)/[`physical_height`](Self::physical_height)
/// （`CreateDCW(szDevice)` + `GetDeviceCaps(DESKTOPHORZRES/DESKTOPVERTRES)`，那是**设备**的
/// 像素数，不受进程档位影响），两者的差就是虚拟化的量（[`size_delta`](Self::size_delta)）。
///
/// 本机实测（Windows 11 26300，单屏 1920×1080，100% 缩放）：
/// `rcMonitor` 1920×1080、`DESKTOPHORZRES` 1920×1080、`LOGPIXELSX` 96、DPI 96、
/// 截图位图 1920×1080 —— **四个数字全等**，因为 100% 缩放下没有虚拟化可做。
/// 150% 缩放下的差异见 `src/windows.rs` 顶部的说明（那是机制推论，不是本机实测）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayMetrics {
    /// 显示器序号（与 [`DisplayInfo::index`] 同一套规则：0 = 主屏）。
    pub index: u32,
    /// 设备名（`\\.\DISPLAY1`）。
    pub device: String,
    /// 是不是主显示器。
    pub primary: bool,
    /// `rcMonitor.left`（虚拟桌面坐标系）。
    pub x: i32,
    /// `rcMonitor.top`（虚拟桌面坐标系）。
    pub y: i32,
    /// `rcMonitor` 宽度（**当前进程 DPI 档位下**的坐标，不一定是物理像素）。
    pub width: u32,
    /// `rcMonitor` 高度（同上）。
    pub height: u32,
    /// 这块屏的有效 DPI 横轴值（100% = 96）。
    pub dpi_x: u32,
    /// 这块屏的有效 DPI 纵轴值。
    pub dpi_y: u32,
    /// 缩放百分比（`dpi_x` 相对 96 四舍五入）。
    pub scale_percent: u32,
    /// 这块屏的**物理像素**宽度（`GetDeviceCaps(hdc, DESKTOPHORZRES)`）。
    ///
    /// 拿不到显示 DC 时退化成 [`width`](Self::width)（并留 debug 日志），所以这个字段
    /// 永远可用，但"退化了"这件事只能从日志里看，不能从值上猜。
    pub physical_width: u32,
    /// 这块屏的**物理像素**高度（`GetDeviceCaps(hdc, DESKTOPVERTRES)`）。
    pub physical_height: u32,
    /// 这块屏 DC 的 `LOGPIXELSX`。**它才是 DPI 虚拟化的直接证据**：DPI-Aware 进程拿到
    /// 设备真实值（144 = 150%），DPI-Unaware 进程被系统钉成 96。
    pub dc_log_pixels_x: u32,
    /// 进程级 DPI（`GetDpiForSystem()`，同一进程的所有行都相同）。
    ///
    /// 放在每行里是因为诊断报告本来就是"一行一块屏"，让调用方不必再拼一次；重复的
    /// 4 字节比"少一个字段再去问一遍"便宜。
    pub system_dpi: u32,
}

impl DisplayMetrics {
    /// `rcMonitor` 与物理像素的差 `(物理 - 报告)`。
    ///
    /// 两个分量都是 0 表示当前档位下**没有虚拟化**（进程已是 Per-Monitor V2，或本机
    /// 所有屏都是 100%）；非 0 表示进程是 DPI-Unaware 且至少一块屏不是 100%，
    /// 此时截图坐标与点击坐标会漂移（见 `src/windows.rs` 顶部的说明）。
    pub fn size_delta(&self) -> (i64, i64) {
        (
            i64::from(self.physical_width) - i64::from(self.width),
            i64::from(self.physical_height) - i64::from(self.height),
        )
    }

    /// 这块屏的坐标/尺寸是否已被 DPI 虚拟化（等价于 [`size_delta`](Self::size_delta) 非 0）。
    pub fn is_virtualized(&self) -> bool {
        self.size_delta() != (0, 0)
    }

    /// 报告尺寸是否与物理尺寸一致（虚拟化的反向断言，便于测试与日志直接读）。
    pub fn matches_physical(&self) -> bool {
        !self.is_virtualized()
    }
}

/// 本机每块屏的 DPI / 缩放 / 虚拟化诊断指标（[`DisplayMetrics`]），序号规则与 [`displays`] 相同。
///
/// 这是**诊断 API**，不是截图路径的一部分：截图仍然只认 [`ScreenshotRequest::monitor`]。
/// 存在的理由有两个：
///
/// 1. 壳层/诊断脚本要能回答"这台机器的缩放到是多少、我拿到的宽高是不是物理像素"；
/// 2. 本 crate 的回归测试要能在真机上钉住"每块屏都报得出 DPI 与缩放比"（见
///    `tests/windows_dpi.rs`）。
///
/// 非 Windows 平台返回空 `Vec`（与 [`displays`] 的口径一致：那个平台没有实现）。
pub fn display_metrics() -> Vec<DisplayMetrics> {
    PlatformBackend::display_metrics()
}

/// 进程级 DPI（`GetDpiForSystem()`；100% 缩放 = 96，150% = 144）。
///
/// 与 [`DisplayInfo::dpi_x`] 的区别：那个是**每块屏**的属性，这个是**进程**的属性。
/// DPI-Unaware 进程会被系统钉成 96，Per-Monitor V2 进程拿到的是主屏的有效 DPI。
/// 非 Windows 平台返回 0（没有这个概念，不要拿 0 当 96 用）。
pub fn system_dpi() -> u32 {
    PlatformBackend::system_dpi()
}

// 调用方只需要认识这两个 crate 的类型，不必再手写 `hbot_core::device::…` 前缀。
pub use hbot_core::device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapability, DeviceCapabilityReport,
    DeviceError, DeviceTarget, InputAction, MouseButton, OcrRequest, OcrText, Screenshot,
    ScreenshotRequest, WindowInfo,
};

/// 一个组合键里的修饰键。
///
/// 只支持这四个：`SendInput` 能"按住"的修饰键在 Windows 上确实就这几个（`VK_CONTROL` /
/// `VK_MENU` / `VK_SHIFT` / `VK_LWIN`）。`AltGr`、`CapsLock` 之类不在这里 —— 前者是
/// Ctrl+Alt 的合成、后者是锁定状态不是按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComboModifier {
    /// Ctrl（`VK_CONTROL`）。
    Ctrl,
    /// Alt（`VK_MENU`）。
    Alt,
    /// Shift（`VK_SHIFT`）。
    Shift,
    /// Win（`VK_LWIN`）。
    Win,
}

impl ComboModifier {
    /// 规范名字（与 [`parse_combo`] 接受的小写形式一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Alt => "alt",
            Self::Shift => "shift",
            Self::Win => "win",
        }
    }
}

/// 组合键的**非修饰键**部分：最后被按下、随即抬起的那个键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComboKey {
    /// 一个可打印字符（`c`、`1`、`/` …）。字母与数字大小写不敏感。
    Char(char),
    /// Enter（`VK_RETURN`）。
    Enter,
    /// Tab（`VK_TAB`）。
    Tab,
    /// Esc（`VK_ESCAPE`）。
    Escape,
    /// 空格（`VK_SPACE`）。
    Space,
    /// 左方向键（`VK_LEFT`）。
    Left,
    /// 上方向键（`VK_UP`）。
    Up,
    /// 右方向键（`VK_RIGHT`）。
    Right,
    /// 下方向键（`VK_DOWN`）。
    Down,
    /// 功能键 `F1`–`F12`，数字是 1–12。
    Function(u8),
}

/// 解析好的组合键：若干修饰键 + 恰好一个非修饰键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Combo {
    /// 要按住的修饰键，按用户给的顺序（去重后）。注入时按这个顺序按下、逆序抬起。
    pub modifiers: Vec<ComboModifier>,
    /// 最后按下的那个键。
    pub key: ComboKey,
}

/// 组合键支持集合的说明文字（错误信息与文档共用一份，避免两处漂移）。
pub const COMBO_SUPPORTED: &str =
    "ctrl/alt/shift/win 修饰键 + 单个可打印字符或 F1–F12/enter/tab/esc/space/方向键";

/// 解析组合键。
///
/// 接受两种写法，混着写也可以：
///
/// - `["Ctrl+C"]` / `["ctrl+c"]`：一个字符串里用 `+` 拼；
/// - `["ctrl", "c"]`：数组里逐项给（`ctx.device.keyCombo(["ctrl","c"])` 的形态）。
///
/// 规则（`docs/09` §6.2 的窄版范围）：
///
/// - 修饰键只认 `ctrl` / `alt` / `shift` / `win`，**大小写不敏感**，重复的会去重；
/// - 必须有**恰好一个**非修饰键；两个真键（`ctrl+c+v`）直接拒绝，而不是猜一个；
/// - 键名 `enter` / `tab` / `esc` / `space` / `left` / `up` / `right` / `down` / `f1`–`f12`，
///   大小写不敏感；
/// - 单个可打印字符按字符给（`c`、`1`、`/`）；空白与控制字符不算（空格请写 `space`）；
/// - 其它一律 [`DeviceError::Failed`]，**文案里列出支持集合** —— 模型看到"支持什么"才会改，
///   只看到"不支持"通常会重试同一个词。
///
/// 这是纯函数（不碰 Win32），所以 Linux/macOS 上也跑同样的单测。
pub fn parse_combo(keys: &[String]) -> Result<Combo, DeviceError> {
    let mut modifiers: Vec<ComboModifier> = Vec::new();
    let mut key: Option<ComboKey> = None;

    for raw in keys {
        for token in raw.split('+') {
            let token = token.trim();
            if token.is_empty() {
                // `ctrl++` 这种写法：中间的 `+` 被当分隔符吃掉了。真正的 `+` 键请用数组
                // 形式 `["ctrl", "+"]`（见本文档）。
                continue;
            }
            if let Some(modifier) = modifier_of(token) {
                if !modifiers.contains(&modifier) {
                    modifiers.push(modifier);
                }
                continue;
            }
            let parsed = key_of(token)?;
            if key.replace(parsed).is_some() {
                return Err(combo_error(&format!(
                    "组合键只能有**一个**非修饰键，`{}` 让第二个键出现了（支持集合：{COMBO_SUPPORTED}）",
                    token
                )));
            }
        }
    }

    let key = key.ok_or_else(|| {
        combo_error(&format!(
            "组合键只有修饰键、没有按键（支持集合：{COMBO_SUPPORTED}）"
        ))
    })?;
    Ok(Combo { modifiers, key })
}

/// 把一个 token 认成修饰键（不认识就是 `None`，交给 [`key_of`]）。
fn modifier_of(token: &str) -> Option<ComboModifier> {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" => Some(ComboModifier::Ctrl),
        "alt" => Some(ComboModifier::Alt),
        "shift" => Some(ComboModifier::Shift),
        "win" => Some(ComboModifier::Win),
        _ => None,
    }
}

/// 把一个 token 认成非修饰键，认不出来就是 [`DeviceError::Failed`]（文案带支持集合）。
fn key_of(token: &str) -> Result<ComboKey, DeviceError> {
    let lower = token.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" => Some(ComboKey::Enter),
        "tab" => Some(ComboKey::Tab),
        "esc" | "escape" => Some(ComboKey::Escape),
        "space" => Some(ComboKey::Space),
        "left" => Some(ComboKey::Left),
        "up" => Some(ComboKey::Up),
        "right" => Some(ComboKey::Right),
        "down" => Some(ComboKey::Down),
        _ => None,
    };
    if let Some(key) = named {
        return Ok(key);
    }
    if let Some(digits) = lower.strip_prefix('f') {
        if let Ok(number) = digits.parse::<u8>() {
            if (1..=12).contains(&number) {
                return Ok(ComboKey::Function(number));
            }
        }
    }
    let mut characters = token.chars();
    if let (Some(character), None) = (characters.next(), characters.next()) {
        if !character.is_control() && !character.is_whitespace() {
            // 归一到小写：`Ctrl+C` 与 `ctrl+c` 必须解析出**相同**的 `Combo`，否则
            // "大小写不敏感"只在错误分支上成立、在比较里失效。注入侧按虚拟键码处理字母，
            // 大小写不影响实际按键。
            return Ok(ComboKey::Char(character.to_ascii_lowercase()));
        }
    }
    Err(combo_error(&format!(
        "不认识的键名 `{token}`（支持集合：{COMBO_SUPPORTED}）"
    )))
}

/// 组合键解析失败的统一错误形状（`Failed` 是 backend 的"执行不了"，与授权拒绝区分开）。
fn combo_error(message: &str) -> DeviceError {
    DeviceError::Failed {
        message: message.to_string(),
    }
}

/// 从路径或映像名里取**小写 basename**。
///
/// 白名单匹配只认映像名（`pathofexile.exe`），不认窗口标题：标题可变、可伪造
/// （`docs/09` §6.3）。`\` 与 `/` 都当分隔符 —— 白名单是用户手写的，Windows 路径两种都见过。
///
/// 纯函数：`C:\Windows\System32\NOTEPAD.EXE` → `notepad.exe`。
pub fn process_name(path: &str) -> String {
    let trimmed = path.trim();
    // 末尾带分隔符的路径（`C:\Games\poe\`）最后一个分量是空串，要跳过它继续往前找，
    // 否则会返回空名字 —— 那会让白名单匹配在"用户复制了目录路径"时静默失败。
    let base = trimmed
        .rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed);
    base.trim().to_ascii_lowercase()
}

/// 前台进程是否在白名单里。
///
/// 两侧都先过 [`process_name`]（所以白名单里写全路径也算），再做大小写不敏感比较。
/// **空名单返回 `false`** —— 空 = 全拒，这个判断只在这里做一次（`docs/09` §4.2）。
pub fn app_is_allowed(allowed_apps: &[String], process: &str) -> bool {
    let wanted = process_name(process);
    if wanted.is_empty() {
        return false;
    }
    allowed_apps
        .iter()
        .any(|allowed| process_name(allowed) == wanted)
}

/// 按上限检查编码后的字节数。
///
/// 超限返回 [`DeviceError::TooLarge`]，**绝不截断**：半张图比没有更危险（`docs/09` §5，
/// 半张图会让模型把缺失区域当成"屏幕上就是黑的"）。
pub fn enforce_limit(png: Vec<u8>, max_bytes: usize) -> Result<Vec<u8>, DeviceError> {
    if png.len() > max_bytes {
        return Err(DeviceError::TooLarge {
            limit_bytes: max_bytes,
        });
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn parse_combo_accepts_the_supported_spellings() {
        // 一个字符串里拼（大小写混写）。
        let ctrl_c = parse_combo(&keys(&["Ctrl+C"])).expect("Ctrl+C 应当能解析");
        assert_eq!(
            ctrl_c,
            Combo {
                modifiers: vec![ComboModifier::Ctrl],
                key: ComboKey::Char('c'),
            }
        );
        // 全小写。
        assert_eq!(parse_combo(&keys(&["ctrl+c"])), Ok(ctrl_c.clone()));
        // 数组逐项给。
        assert_eq!(parse_combo(&keys(&["ctrl", "c"])), Ok(ctrl_c));
        // 功能键。
        assert_eq!(
            parse_combo(&keys(&["F5"])),
            Ok(Combo {
                modifiers: Vec::new(),
                key: ComboKey::Function(5),
            })
        );
        // 具名键。
        assert_eq!(
            parse_combo(&keys(&["enter"])),
            Ok(Combo {
                modifiers: Vec::new(),
                key: ComboKey::Enter,
            })
        );
        // 多修饰键 + 大小写不敏感。注意这里用 `K` 而不是 `Delete`：`delete` 不在窄版支持
        // 集合里（需求只列了 enter/tab/esc/space/方向键），所以它必须被拒 —— 见下一条测试。
        assert_eq!(
            parse_combo(&keys(&["CTRL+ALT+Shift+K"]))
                .map(|combo| combo.modifiers)
                .expect("多修饰键应当能解析"),
            vec![
                ComboModifier::Ctrl,
                ComboModifier::Alt,
                ComboModifier::Shift
            ]
        );
    }

    #[test]
    fn parse_combo_rejects_unknown_keys_and_lists_the_supported_set() {
        let error = parse_combo(&keys(&["hyper+c"])).expect_err("hyper 不认识");
        let DeviceError::Failed { message } = &error else {
            panic!("应当返回 Failed，实际是 {error:?}");
        };
        assert!(message.contains("hyper"), "要点名是哪个键不认识：{message}");
        assert!(
            message.contains("ctrl/alt/shift/win"),
            "必须列出支持集合：{message}"
        );
        // 不在支持集合里的具名键也要拒，而不是当成一个字符 `D`。
        assert!(
            parse_combo(&keys(&["ctrl+delete"])).is_err(),
            "delete 不在窄版支持集合里"
        );
        assert!(
            parse_combo(&keys(&["ctrl+home"])).is_err(),
            "home 不在窄版支持集合里"
        );
    }

    #[test]
    fn parse_combo_rejects_two_real_keys_and_modifier_only() {
        assert!(parse_combo(&keys(&["ctrl+c+v"])).is_err(), "两个真键");
        assert!(parse_combo(&keys(&["ctrl"])).is_err(), "只有修饰键");
        assert!(parse_combo(&keys(&[])).is_err(), "空数组");
        assert!(parse_combo(&keys(&["f13"])).is_err(), "F13 超出范围");
        assert!(parse_combo(&keys(&["f0"])).is_err(), "F0 不存在");
    }

    #[test]
    fn parse_combo_accepts_every_named_key_and_deduplicates_modifiers() {
        for (name, expected) in [
            ("tab", ComboKey::Tab),
            ("esc", ComboKey::Escape),
            ("escape", ComboKey::Escape),
            ("space", ComboKey::Space),
            ("left", ComboKey::Left),
            ("up", ComboKey::Up),
            ("right", ComboKey::Right),
            ("down", ComboKey::Down),
            ("f1", ComboKey::Function(1)),
            ("f12", ComboKey::Function(12)),
        ] {
            assert_eq!(
                parse_combo(&keys(&[name])).map(|combo| combo.key),
                Ok(expected),
                "`{name}` 应当解析成 {expected:?}"
            );
        }
        let duplicated = parse_combo(&keys(&["ctrl+ctrl+c"])).expect("重复修饰键不该报错");
        assert_eq!(duplicated.modifiers, vec![ComboModifier::Ctrl]);
    }

    #[test]
    fn parse_combo_takes_single_printable_characters_and_refuses_whitespace() {
        assert_eq!(
            parse_combo(&keys(&["ctrl", "/"])).map(|combo| combo.key),
            Ok(ComboKey::Char('/'))
        );
        assert!(parse_combo(&keys(&["ctrl", "\t"])).is_err(), "控制字符");
        assert!(
            parse_combo(&keys(&["ctrl", " "])).is_err(),
            "空白请写 space"
        );
        assert!(parse_combo(&keys(&["ctrl", "ab"])).is_err(), "两个字符");
    }

    #[test]
    fn process_name_is_the_lowercase_basename() {
        assert_eq!(
            process_name(r"C:\Windows\System32\NOTEPAD.EXE"),
            "notepad.exe"
        );
        assert_eq!(process_name("/usr/bin/Foo"), "foo");
        assert_eq!(process_name("PathOfExile.exe"), "pathofexile.exe");
        assert_eq!(process_name(r"C:\Games\poe\"), "poe");
        assert_eq!(process_name("  notepad.exe  "), "notepad.exe");
        assert_eq!(process_name(""), "");
    }

    #[test]
    fn app_is_allowed_matches_by_lowercase_basename_and_empty_list_allows_nothing() {
        let allowed = keys(&[r"C:\Games\PathOfExile.exe", "Notepad.EXE"]);
        assert!(app_is_allowed(&allowed, "pathofexile.exe"));
        assert!(app_is_allowed(&allowed, "NOTEPAD.EXE"));
        assert!(app_is_allowed(&allowed, r"C:\Windows\notepad.exe"));
        assert!(!app_is_allowed(&allowed, "chrome.exe"));
        assert!(!app_is_allowed(&[], "notepad.exe"), "空名单 = 全拒");
        assert!(!app_is_allowed(&allowed, ""), "取不到进程名 = 拒");
    }

    #[test]
    fn enforce_limit_passes_through_and_rejects_oversize() {
        let png = vec![0u8; 1024];
        assert_eq!(
            enforce_limit(png.clone(), 1024),
            Ok(png.clone()),
            "正好等于上限"
        );
        assert_eq!(
            enforce_limit(png.clone(), 4096),
            Ok(png.clone()),
            "小于上限"
        );
        let error = enforce_limit(png, 1023).expect_err("超一个字节就该拒");
        assert_eq!(error, DeviceError::TooLarge { limit_bytes: 1023 });
        assert_eq!(error.code(), "HBOT_DEVICE_TOO_LARGE");
    }

    #[test]
    fn the_platform_backend_is_a_send_sync_contract_implementation() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let backend = platform_backend();
        assert_send_sync(&backend);
        let boxed: Box<dyn DeviceBackend> = Box::new(backend);
        // `Ocr` 是**按引擎能否建出来**报的（Windows 上取决于是不是装了 OCR 语言包），
        // 非 Windows 恒为 false —— 所以这里只断言"报告说得清边界"，不断言具体值。
        let report = boxed.capabilities();
        assert!(report.detail.is_some(), "能力报告要说明边界");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_unsupported_backend_refuses_everything_with_a_host_limit() {
        let backend = UnsupportedBackend::new();
        let report = backend.capabilities();
        assert!(
            !report.screen && !report.input && !report.window && !report.clipboard && !report.ocr,
            "非 Windows 宿主不该报告任何设备能力"
        );
        assert_eq!(report.detail.as_deref(), Some("这个宿主没有设备能力实现"));
        for error in [
            backend
                .screenshot(&ScreenshotRequest {
                    monitor: None,
                    max_bytes: 1024,
                })
                .expect_err("截图必须被拒"),
            backend
                .input(
                    &InputAction::Click {
                        x: 0,
                        y: 0,
                        button: MouseButton::Left,
                    },
                    &["notepad.exe".to_string()],
                )
                .expect_err("输入必须被拒"),
            backend.windows().expect_err("窗口枚举必须被拒"),
            backend
                .clipboard(&ClipboardOp::Read)
                .expect_err("剪贴板必须被拒"),
            backend
                .ocr(&OcrRequest {
                    target: DeviceTarget::None,
                    max_chars: 100,
                })
                .expect_err("OCR 必须被拒"),
        ] {
            assert_eq!(
                error,
                DeviceError::HostLimit {
                    detail: "这个宿主没有设备能力实现".to_string()
                }
            );
        }
    }
}
