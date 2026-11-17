//! **Per-Monitor V2 DPI 感知**：在壳里声明、读回真实档位、并把证据记进日志。
//!
//! # 为什么这件事只能在壳里做
//!
//! DPI 感知是**进程**的属性，声明它只有两条路：
//!
//! 1. **PE manifest**（`<dpiAwareness>PerMonitorV2</dpiAwareness>`）—— 首选，但它挂的是**可执行
//!    文件**的资源。`hbot-device` 是库，改不了调用方的 exe；手工用 `embed-manifest` 之类生成
//!    `.res` 又要引新依赖（本批明令禁止）。
//! 2. **运行时 API**（`SetProcessDpiAwarenessContext`）—— 必须在**任何窗口/WebView 创建之前**
//!    调用，否则系统已经按旧档位算过一遍了。所以它属于 `run()` 的最早期，不属于库。
//!
//! 于是分工是：本模块声明进程档位；`hbot-device` 只**读**每屏 DPI/缩放与虚拟化前后的尺寸差
//! （`hbot_device::display_metrics()`），不改变进程状态。
//!
//! # 声明之后行为变什么（本机实测，Windows 11 26300、单屏 1920×1080、100% 缩放）
//!
//! | 量 | 声明前（DPI-Unaware） | 声明后（Per-Monitor V2） |
//! |---|---|---|
//! | `GetThreadDpiAwarenessContext` → 档位 | Unaware | Per-Monitor V2 |
//! | `GetDpiForSystem()` | 96 | 96 |
//! | `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` | 96 | 96 |
//! | `rcMonitor`（`GetMonitorInfoW`） | 1920×1080 | 1920×1080 |
//! | `GetDeviceCaps(DESKTOPHORZRES)` | 1920×1080 | 1920×1080 |
//! | `GetDeviceCaps(LOGPIXELSX)` | 96 | 96 |
//! | 截图位图 | 1920×1080 | 1920×1080 |
//!
//! **这台机器是 100% 缩放，所以两种档位下所有数字都相同** —— 没有虚拟化可做。这张表能证明的
//! 是"100% 缩放下两种档位等价"，不能证明虚拟化存在。150% 缩放下（系统文档保证的行为，本机
//! 无 150% 可测，如实标注未实测）：声明前 `rcMonitor` 与截图位图都会变成 `1920/1.5 = 1280`
//! 宽（逻辑像素），声明后仍是 1920（物理像素）；`LOGPIXELSX` 声明前被钉成 96、声明后是 144。
//!
//! 由此回答计划里那个问题：**声明 Per-Monitor V2 后，`rcMonitor` 是物理像素，`CreateDCW`
//! 抓到的位图尺寸等于 `rcMonitor` 的宽高**（本机 1920×1080 = 1920×1080）；而声明**之前**两者
//! 也相等，只是两者都是**逻辑像素**。这正是上一批"跨屏点击偏差"的根因：`SendInput` 吃的是
//! 物理像素坐标，截图/`rcMonitor` 给的是逻辑像素，两者在非 100% 缩放下差一个 1.5 倍的因子。
//!
//! # 一个必须说清的现状：tao 自己也会声明
//!
//! Tauri 的窗口层 tao 0.37.2 在 `EventLoop::new` 里已经调了同一个 API（`dpi_aware` 属性默认开），
//! 所以**真正的 Tauri 应用即使没有本模块，最终也是 Per-Monitor V2**。本模块仍然必要，因为：
//!
//! 1. 它发生在 `Builder::run` **之前** —— `EventLoop::new` 之前的那段（单实例插件、崩溃 hook、
//!    预取核心）也处在已声明的档位下，行为一致、不会出现"前 100ms 与之后不同"；
//! 2. 它**读回**真实档位并记日志，`--dpi-probe` 能在不开窗口的情况下回答"这个进程现在是什么
//!    档位"；tao 的声明是静默的，没有任何可观测输出；
//! 3. 非 Tauri 宿主（`hbot-cli` / 后台服务）不经过 tao，只有本模块这条路径能给出同一套语义；
//! 4. 老 Windows 上的三档回退（PMv2 → PMv1 → `SetProcessDpiAwareness` → `SetProcessDPIAware`）
//!    是显式写出来的，出问题能定位到具体哪一档生效。
//!
//! # 幂等与失败
//!
//! `declare()` 可以随便多调（每次先读档位，已经是 PMv2 就直接返回 `AlreadyDeclared`），
//! 失败**不 panic**：拿不到 API 就打 `warn` 继续跑 —— DPI 声明失败最多是坐标不准，
//! 不该让应用起不来。

