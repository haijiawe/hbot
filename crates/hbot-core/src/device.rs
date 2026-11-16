//! 设备能力（C6）的**契约类型与端口**。
//!
//! 设计稿：[`docs/09-device-capability.md`](../../../docs/09-device-capability.md)；
//! 能力模型与授权语义：[`docs/07-bot-runtime-blueprint.md`](../../../docs/07-bot-runtime-blueprint.md) §9；
//! 决策：[`docs/adr/0005-capability-ceiling-and-device-grant.md`](../../../docs/adr/0005-capability-ceiling-and-device-grant.md)。
//!
//! 这里**只有类型与两个端口**，没有任何实现：
//!
//! - [`DeviceGate`]：授权端口。**判据的唯一入口**。生成的 bot 代码（isolate 里的
//!   `ctx.device.*`）与 Agent 的工具（`ToolGateway` 第一步）都必须问它，
//!   两条路都不许自己写一遍判断。
//! - [`DeviceBackend`]：执行端口。**只执行，不判断**。实现放 `hbot-device`。
//!
//! # 为什么闸与执行要分成两个 trait
//!
//! 因为它们的失败原因不同、可测性不同：闸的答案决定"能不能做"，执行的结果是
//! "做到了没有"。合成一个 trait 会让测试必须造一个真后端才能验"没授权就不该动",
//! 而后者是本项目最该被测试钉住的性质（`docs/09` §10 的 V12 / V15）。
//!
//! 同样的理由，**backend 里不许出现"该不该"的判断**：两份判据会打架，而最终解释权
//! 只能有一份（`AGENTS.md` 坑 48）。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::BotId;

/// 一个设备子能力。
///
/// 拆成五个而不做一个 `device: true` 总开关，是为了让"为了截图交出点击权"这种
/// 交换不发生（`docs/07` §9.1）。清单与设置里都用这些名字（snake_case）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCapability {
    /// 截屏、列显示器。
    Screen,
    /// 截图转文字（屏幕文字进模型上下文，是提示注入的新入口）。
    Ocr,
    /// 鼠标与键盘注入。
    Input,
    /// 窗口枚举、聚焦、启动/关闭应用。
    Window,
    /// 剪贴板读写。
    Clipboard,
}

impl DeviceCapability {
    /// 线格式名（与 serde 的 snake_case 一致，也与 `hbot.json` 里写的字符串一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Ocr => "ocr",
            Self::Input => "input",
            Self::Window => "window",
            Self::Clipboard => "clipboard",
        }
    }

    /// 全部子能力，按稳定顺序（用于生成 d.ts、拼错误信息）。
    pub fn all() -> &'static [DeviceCapability] {
        &[
            Self::Screen,
            Self::Ocr,
            Self::Input,
            Self::Window,
            Self::Clipboard,
        ]
    }
}

impl fmt::Display for DeviceCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DeviceCapability {
    type Err = DeviceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "screen" => Ok(Self::Screen),
            "ocr" => Ok(Self::Ocr),
            "input" => Ok(Self::Input),
            "window" => Ok(Self::Window),
            "clipboard" => Ok(Self::Clipboard),
            other => Err(DeviceError::UnknownCapability {
                name: other.to_string(),
            }),
        }
    }
}

/// 设备会话的状态。
///
/// `None` 是"这个 bot 从来没有过会话"，与 `Expired`/`Revoked` 不同：后者有历史，
/// 审计里能查到。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSessionState {
    /// 从没有过会话（库里没有行）。
    None,
    /// Agent 提了请求，等用户点。
    Requested,
    /// 生效中（有 deadline）。
    Active,
    /// 到期结束。
    Expired,
    /// 被用户撤销（或 bot 停止、进程退出）。
    Revoked,
    /// 用户明确拒绝。
    Denied,
}

