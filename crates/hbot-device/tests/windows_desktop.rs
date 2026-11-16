//! Windows 真机测试：**只验证"不打扰用户"的那些性质**。
//!
//! CI 里绝不移动鼠标、绝不截整屏 —— 那会让正在用机器的人莫名其妙。所以这里只做：
//!
//! - 能力报告（只 `GetDC` 一下、查一次输入桌面名）；
//! - 窗口枚举（只读）；
//! - 空白名单拒绝（在碰 `SendInput` 之前就返回了）；
//! - 剪贴板**读**（空剪贴板也是合法的成功）；
//! - 指定了未实现的显示器索引 → 明确 `HostLimit`（连截图都不开始）。
//!
//! 真截屏、真写剪贴板的两条被 `#[ignore]` 关掉，手动跑：
//! `cargo test -p hbot-device -- --ignored`。

#![cfg(windows)]

use hbot_device::{
    ClipboardOp, DeviceBackend, DeviceError, InputAction, MouseButton, ScreenshotRequest,
    WindowsBackend, process_name,
};

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
        "能力报告要写明 OCR 未实现：{detail}"
    );
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
    let own = std::env::current_exe()
        .ok()
        .map(|path| process_name(&path.to_string_lossy()))
        .unwrap_or_default();
    if !own.is_empty() {
        assert!(
            windows.iter().all(|window| window.process != own),
            "自己的进程（{own}）不该出现在可操作窗口列表里"
        );
    }
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
fn an_unimplemented_monitor_index_is_a_host_limit_not_a_silent_primary_capture() {
    let error = WindowsBackend::new()
        .screenshot(&ScreenshotRequest {
            monitor: Some(1),
            max_bytes: 8 * 1024 * 1024,
        })
        .expect_err("显示器 1 未实现，必须拒绝");
    match error {
        DeviceError::HostLimit { detail } => {
            assert!(
                detail.contains("monitor 0"),
                "错误信息要说明只实现了 monitor 0：{detail}"
            );
        }
        other => panic!("应当是 HostLimit，实际 {other:?}"),
    }
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

#[test]
#[ignore = "需要真实桌面会话"]
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
    assert_eq!(
        &shot.png[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "输出必须是 PNG"
    );
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