use tracing::{info, warn};

/// 进程/线程的 DPI 感知档位（`GetThreadDpiAwarenessContext` 的语义分类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awareness {
    /// 不感知（96 DPI 一套坐标，系统帮忙虚拟化）。
    Unaware,
    /// 系统级感知（Win8.1 之前唯一的一档）。
    SystemAware,
    /// Per-Monitor V1（Win8.1）。
    PerMonitor,
    /// Per-Monitor V2（Win10 1703+，我们要的这一档）。
    PerMonitorV2,
    /// 认不出来的档位（比 `DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED` 这类更新的枚举）。
    Unknown,
}

impl Awareness {
    /// 日志/JSON 里用的稳定名字（不要改：`--dpi-probe` 的输出被脚本消费）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Unaware => "unaware",
            Self::SystemAware => "system-aware",
            Self::PerMonitor => "per-monitor-v1",
            Self::PerMonitorV2 => "per-monitor-v2",
            Self::Unknown => "unknown",
        }
    }
}

/// 实际生效的那一步（诊断：出问题时能定位到哪一档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`。
    PerMonitorV2,
    /// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE)` 或
    /// `SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE)`（Win8.1 的两条等价路径）。
    PerMonitorV1,
    /// `SetProcessDPIAware()`（Vista+，只能到 System-Aware）。
    SystemAware,
    /// 调用前就已经是 Per-Monitor V2（清单或更早的调用声明过了）。
    AlreadyDeclared,
    /// 非 Windows 平台：没有 DPI 概念。
    NotWindows,
    /// 全部尝试都失败（极老的 Windows，或档位已被 manifest 钉死成别的）。
    Failed,
}

impl Api {
    /// 日志/JSON 里用的稳定名字（不要改：`--dpi-probe` 的输出被脚本消费）。
    pub fn name(self) -> &'static str {
        match self {
            Self::PerMonitorV2 => "SetProcessDpiAwarenessContext(PerMonitorV2)",
            Self::PerMonitorV1 => "SetProcessDpiAwarenessContext(PerMonitorV1)",
            Self::SystemAware => "SetProcessDPIAware",
            Self::AlreadyDeclared => "already-declared",
            Self::NotWindows => "not-windows",
            Self::Failed => "failed",
        }
    }
}

/// 一次声明的结果：**声明前档位 / 声明后读回的档位 / 生效的那一步**。
///
/// `after` 是**读回来**的，不是"我请求的"：Windows 允许系统拒绝（档位已被 manifest 定死），
/// 只记请求值会把"没生效"写成"生效了"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// 调用前的档位。
    pub before: Awareness,
    /// 调用后（`GetThreadDpiAwarenessContext` 读回）的档位。
    pub after: Awareness,
    /// 实际生效的那一步。
    pub api: Api,
}

impl Declaration {
    /// 是否真的到了 Per-Monitor V2（唯一算"成功"的判据）。
    pub fn is_per_monitor_v2(&self) -> bool {
        self.after == Awareness::PerMonitorV2
    }

    /// 一行摘要，给 `startup_trace` 与日志用。
    pub fn summary(&self) -> String {
        format!(
            "before={} after={} api={}",
            self.before.name(),
            self.after.name(),
            self.api.name()
        )
    }
}

/// 读当前的 DPI 感知档位（`GetThreadDpiAwarenessContext`）。
///
/// 非 Windows 平台恒为 [`Awareness::Unknown`]。
pub fn current() -> Awareness {
    #[cfg(windows)]
    let awareness = win::current();
    #[cfg(not(windows))]
    let awareness = Awareness::Unknown;
    awareness
}

