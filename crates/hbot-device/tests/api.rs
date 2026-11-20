//! 平台无关的公开 API 测试：**从 crate 外面**验证"契约能被对象化、公开工具函数真的可用"。
//!
//! 这些测试在 Linux/macOS 上也会跑（`cargo test -p hbot-device`），所以它们只能碰纯函数与
//! 能力报告，不能碰截图/注入/剪贴板 —— 那些在 `tests/windows_desktop.rs`。

use hbot_device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapabilityReport, DeviceError, DeviceTarget,
    InputAction, MouseButton, OcrRequest, Screenshot, ScreenshotRequest, WindowInfo, enforce_limit,
    parse_combo, platform_backend, png, process_name,
};

#[test]
fn the_backend_is_usable_as_a_trait_object() {
    // `DeviceBackend` 是 `Send + Sync` 的 trait，组合根会把它塞进 `Arc<dyn DeviceBackend>`
    // 跨线程共享。这里钉住"可以对象化"这条性质（加一个泛型方法就会破坏它）。
    let backend: Box<dyn DeviceBackend> = Box::new(platform_backend());
    let report: DeviceCapabilityReport = backend.capabilities();
    // 能力内容是**平台相关**的，这条测试钉的是「能对象化、能跨线程共享」。
    // Windows 上 `window`/`clipboard` 恒真；其它宿主的 `platform_backend()` 是
    // `UnsupportedBackend`（六个能力全 `false` + `detail` 写明原因，见 `src/unsupported.rs`）。
    // 曾把 Windows 的能力当成跨平台契约写死断言，CI 的 ubuntu 立刻红 —— 别再把
    // 某一平台的实现细节当成公开 API 的不变量。
    if cfg!(windows) {
        // `detail` 在 Windows 上是**能力说明**（显示器清单、OCR 语言包、
        // PrintWindow 的限制…），不是错误原因，所以这里只钉能力位。
        assert!(report.window, "Windows 后端必须报告窗口枚举");
        assert!(report.clipboard, "Windows 后端必须报告剪贴板");
    } else {
        assert!(
            !report.window && !report.clipboard,
            "非 Windows 宿主没有设备实现，能力必须全 false：{report:?}"
        );
        assert!(
            report.detail.is_some(),
            "能力全 false 时必须给出原因，否则用户分不清「没实现」和「配置关了」"
        );
    }
    // 通过 trait 对象调用，钉住方法签名没被改成泛型/关联类型。
    let _windows: Result<Vec<WindowInfo>, DeviceError> = backend.windows();
    let _clipboard: Result<ClipboardValue, DeviceError> = backend.clipboard(&ClipboardOp::Read);
    // `dyn DeviceBackend` 必须能跨线程共享（组合根会把它塞进 `Arc`）。
    fn assert_shared(value: Box<dyn DeviceBackend>) {
        let shared: std::sync::Arc<dyn DeviceBackend> = std::sync::Arc::from(value);
        std::thread::spawn(move || {
            let _ = shared.capabilities();
        })
        .join()
        .expect("另一线程不该 panic");
    }
    assert_shared(Box::new(platform_backend()));
}