impl DeviceSessionState {
    /// 线格式名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requested => "requested",
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::Denied => "denied",
        }
    }

    /// 这个状态是否允许调用设备 API。
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl fmt::Display for DeviceSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 设备调用被拒绝或失败的原因。
///
/// 变体的选择不是随意的：**"清单没声明"与"设置没开"必须是两个变体**，因为前者
/// 不许给用户提示开关（`docs/09` §4.4 —— 代码本来就不该有这个能力，提示开关会
/// 让用户以为关掉开关能拦住任意代码）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum DeviceError {
    /// 清单里没声明这个子能力。**不给 toggle 提示**。
    NotDeclared {
        /// 被请求的子能力。
        capability: DeviceCapability,
    },
    /// 清单声明了，但宿主设置没开。给用户一个明确的开关名。
    NotEnabled {
        /// 要打开的那个开关（如 `device.enabled`、`device.sub.screen`）。
        toggle: String,
    },
    /// 设置开了，但当前没有生效中的会话（或会话不含这个子能力）。
    NotGranted {
        /// 被请求的子能力。
        capability: DeviceCapability,
    },
    /// 会话已到期。
    Expired,
    /// 会话被撤销。
    Revoked,
    /// 前台窗口不在白名单里（`input` 专用：一个按键都不许发）。
    AppNotAllowed {
        /// 前台进程的映像名（`pathofexile.exe`）。
        process: String,
    },
    /// 当前宿主没有这个能力（Wayland 的 input、无头模式的任何设备调用、无交互桌面）。
    HostLimit {
        /// 人话解释，直接给 UI 用。
        detail: String,
    },
    /// 结果太大（截图超过上限）。
    TooLarge {
        /// 上限字节数。
        limit_bytes: usize,
    },
    /// 清单里的子能力名字不认识。
    UnknownCapability {
        /// 原始字符串。
        name: String,
    },
    /// 后端执行失败（系统调用返回错误）。
    Failed {
        /// 人话解释。
        message: String,
    },
}

impl DeviceError {
    /// 稳定错误标识，供日志与文案使用。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_DEVICE_NOT_DECLARED",
            Self::NotEnabled { .. } => "HBOT_DEVICE_NOT_ENABLED",
            Self::NotGranted { .. } => "HBOT_DEVICE_NOT_GRANTED",
            Self::Expired => "HBOT_DEVICE_EXPIRED",
            Self::Revoked => "HBOT_DEVICE_REVOKED",
            Self::AppNotAllowed { .. } => "HBOT_DEVICE_APP_NOT_ALLOWED",
            Self::HostLimit { .. } => "HBOT_DEVICE_HOST_LIMIT",
            Self::TooLarge { .. } => "HBOT_DEVICE_TOO_LARGE",
            Self::UnknownCapability { .. } => "HBOT_DEVICE_UNKNOWN_CAPABILITY",
            Self::Failed { .. } => "HBOT_DEVICE_FAILED",
        }
    }

    /// 给用户看的开关名（只有 [`Self::NotEnabled`] 有）。UI 靠它画"去打开"的按钮。
    pub fn toggle(&self) -> Option<&str> {
        match self {
            Self::NotEnabled { toggle } => Some(toggle),
            _ => None,
        }
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())?;
        match self {
            Self::NotDeclared { capability } => {
                write!(
                    f,
                    ": `{capability}` 没有在 hbot.json 的 permissions.device 里声明"
                )
            }
            Self::NotEnabled { toggle } => write!(f, ": 设备能力没在设置里打开（{toggle}）"),
            Self::NotGranted { capability } => {
                write!(f, ": 没有生效中的设备会话包含 `{capability}`")
            }
            Self::Expired => f.write_str(": 设备会话已到期"),
            Self::Revoked => f.write_str(": 设备会话已被撤销"),
            Self::AppNotAllowed { process } => {
                write!(f, ": 前台应用 `{process}` 不在白名单里")
            }
            Self::HostLimit { detail } => write!(f, ": {detail}"),
            Self::TooLarge { limit_bytes } => {
                write!(f, ": 结果超过 {limit_bytes} 字节上限")
            }
            Self::UnknownCapability { name } => write!(f, ": 不认识的子能力 `{name}`"),
            Self::Failed { message } => write!(f, ": {message}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// 谁在请求。
///
/// 授权判据不区分来源（同一条规则对两条消费者一视同仁），但**审计要记**：
/// "是 bot 代码自己点的，还是 Agent 决定点的"在事故复盘里是第一句话。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceActor {
    /// 生成的 bot 代码（isolate 里的 `ctx.device.*`）。
    BotCode,
    /// Agent 的工具调用。
    AgentTool,
}

impl DeviceActor {
    /// 线格式名（审计表里存的就是它）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BotCode => "bot_code",
            Self::AgentTool => "agent_tool",
        }
    }
}