/// 进程级 DPI（`GetDpiForSystem()`；100% = 96、150% = 144）。非 Windows 平台返回 0。
pub fn system_dpi() -> u32 {
    #[cfg(windows)]
    let dpi = win::system_dpi();
    #[cfg(not(windows))]
    let dpi = 0;
    dpi
}

/// 把本进程切成 **Per-Monitor V2**，失败时按 PMv1 → Per-Monitor(shcore) → System-Aware 逐级回退。
///
/// **必须在任何窗口/WebView 创建之前调用**（`run()` 里紧跟 `init_tracing()`，`Builder` 之前）。
/// 幂等：已经是 PMv2 时直接返回 [`Api::AlreadyDeclared`]；失败不 panic，只 `warn`。
///
/// 返回的 [`Declaration::after`] 是**读回**的档位，调用方应以它为准（而不是 `api`）。
pub fn declare() -> Declaration {
    #[cfg(windows)]
    let declaration = win::declare();
    #[cfg(not(windows))]
    let declaration = Declaration {
        before: Awareness::Unknown,
        after: Awareness::Unknown,
        api: Api::NotWindows,
    };

    if declaration.is_per_monitor_v2() {
        info!(
            before = declaration.before.name(),
            api = declaration.api.name(),
            "DPI 感知：Per-Monitor V2 已生效"
        );
    } else {
        // 不 panic：DPI 声明失败只是坐标可能不准，不该让应用起不来。
        warn!(
            before = declaration.before.name(),
            after = declaration.after.name(),
            api = declaration.api.name(),
            "DPI 感知：没能拿到 Per-Monitor V2（坐标可能按逻辑像素算，见 src/dpi.rs 的表）"
        );
    }
    declaration
}

/// 主窗口建好之后记一条窗口级 DPI 证据（`GetDpiForWindow` + 该窗口的 DPI 上下文）。
///
/// 为什么要窗口级而不是只记进程级：真实生效的是**窗口**的 DPI 上下文，`GetDpiForWindow` 是
/// 唯一一个"这块屏 + 这个窗口"的交叉验证；进程档位对了但窗口上下文被别的东西改回 Unaware
/// 的情况是存在的（宿主链式声明）。
///
/// 非 Windows 平台是空操作。
#[cfg(windows)]
pub fn log_window_dpi<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager;

    let Some(window) = app.get_webview_window("main") else {
        warn!("找不到主窗口（label = main），跳过窗口 DPI 记录");
        return;
    };
    let Ok(hwnd) = window.hwnd() else {
        warn!("拿不到主窗口的 HWND，跳过窗口 DPI 记录");
        return;
    };
    // `hwnd` 是 `windows` crate 的 newtype（`HWND(pub *mut c_void)`）。这里只取裸指针，
    // **不** `use` 那个类型 —— 壳不为一个字段把 `windows` crate 写进依赖表。
    // 不加 `as` 转换：`0` 号字段本身就是 `*mut c_void`，再转一次是 no-op（clippy 会拦）。
    let raw: *mut core::ffi::c_void = hwnd.0;
    let awareness = win::window_awareness(raw);
    info!(
        window_dpi = win::window_dpi(raw).unwrap_or(0),
        window_awareness = awareness.name(),
        system_dpi = win::system_dpi(),
        per_monitor_v2 = awareness == Awareness::PerMonitorV2,
        "主窗口 DPI 证据"
    );
}

/// 主窗口建好之后记一条窗口级 DPI 证据（非 Windows 平台空操作，见 Windows 版本的文档）。
#[cfg(not(windows))]
pub fn log_window_dpi<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) {}

