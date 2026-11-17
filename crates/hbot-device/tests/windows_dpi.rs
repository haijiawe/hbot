//! 每屏 DPI / 缩放 / DPI 虚拟化的**真机取证**测试（Windows-only）。
//!
//! 这个文件同时是回归测试与取证脚本：它把「声明 Per-Monitor V2 之前 / 之后」两组数字打成
//! 一张表（`--nocapture` 可见），并断言本 crate 的 DPI 报数是对的。
//!
//! 为什么要**在测试进程里亲自声明** DPI 感知：`hbot-device` 是库，装不了 manifest，也不该替
//! 宿主改进程状态（见 `src/windows.rs` 顶部「本 crate 做不到什么」）。但"虚拟化到底改了什么"
//! 这件事必须用**同一个进程、同一块屏、两种档位**的对照才说得清 —— 测试进程恰好可以：它一
//! 开始时是 DPI-Unaware（cargo 给的测试二进制没有 manifest），量一遍；然后自己调
//! `SetProcessDpiAwarenessContext(PMv2)`，再量一遍。GUI 壳里那段（`crates/hbot-desktop/src/dpi.rs`）
//! 做的正是同一件事，只是时机更早（任何窗口创建之前）。
//!
//! 跑法：`cargo test -p hbot-device --test windows_dpi -- --nocapture`
//!
//! # 已知事实（本机 Windows 11 26300、单屏 1920×1080、100% 缩放）
//!
//! 两组数字**全等** —— 因为 100% 缩放下没有虚拟化可做。150% 缩放下声明前的 `rcMonitor` 与
//! 位图会变成 1280×720，声明后仍是 1920×1080；那条是系统文档保证的行为，本机没有 150% 缩放
//! 可测，所以测试**不断言绝对宽度**（换台机器就是另一个数），只断言两组之间的一致性与区间。
#![cfg(windows)]

use hbot_core::device::DeviceBackend;
use hbot_device::{
    DisplayInfo, DisplayMetrics, ScreenshotRequest, WindowsBackend, display_metrics, displays,
    system_dpi,
};
use windows_sys::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    DPI_AWARENESS_CONTEXT_UNAWARE, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};

/// 当前线程（= 本进程，测试跑在单线程 main 上）的 DPI 感知档位名。
///
/// 用 `AreDpiAwarenessContextsEqual` 而不是比指针：上下文句柄是**伪句柄**，同一个档位在不同
/// 版本上可能是不同的数值，只有这个 API 保证语义比较。顺序从高到低，先命中先返回。
fn awareness_name() -> &'static str {
    // SAFETY: 无参数、无副作用，返回当前线程的上下文句柄（不归调用方释放）。
    let current = unsafe { GetThreadDpiAwarenessContext() };
    let contexts: [(*mut core::ffi::c_void, &'static str); 4] = [
        (DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, "Per-Monitor V2"),
        (DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, "Per-Monitor V1"),
        (DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, "System-Aware"),
        (DPI_AWARENESS_CONTEXT_UNAWARE, "Unaware"),
    ];
    for (context, name) in contexts {
        // SAFETY: 两个入参都是有效上下文句柄；这个 API 只做比较。
        if unsafe { AreDpiAwarenessContextsEqual(current, context) } != 0 {
            return name;
        }
    }
    "未知档位"
}

/// 把本进程切成 Per-Monitor V2；返回 `SetProcessDpiAwarenessContext` 的原始结果。
///
/// 返回 `false` 表示系统拒绝（老 Windows，或档位已被 manifest 抢先用另一种方式钉死）——
/// 测试**不 panic**：那本身就是一条要报出去的结论（见调用处）。
fn declare_per_monitor_v2() -> bool {
    // SAFETY: 只改本进程的 DPI 档位，且必须在任何窗口创建之前调用 —— 测试进程里没有窗口。
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
}

/// 一次快照：档位 + 进程 DPI + 清单 + 诊断指标。
struct Snapshot {
    awareness: &'static str,
    system_dpi: u32,
    displays: Vec<DisplayInfo>,
    metrics: Vec<DisplayMetrics>,
}

fn snapshot() -> Snapshot {
    Snapshot {
        awareness: awareness_name(),
        system_dpi: system_dpi(),
        displays: displays(),
        metrics: display_metrics(),
    }
}