#[test]
fn the_png_encoder_is_reachable_from_outside_the_crate() {
    // 2×2：红、绿、蓝、白。
    let pixels = [
        0xFF, 0x00, 0x00, // 红
        0x00, 0xFF, 0x00, // 绿
        0x00, 0x00, 0xFF, // 蓝
        0xFF, 0xFF, 0xFF, // 白
    ];
    let encoded = png::encode_rgb(2, 2, &pixels).expect("2×2 应当能编码");

    // PNG 签名。
    assert_eq!(
        &encoded[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "PNG 签名不对"
    );
    // IHDR 紧跟签名（偏移 8 是长度、12 是 "IHDR"、16 起是宽高，都是大端）。
    assert_eq!(&encoded[12..16], b"IHDR");
    let width = u32::from_be_bytes([encoded[16], encoded[17], encoded[18], encoded[19]]);
    let height = u32::from_be_bytes([encoded[20], encoded[21], encoded[22], encoded[23]]);
    assert_eq!((width, height), (2, 2), "IHDR 里的尺寸不对");
    assert_eq!(&encoded[encoded.len() - 8..encoded.len() - 4], b"IEND");
    // 像素长度不对要失败，而不是 panic 或产出坏图。
    assert!(matches!(
        png::encode_rgb(2, 2, &pixels[..8]),
        Err(DeviceError::Failed { .. })
    ));
}

#[test]
fn combo_parsing_accepts_the_documented_spellings_from_outside_the_crate() {
    for spelling in [vec!["Ctrl+C"], vec!["ctrl+c"], vec!["ctrl", "c"]] {
        let keys: Vec<String> = spelling.iter().map(|item| (*item).to_string()).collect();
        let combo = parse_combo(&keys).expect("这三种写法都该被接受");
        assert_eq!(combo.key, hbot_device::ComboKey::Char('c'));
        assert_eq!(combo.modifiers, vec![hbot_device::ComboModifier::Ctrl]);
    }
    assert!(parse_combo(&["F5".to_string()]).is_ok());
    assert!(parse_combo(&["enter".to_string()]).is_ok());
    let error = parse_combo(&["hyper+c".to_string()]).expect_err("hyper 不支持");
    assert_eq!(error.code(), "HBOT_DEVICE_FAILED");
    assert!(
        error.to_string().contains("ctrl/alt/shift/win"),
        "错误信息要列出支持集合：{error}"
    );
}

#[test]
fn process_name_and_the_allow_list_check_are_public_and_case_insensitive() {
    assert_eq!(
        process_name(r"C:\Program Files\Some Game\Game.EXE"),
        "game.exe"
    );
    let allowed = vec![r"C:\Games\PathOfExile.exe".to_string()];
    assert!(hbot_device::app_is_allowed(&allowed, "PATHOFEXILE.EXE"));
    assert!(!hbot_device::app_is_allowed(&[], "pathofexile.exe"));
}

#[test]
fn the_byte_limit_is_enforced_without_truncating() {
    let payload = vec![0xABu8; 100];
    assert_eq!(enforce_limit(payload.clone(), 100), Ok(payload.clone()));
    assert_eq!(
        enforce_limit(payload, 99),
        Err(DeviceError::TooLarge { limit_bytes: 99 }),
        "超限要报 TooLarge，不许截断"
    );
}

#[test]
fn an_oversized_screenshot_result_is_rejected_rather_than_shortened() {
    // 需求里的第 ⑥ 条：用一个假的 `Screenshot` 走 `enforce_limit`，不真截屏。
    let fake_png = vec![0u8; 4096];
    let result = enforce_limit(fake_png, 1024);
    assert!(
        matches!(result, Err(DeviceError::TooLarge { limit_bytes: 1024 })),
        "超过 max_bytes 必须拒绝"
    );
    // 正好等于上限是允许的（边界包含）。
    let at_limit = vec![0u8; 1024];
    assert!(enforce_limit(at_limit, 1024).is_ok());
}

#[test]
fn the_contract_types_are_re_exported_so_callers_need_only_one_crate() {
    // 编译期性质：这些名字都能从 `hbot_device` 直接拿到。
    let _request = ScreenshotRequest {
        monitor: Some(0),
        max_bytes: 8 * 1024 * 1024,
    };
    let _action = InputAction::Click {
        x: 0,
        y: 0,
        button: MouseButton::Left,
    };
    let _value = ClipboardValue {
        text: String::new(),
    };
    let _screenshot = Screenshot {
        width: 1,
        height: 1,
        png: Vec::new(),
        monitor: 0,
    };
    let _op = ClipboardOp::Read;
}
#[test]
fn an_unsupported_host_refuses_every_device_call_with_a_host_limit() {
    // `docs/07` §10 的 V10：「能力受限时返回 `HOST_LIMIT` 而非静默降级」。
    // 桌面、但没有设备实现的宿主（Linux/macOS）就是这条判据最真实的样本 ——
    // 它必须在**每一个**入口上说「不」，而不是 panic、不是返回空结果、
    // 更不是截一张全黑的图让人以为成功了。
    if cfg!(windows) {
        // Windows 有真实现，这条判据在那边由 `tests/windows_desktop.rs` 的
        // 「不存在的显示器序号 / 无交互桌面」两组用例覆盖。
        return;
    }
    let backend = platform_backend();
    let detail = "这个宿主没有设备能力实现";
    let expect_refusal = |what: &str, error: DeviceError| match error {
        DeviceError::HostLimit { detail: got } => assert_eq!(
            got, detail,
            "{what} 的拒绝文案要与 capabilities().detail 同源，否则两处会漂移"
        ),
        other => panic!("{what} 必须是 HostLimit，实际 {other:?}"),
    };
    expect_refusal(
        "screenshot",
        backend
            .screenshot(&ScreenshotRequest {
                monitor: None,
                max_bytes: 8 * 1024 * 1024,
            })
            .expect_err("无设备实现的宿主必须拒绝截屏"),
    );
    expect_refusal(
        "ocr",
        backend
            .ocr(&OcrRequest {
                target: DeviceTarget::None,
                max_chars: 200,
            })
            .expect_err("无设备实现的宿主必须拒绝 OCR"),
    );
    expect_refusal(
        "input",
        backend
            .input(
                &InputAction::Click {
                    x: 0,
                    y: 0,
                    button: MouseButton::Left,
                },
                &[],
            )
            .expect_err("无设备实现的宿主必须拒绝注入"),
    );
    expect_refusal(
        "windows",
        backend
            .windows()
            .expect_err("无设备实现的宿主必须拒绝窗口枚举"),
    );
    expect_refusal(
        "clipboard",
        backend
            .clipboard(&ClipboardOp::Read)
            .expect_err("无设备实现的宿主必须拒绝剪贴板"),
    );
    // 三个非 `Result` 的诊断入口：给空值而不是编一个「像真的」的数字。
    assert!(
        hbot_device::displays().is_empty(),
        "无设备实现的宿主不该报出显示器"
    );
    assert_eq!(
        hbot_device::system_dpi(),
        0,
        "无 DPI 概念的宿主返回 0，不要当 96 用"
    );
}