/// `--dpi-probe` 的实现：声明一次、把"声明前 / 声明后 / 进程 DPI"打成一行 JSON 后退出。
///
/// 存在的理由：验证 DPI 声明**不需要**把 GUI 起起来（起 GUI 要 WebView2 + 窗口，CI 上不一定
/// 有），而"进程现在是什么档位"这件事一条命令就能问清。
///
/// 退出码：`0` = Per-Monitor V2 已生效；`1` = 没拿到 PMv2（原因看 JSON 里的 `api`/`after`）。
pub fn probe() -> i32 {
    let before = current();
    let declaration = declare();
    let after = current();
    let report = serde_json::json!({
        "platform": if cfg!(windows) { "windows" } else { "other" },
        "awarenessBefore": before.name(),
        "awarenessAfter": after.name(),
        "requested": declaration.api.name(),
        "perMonitorV2": after == Awareness::PerMonitorV2,
        "systemDpi": system_dpi(),
    });
    println!("{report}");
    if after == Awareness::PerMonitorV2 {
        0
    } else {
        1
    }
}

/// Windows 实现。所有 Win32 调用都收在这里，`unsafe` 块因此只集中在几十行内。
#[cfg(windows)]
mod win {
    use super::{Api, Awareness, Declaration};
    use core::ffi::c_void;
    use windows_sys::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE, GetDpiForSystem,
        GetDpiForWindow, GetThreadDpiAwarenessContext, GetWindowDpiAwarenessContext,
        PROCESS_PER_MONITOR_DPI_AWARE, SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

    /// 上下文 → 档位的对照表。**顺序从新到旧**：`UNAWARE_GDISCALED`(-5) 也落在未知档位，
    /// 靠"认不出就是 Unknown"兜住，而不是硬塞进 Unaware 里假装认识。
    const CONTEXTS: [(DPI_AWARENESS_CONTEXT, Awareness); 4] = [
        (
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            Awareness::PerMonitorV2,
        ),
        (
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
            Awareness::PerMonitor,
        ),
        (DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, Awareness::SystemAware),
        (DPI_AWARENESS_CONTEXT_UNAWARE, Awareness::Unaware),
    ];

    /// 把上下文句柄翻译成档位。
    ///
    /// 用 `AreDpiAwarenessContextsEqual` 而不是比指针：上下文是**伪句柄**，同一个档位在不同
    /// 版本上可能是不同数值，只有这个 API 保证语义比较。
    pub(super) fn awareness_of(context: DPI_AWARENESS_CONTEXT) -> Awareness {
        for (candidate, awareness) in CONTEXTS {
            // SAFETY: 两个入参都是上下文句柄；这个 API 只做比较，不解引用。
            if unsafe { AreDpiAwarenessContextsEqual(context, candidate) } != 0 {
                return awareness;
            }
        }
        Awareness::Unknown
    }

    /// 当前线程（进程的默认线程上下文）的档位。
    pub(super) fn current() -> Awareness {
        // SAFETY: 无参数、无副作用，返回伪句柄（不归调用方释放）。
        awareness_of(unsafe { GetThreadDpiAwarenessContext() })
    }

    /// 进程级 DPI（拿不到按 96 —— 那是"不感知"的等价物，比 0 更能用）。
    pub(super) fn system_dpi() -> u32 {
        // SAFETY: 无参数、无副作用，只读系统设置。
        let dpi = unsafe { GetDpiForSystem() };
        if dpi > 0 { dpi } else { 96 }
    }

    /// 某个窗口的 DPI（拿不到返回 `None`，不要用 0 冒充）。
    pub(super) fn window_dpi(hwnd: *mut c_void) -> Option<u32> {
        // SAFETY: `hwnd` 由 Tauri 提供且窗口仍活着（调用点在 `setup` 里）；这个 API 对
        // 无效句柄返回 0，不会炸。
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        (dpi > 0).then_some(dpi)
    }

    /// 某个窗口所处的 DPI 上下文（真实生效的是它，不是线程上下文）。
    pub(super) fn window_awareness(hwnd: *mut c_void) -> Awareness {
        // SAFETY: 同上；无效句柄返回空上下文，`awareness_of` 会把它归到 `Unknown`。
        awareness_of(unsafe { GetWindowDpiAwarenessContext(hwnd) })
    }

    /// `SetProcessDpiAwarenessContext` 的封装（返回是否被接受）。
    fn try_context(context: DPI_AWARENESS_CONTEXT) -> bool {
        // SAFETY: 改本进程的档位；调用点保证在任何窗口创建之前（`run()` 里）。
        unsafe { SetProcessDpiAwarenessContext(context) != 0 }
    }