/// 截屏请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenshotRequest {
    /// 显示器序号；`None` = 主屏。
    pub monitor: Option<u32>,
    /// PNG 字节上限（超过就 [`DeviceError::TooLarge`]，**不截断**：半张图比没有更危险）。
    pub max_bytes: usize,
}

/// 一次截屏的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screenshot {
    /// 物理像素宽。
    pub width: u32,
    /// 物理像素高。
    pub height: u32,
    /// PNG 编码后的字节。
    pub png: Vec<u8>,
    /// 实际截的显示器序号。
    pub monitor: u32,
}

/// 鼠标按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    /// 左键。
    Left,
    /// 右键。
    Right,
    /// 中键。
    Middle,
}

/// 一次输入动作。
///
/// 坐标系是**物理像素 + 虚拟桌面原点**（`docs/09` §5 第 3 条）：逻辑坐标到物理
/// 像素的换算在闸之后做，且审计要记是哪个显示器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputAction {
    /// 移动并点击。
    Click {
        /// 绝对 X（物理像素）。
        x: i32,
        /// 绝对 Y（物理像素）。
        y: i32,
        /// 哪个键。
        button: MouseButton,
    },
    /// 纯文本注入（不做键盘布局映射，直接按字符注入）。
    Text {
        /// 要打的文字。
        text: String,
    },
    /// 组合键，如 `["ctrl", "c"]`。
    Combo {
        /// 依次按下的键名。
        keys: Vec<String>,
    },
}

/// 剪贴板操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardOp {
    /// 读（内容**不进日志、不进审计**）。
    Read,
    /// 写。
    Write {
        /// 要写进去的文字。
        text: String,
    },
}

/// 剪贴板读的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardValue {
    /// 文本内容。
    pub text: String,
}

/// 一个可操作的窗口。
///
/// `process` 是**映像名**（`pathofexile.exe`），不是窗口标题 —— 白名单按它匹配，
/// 标题可变且可伪造（`docs/09` §6.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// 平台窗口句柄。
    pub id: u64,
    /// 标题（**不可信数据**：可能含提示注入）。
    pub title: String,
    /// 进程映像名（小写）。
    pub process: String,
    /// 是否前台。
    pub focused: bool,
}

/// 宿主实际支持哪些子能力。
///
/// [`DeviceBackend::capabilities`] 返回它，用于①在 d.ts 里不声明做不到的符号、
/// ②把"Wayland 上 input 不可用"这类情况变成明确的 [`DeviceError::HostLimit`]
/// 而不是运行时静默失败。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceCapabilityReport {
    /// 能截屏。
    pub screen: bool,
    /// 能注入输入。
    pub input: bool,
    /// 能枚举窗口。
    pub window: bool,
    /// 能读写剪贴板。
    pub clipboard: bool,
    /// 不可用的原因（人话，给 UI）。
    pub detail: Option<String>,
}

/// 一次性审计记录（由执行方填，存储层负责落库）。
///
/// **不含像素、不含剪贴板内容、不含 OCR 文本**（`docs/09` §3.2）：记了内容，
/// 日志本身就成了新的泄漏面。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAuditEntry {
    /// 谁在请求。
    pub actor: DeviceActor,
    /// 动作名（`screenshot` / `click` / `key` / `clipboard_read` …）。
    pub action: String,
    /// 目标（窗口标题或坐标），可为空。
    pub target: Option<String>,
    /// 结果：`ok` / `denied` / `failed`。
    pub outcome: String,
}