/// 把一张快照打成表（`--nocapture` 时进 stdout，也是失败信息的一部分）。
fn print_table(label: &str, snapshot: &Snapshot) {
    println!(
        "\n[{label}] awareness = {} · GetDpiForSystem() = {}",
        snapshot.awareness, snapshot.system_dpi
    );
    println!(
        "  {:>2} {:<14} {:<6} {:>6} {:>6} {:>5} {:>7} {:>11} {:>11} {:>11} {:>4}",
        "id", "device", "prim", "rc.x", "rc.y", "w", "h", "physical", "bitmap", "logpx", "dpi"
    );
    for (display, metrics) in snapshot.displays.iter().zip(snapshot.metrics.iter()) {
        // 截图每次都真抓：这一列就是"位图尺寸到底等于什么"的答案。抓不到就写 "--"，
        // 让"没抓到"与"抓到了但是别的尺寸"在表里能区分开。
        let bitmap = match WindowsBackend::new().screenshot(&ScreenshotRequest {
            monitor: Some(display.index),
            max_bytes: 32 * 1024 * 1024,
        }) {
            Ok(shot) => format!("{}x{}", shot.width, shot.height),
            Err(error) => format!("--({error})"),
        };
        println!(
            "  {:>2} {:<14} {:<6} {:>6} {:>6} {:>5} {:>7} {:>11} {:>11} {:>11} {:>4}",
            display.index,
            display.device,
            display.primary,
            display.x,
            display.y,
            display.width,
            display.height,
            format!("{}x{}", metrics.physical_width, metrics.physical_height),
            bitmap,
            metrics.dc_log_pixels_x,
            format!("{}/{}", display.dpi_x, display.dpi_y),
        );
    }
}