    /// 声明 Per-Monitor V2，失败逐级回退；返回**读回**的档位。
    pub(super) fn declare() -> Declaration {
        let before = current();
        if before == Awareness::PerMonitorV2 {
            return Declaration {
                before,
                after: before,
                api: Api::AlreadyDeclared,
            };
        }

        // 1) 首选：Per-Monitor V2（Win10 1703+）。返回 FALSE 的常见原因是"档位已经定过了"
        //    （manifest / tao / 更早的调用）—— 所以下面每一步都用 `current()` 读回再判断，
        //    而不是信返回值。
        if try_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            let after = current();
            if after == Awareness::PerMonitorV2 {
                return Declaration {
                    before,
                    after,
                    api: Api::PerMonitorV2,
                };
            }
        }

        // 2) 次选：Per-Monitor V1（Win8.1 的 `SetProcessDpiAwarenessContext` 版本）。
        if try_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
            let after = current();
            if after == Awareness::PerMonitor {
                return Declaration {
                    before,
                    after,
                    api: Api::PerMonitorV1,
                };
            }
        }

        // 3) 再次：shcore 的 `SetProcessDpiAwareness`（Win8.1；老 API 用 HRESULT 表达失败，
        //    "已经声明过"时返回 E_ACCESSDENIED，所以 `>= 0` 才算成功）。
        // SAFETY: 改本进程的档位；调用点保证在任何窗口创建之前。
        if unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) } >= 0 {
            let after = current();
            if matches!(after, Awareness::PerMonitor | Awareness::PerMonitorV2) {
                return Declaration {
                    before,
                    after,
                    api: Api::PerMonitorV1,
                };
            }
        }

        // 4) 最后：Vista+ 的 `SetProcessDPIAware()`，只能到 System-Aware —— 但至少让坐标不再
        //    被按 96 虚拟化，比继续 Unaware 好。
        // SAFETY: 改本进程的档位；调用点保证在任何窗口创建之前。
        if unsafe { SetProcessDPIAware() } != 0 {
            let after = current();
            if after == Awareness::SystemAware {
                return Declaration {
                    before,
                    after,
                    api: Api::SystemAware,
                };
            }
        }

        Declaration {
            before,
            after: current(),
            api: Api::Failed,
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{Api, Awareness, declare, probe, system_dpi};

    /// 声明之后必须真的读回 Per-Monitor V2，并且**幂等**（第二次是 `AlreadyDeclared`）。
    ///
    /// 这条测试就是"壳里的注入点有效"的证据：`cargo test -p hbot-desktop` 跑过它，
    /// 说明这个进程能被切到 PMv2。**变异验证**：把 `declare()` 里的
    /// `SetProcessDpiAwarenessContext` 调用删掉（或挪到窗口创建之后），这条测试会红。
    #[test]
    fn declare_reaches_per_monitor_v2_and_is_idempotent() {
        let first = declare();
        assert!(
            first.is_per_monitor_v2(),
            "声明后读回的档位必须是 Per-Monitor V2，实际 {:?}（api = {:?}）",
            first.after,
            first.api
        );
        assert!(
            super::current() == Awareness::PerMonitorV2,
            "GetThreadDpiAwarenessContext 必须确认 PMv2"
        );

        let second = declare();
        assert_eq!(
            second.api,
            Api::AlreadyDeclared,
            "第二次声明必须直接返回 already-declared（幂等），实际 {:?}",
            second.api
        );
        assert!(second.is_per_monitor_v2());
    }

    /// 进程 DPI 必须 ≥ 96（拿不到时按 96 兜底，见 `system_dpi`）。
    #[test]
    fn system_dpi_is_at_least_96() {
        assert!(system_dpi() >= 96, "GetDpiForSystem 返回 {}", system_dpi());
    }

    /// `--dpi-probe` 在已声明 PMv2 的进程里必须返回 0（脚本消费这个退出码）。
    #[test]
    fn probe_succeeds_once_declared() {
        let _ = declare();
        assert_eq!(probe(), 0, "PMv2 已生效时 --dpi-probe 必须返回 0");
    }
}