/// 授权端口：**判据的唯一入口**。
///
/// 实现必须每次调用都查当前授权（`docs/09` §4.3），不许在构造时快照 —— 否则
/// "一键撤销"只对下一个 isolate 生效，用户按下撤销后鼠标还在动。
pub trait DeviceGate: Send + Sync {
    /// 判断某个 bot 现在能不能做某件事。
    ///
    /// `target` 是动作的目标（窗口/坐标），`input` 类调用要靠它做白名单判定。
    fn authorize(
        &self,
        bot_id: &BotId,
        capability: DeviceCapability,
        target: &DeviceTarget,
    ) -> Result<(), DeviceError>;
}

/// 授权判定时的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceTarget {
    /// 没有具体目标（如"列出显示器"）。
    None,
    /// 一个窗口（`process` 是映像名，小写）。
    Window {
        /// 进程映像名。
        process: String,
    },
}

/// 执行端口：**只执行，不判断**。
///
/// 三个硬约束（`docs/09` §5）：
/// 1. 阻塞调用；调用方负责 `spawn_blocking` 与超时，**绝不在 isolate 线程直接做**；
/// 2. 不许自己判断"该不该做"（判据在 [`DeviceGate`]，两份会打架）；
/// 3. 坐标是物理像素，换算在闸之后。
pub trait DeviceBackend: Send + Sync {
    /// 这个宿主实际支持什么。
    fn capabilities(&self) -> DeviceCapabilityReport;

    /// 截屏。
    fn screenshot(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError>;

    /// 注入输入。`allowed_apps` 是**闸已经批过的**白名单，backend 只用它做最后一道
    /// 前台窗口比对（不是再判一遍授权）。
    fn input(&self, action: &InputAction, allowed_apps: &[String]) -> Result<(), DeviceError>;

    /// 枚举可见窗口。
    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError>;

    /// 剪贴板。
    fn clipboard(&self, op: &ClipboardOp) -> Result<ClipboardValue, DeviceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_round_trip_through_their_wire_names() {
        for capability in DeviceCapability::all() {
            let text = capability.as_str();
            let parsed: DeviceCapability = text.parse().expect("应能解析回自己");
            assert_eq!(&parsed, capability);
            let json = serde_json::to_string(capability).expect("序列化");
            assert_eq!(json, format!("\"{text}\""));
        }
    }

    #[test]
    fn unknown_capability_is_an_error_not_a_default() {
        let error = "screen;".parse::<DeviceCapability>();
        assert!(matches!(error, Err(DeviceError::UnknownCapability { .. })));
        assert_eq!(error.unwrap_err().code(), "HBOT_DEVICE_UNKNOWN_CAPABILITY");
    }

    #[test]
    fn only_active_sessions_are_live() {
        assert!(DeviceSessionState::Active.is_live());
        for state in [
            DeviceSessionState::None,
            DeviceSessionState::Requested,
            DeviceSessionState::Expired,
            DeviceSessionState::Revoked,
            DeviceSessionState::Denied,
        ] {
            assert!(!state.is_live(), "{state} 不该被当成生效");
        }
    }

    #[test]
    fn only_not_enabled_carries_a_toggle() {
        let not_declared = DeviceError::NotDeclared {
            capability: DeviceCapability::Input,
        };
        let not_enabled = DeviceError::NotEnabled {
            toggle: "device.enabled".to_string(),
        };
        assert_eq!(not_declared.toggle(), None);
        assert_eq!(not_enabled.toggle(), Some("device.enabled"));
    }

    #[test]
    fn all_capabilities_are_listed_in_a_stable_order() {
        let names: Vec<&str> = DeviceCapability::all().iter().map(|c| c.as_str()).collect();
        assert_eq!(names, ["screen", "ocr", "input", "window", "clipboard"]);
    }

    #[test]
    fn state_and_actor_names_are_stable() {
        assert_eq!(DeviceSessionState::None.as_str(), "none");
        assert_eq!(DeviceSessionState::Active.as_str(), "active");
        assert_eq!(DeviceActor::BotCode.as_str(), "bot_code");
        assert_eq!(DeviceActor::AgentTool.as_str(), "agent_tool");
    }
}