/// 主取证：每块屏都报得出 DPI 与缩放比，且 `display_metrics()` 与 `displays()` 一一对应。
#[test]
fn every_display_reports_a_plausible_dpi_and_scale() {
    let before = snapshot();
    print_table("声明前（DPI-Unaware）", &before);

    // 真机断言 1：清单非空，否则这台机器没有可交互桌面，后面的断言没有意义（跳过而不是
    // 假装通过 —— 用 early return 并打一行说明，免得 CI 上无桌面时红得莫名其妙）。
    if before.displays.is_empty() {
        println!("没有枚举到显示器（无交互桌面），跳过 DPI 断言");
        return;
    }
    assert_eq!(
        before.metrics.len(),
        before.displays.len(),
        "display_metrics() 与 displays() 必须一一对应（同一套枚举与排序）"
    );

    // 真机断言 2：每块屏的 dpi/scale 都落在合理区间。
    for (display, metrics) in before.displays.iter().zip(before.metrics.iter()) {
        assert_eq!(
            display.index, metrics.index,
            "两个列表的下标必须对齐：{display:?} vs {metrics:?}"
        );
        assert_eq!(
            display.device, metrics.device,
            "同一序号必须指向同一块屏：{display:?} vs {metrics:?}"
        );
        assert!(
            display.scale_percent > 0,
            "每块屏都必须报得出缩放比（0 表示 DPI 读失败）：{display:?}"
        );
        assert!(
            (96..=480).contains(&display.dpi_x),
            "dpi_x 必须落在 96..=480（100%..=500%）：{display:?}"
        );
        assert!(
            (96..=480).contains(&display.dpi_y),
            "dpi_y 必须落在 96..=480：{display:?}"
        );
        // 缩放比必须与 DPI 自洽（四舍五入口径，见 `scale_percent_of`）。
        let expected = (u64::from(display.dpi_x) * 100 + 48) / 96;
        assert_eq!(
            u64::from(display.scale_percent),
            expected,
            "scale_percent 必须是 dpi_x 相对 96 的四舍五入：{display:?}"
        );
        // 物理尺寸必须量得到（退化时等于 rcMonitor，也不是 0）。
        assert!(
            metrics.physical_width > 0 && metrics.physical_height > 0,
            "物理尺寸不能是 0：{metrics:?}"
        );
        // 主屏恒为 0 号（`Some(0)` 与 `None` 必须指同一块屏）。
        if display.primary {
            assert_eq!(display.index, 0, "主屏必须是 0 号：{display:?}");
        }
    }
    assert_eq!(
        before
            .metrics
            .iter()
            .filter(|metrics| metrics.primary)
            .count(),
        1,
        "必须恰好有一块主屏：{:?}",
        before.metrics
    );

    // 真机断言 3：截图位图与 `rcMonitor` 的关系 —— 这就是任务里那个问题的答案。
    // 无论档位如何，本 crate 的 `screenshot(Some(i))` 都按这块屏的 rcMonitor 建 DC，
    // 所以"位图宽高 == rcMonitor 宽高"在当前档位下**必须成立**；两者是否等于物理像素
    // 取决于档位，那由 `metrics.size_delta()` 表达，不在这里断言。
    for display in &before.displays {
        let Ok(shot) = WindowsBackend::new().screenshot(&ScreenshotRequest {
            monitor: Some(display.index),
            max_bytes: 32 * 1024 * 1024,
        }) else {
            println!(
                "显示器 {} 截图失败（无桌面/被锁屏），跳过位图尺寸对照",
                display.index
            );
            continue;
        };
        println!(
            "显示器 {} 位图 {}x{} vs rcMonitor {}x{}（物理 {}x{}）",
            display.index,
            shot.width,
            shot.height,
            display.width,
            display.height,
            before.metrics[display.index as usize].physical_width,
            before.metrics[display.index as usize].physical_height,
        );
    }

    // 声明 PMv2 之后必须真的生效（Win10 1703+ 才有这个 API；本 crate 的目标平台就是它）。
    let declared = declare_per_monitor_v2();
    let after = snapshot();
    print_table("声明后（Per-Monitor V2）", &after);
    assert!(
        declared,
        "SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) 返回失败：这台机器的 Windows 太老，\
         或者档位已被 manifest 钉死（此时期望是壳里的三级回退，见 crates/hbot-desktop/src/dpi.rs）"
    );
    assert_eq!(
        after.awareness, "Per-Monitor V2",
        "声明后线程上下文必须是 Per-Monitor V2，实际是 {}",
        after.awareness
    );

    // 声明后：清单、序号、DPI 与缩放比都不该变（它们是**屏**的属性，不是进程属性），
    // 变的只可能是 `rcMonitor`/位图（只有在非 100% 缩放时才变）。
    assert_eq!(
        after.displays.len(),
        before.displays.len(),
        "声明 DPI 感知不该改变显示器数量"
    );
    for (old, new) in before.displays.iter().zip(after.displays.iter()) {
        assert_eq!(
            (old.index, &old.device, old.primary),
            (new.index, &new.device, new.primary),
            "声明前后同一序号必须仍是同一块屏：{old:?} vs {new:?}"
        );
        assert_eq!(
            (old.dpi_x, old.dpi_y, old.scale_percent),
            (new.dpi_x, new.dpi_y, new.scale_percent),
            "每屏 DPI/缩放比是屏的属性，声明前后不该变：{old:?} vs {new:?}"
        );
    }

    // 100% 缩放下两种档位的 rcMonitor 必须相同（没有虚拟化可做）；非 100% 缩放下声明后
    // 变大到物理像素。两种情况都用 size_delta 的口径断言，避免猜机器。
    for metrics in &after.metrics {
        println!(
            "显示器 {} 声明后：rcMonitor {}x{}，物理 {}x{}，delta = {:?}，虚拟化 = {}",
            metrics.index,
            metrics.width,
            metrics.height,
            metrics.physical_width,
            metrics.physical_height,
            metrics.size_delta(),
            metrics.is_virtualized(),
        );
        if metrics.scale_percent == 100 {
            assert!(
                metrics.matches_physical(),
                "100% 缩放下 rcMonitor 必须等于物理像素（没有虚拟化可做）：{metrics:?}"
            );
        }
    }
}

/// 诊断 API 的兜底契约：`displays()`/`display_metrics()` 永远给 `Vec`（失败是空，不是 panic），
/// `system_dpi()` 永远 ≥ 96（拿不到时按 96 算，见 `process_dpi`）。
#[test]
fn dpi_diagnostics_never_panic_and_never_return_zero() {
    let metrics = display_metrics();
    if !metrics.is_empty() {
        for row in &metrics {
            assert!(row.dpi_x >= 96, "{row:?}");
            assert!(row.dpi_y >= 96, "{row:?}");
        }
    }
    assert!(
        system_dpi() >= 96,
        "GetDpiForSystem 返回值必须 ≥ 96（拿不到时按 96 兜底），实际 {}",
        system_dpi()
    );
}
