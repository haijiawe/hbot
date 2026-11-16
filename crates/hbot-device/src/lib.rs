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
//! | Windows 10/11 | [`WindowsBackend`]：截图、注入、窗口枚举、剪贴板都实现了 |
//! | 其它 | [`UnsupportedBackend`]：`capabilities()` 全 `false`，每个方法返回 [`DeviceError::HostLimit`] |
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
//! - **只做主显示器**（`monitor` 只接受 `0`/`None`）：其它索引返回 `HostLimit`。虚拟桌面的负坐标
//!   （主屏左边的副屏）在 `click` 的坐标归一化里是对的，但截图拿不到那些区域。
//! - **DPI**：本 crate 是库，**没法**给宿主进程装 Per-Monitor V2 manifest。宿主没声明 DPI 感知时，
//!   系统会把窗口矩形与像素虚拟化（150% 缩放下一张 1920 宽的屏可能只有 1280 宽），坐标也会错位。
//!   这条必须在 `hbot-desktop` 的壳里解决（`docs/09` §6.4），不是 backend 能修的。
//! - **没有窗口级截图入口**：trait 只有 `monitor` 参数，没有 `window_id`。想"截某个窗口"要先用
//!   [`windows()`](DeviceBackend::windows) 找到它、切到前台——这正是"窗口级截图未做"的含义。
//! - **没有 OCR**：`DeviceCapability::Ocr` 在能力报告里恒为不支持；OCR 属于另一层（`docs/09` §9.1）。
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

// 调用方只需要认识这两个 crate 的类型，不必再手写 `hbot_core::device::…` 前缀。
pub use hbot_core::device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapability, DeviceCapabilityReport,
    DeviceError, InputAction, MouseButton, Screenshot, ScreenshotRequest, WindowInfo,
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
        // `Ocr` 不在窄版范围里，任何平台都不该报告为支持。
        let report = boxed.capabilities();
        assert!(report.detail.is_some(), "能力报告要说明边界");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_unsupported_backend_refuses_everything_with_a_host_limit() {
        let backend = UnsupportedBackend::new();
        let report = backend.capabilities();
        assert!(
            !report.screen && !report.input && !report.window && !report.clipboard,
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
