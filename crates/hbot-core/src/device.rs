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
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

    /// 这个子能力是不是"抓一整帧"（要抓整张位图、可能还要跑识别）。
    ///
    /// 限速用它把截屏与 OCR 单独收紧到 [`DeviceRateLimits::frames_per_second`]：
    /// 抓一帧的代价是点一下鼠标的几百倍，两者不该共用同一个配额。
    #[must_use]
    pub fn is_frame(self) -> bool {
        matches!(self, Self::Screen | Self::Ocr)
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
    /// 调得太频繁（[`DeviceRateWindow`] 的判据）。
    ///
    /// **与 [`Self::HostLimit`] 必须是两个变体**：那一个是"这台机器做不到"，
    /// 这一个是"这台机器做得到、但你先歇一下"。混成一个码，用户看到"不支持设备能力"
    /// 会去查系统设置，而真正的处置是少调几次。
    RateLimited {
        /// 被限住的子能力。
        capability: DeviceCapability,
        /// 多久之后可以再试（毫秒）。UI 可以直接拿它做倒计时。
        retry_after_ms: u64,
    },
    /// 后端没在预算内返回。
    ///
    /// 同样**不能报成 [`Self::HostLimit`]**（`docs/09` §13 问 8）：宿主明明支持，
    /// 只是这次慢了。用户看到 `HOST_LIMIT` 会去查"我的系统为什么不支持截屏"。
    Timeout {
        /// 本次调用的预算（毫秒）。
        ms: u64,
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
            Self::RateLimited { .. } => "HBOT_DEVICE_RATE_LIMITED",
            Self::Timeout { .. } => "HBOT_DEVICE_TIMEOUT",
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
            Self::RateLimited {
                capability,
                retry_after_ms,
            } => write!(
                f,
                ": `{capability}` 调得太频繁，{} 毫秒后再试",
                retry_after_ms
            ),
            Self::Timeout { ms } => write!(f, ": 设备后端在 {ms} 毫秒内没有返回"),
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

/// OCR 请求：把屏幕上的文字读成文本。
///
/// 为什么窄版走 OCR 而不是把图给模型（`docs/09` §9.1）：[`crate::DeviceBackend`]
/// 这条链路两侧都不发图像块，而补视觉输入要改 provider 契约。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OcrRequest {
    /// 读哪个目标：`None` = 主屏整屏。
    pub target: DeviceTarget,
    /// 最多返回多少字符（`char` 数，不是字节）。
    ///
    /// 超了就**截断并把 [`OcrText::truncated`] 置 `true`** —— 这里与截屏的
    /// "超上限就报错"故意不同：半张图会让人看错界面，半段文字只是少读几行，
    /// 而"读不到"会让模型直接猜。
    pub max_chars: usize,
}

/// 一次 OCR 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OcrText {
    /// 识别出的文本（**不可信数据**：屏幕上的字可能是别人写的）。
    ///
    /// 消费者必须当外部内容处理：`bot` 侧要打标，Agent 侧要按 `Risk::External`
    /// 对待，任何"照屏幕上的话去做"的路径都要走审批（`docs/09` §9.1）。
    pub text: String,
    /// 是否因为 [`OcrRequest::max_chars`] 被截断。
    pub truncated: bool,
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
    /// 能跑 OCR（把屏幕文字读成文本）。
    pub ocr: bool,
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

/// 设备调用的频率上限（`docs/09` §13 问 7 的数字）。
///
/// 两个上限分开是为了对准两种伤害：
///
/// - **总调用数**（`calls_per_second`）拦"循环里狂点"—— 按钮会被点成双击、
///   组合键会自己拼出系统快捷键。改的是**别人的桌面状态**。
/// - **帧数**（`frames_per_second`）比总数更严：抓屏与 OCR 各自要抓一整张位图、
///   OCR 还要跑识别，代价是点一下鼠标的几百倍。8 次/秒的截屏能把一个 4 核机器
///   的 CPU 吃满，而 20 次/秒的点击不会。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRateLimits {
    /// 一秒内允许的设备调用总数。
    pub calls_per_second: u32,
    /// 一秒内允许的"帧"调用数（`screen` 与 `ocr` 各算一帧）。
    pub frames_per_second: u32,
}

impl Default for DeviceRateLimits {
    fn default() -> Self {
        Self {
            calls_per_second: 20,
            frames_per_second: 2,
        }
    }
}

/// 一秒的滑动窗口，拦"调得太频繁"。
///
/// **为什么是滑动窗口而不是令牌桶**：令牌桶允许突发（攒满 20 个令牌后一口气用掉），
/// 而这里的伤害正是突发 —— 一秒内 20 次点击等于把 20 个动作在几十毫秒内砸进别人的
/// 界面。滑动窗口把"任意 1 秒内不超过 N 次"变成硬约束。
///
/// 时间由调用方传入（[`Instant`]），不在这里读时钟：测试要能一秒一秒地推。
///
/// # 每份实现管一棵窗口
///
/// 窗口是**按被授权方**分的：bot 代码那条路每个 `Sandbox` 一个槽（一个槽就是一个
/// bot 的 isolate），Agent 那条路每个会话后端一个。判定代码只有这一份，窗口实例
/// 按调用方分 —— 与 [`DeviceGate`] 的关系一样（一个实现、几处装配）。
pub struct DeviceRateWindow {
    limits: DeviceRateLimits,
    state: Mutex<WindowState>,
}

/// 窗口里留的两次调用的时刻。`VecDeque` 从前面淘汰过期的。
struct WindowState {
    calls: std::collections::VecDeque<Instant>,
    frames: std::collections::VecDeque<Instant>,
}

/// 窗口长度：固定 1 秒（`*_per_second` 的"per second"就是这个意思）。
const WINDOW: Duration = Duration::from_secs(1);

impl Default for DeviceRateWindow {
    /// 文档里的默认上限（20 次/秒、2 帧/秒）。装配方不特别指定时用它。
    fn default() -> Self {
        Self::new(DeviceRateLimits::default())
    }
}

impl DeviceRateWindow {
    /// 用给定的上限建一棵空窗口。
    #[must_use]
    pub fn new(limits: DeviceRateLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(WindowState {
                calls: std::collections::VecDeque::new(),
                frames: std::collections::VecDeque::new(),
            }),
        }
    }

    /// 上限（UI 与文档都要能查，别让数字只活在代码里）。
    #[must_use]
    pub fn limits(&self) -> DeviceRateLimits {
        self.limits
    }

    /// 现在允许再一次 `capability` 调用吗？
    ///
    /// 允许就把这次调用记进窗口（**先判后记**：拒绝的那次不占配额，否则一个
    /// 被限住的循环会让窗口永远排满，用户的"等一秒"变成"等一辈子"）。
    ///
    /// # Errors
    ///
    /// [`DeviceError::RateLimited`]，带 `retry_after_ms`（最早那次调用滑出窗口的
    /// 剩余时间）。`0` 是不可能的 —— 判据成立时窗口里必有一条更早的记录。
    pub fn admit(&self, capability: DeviceCapability, now: Instant) -> Result<(), DeviceError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        prune(&mut state.calls, now);
        prune(&mut state.frames, now);

        // 先看更严的那条：帧超了即使总数没超也要拒。
        if capability.is_frame() {
            if let Some(retry_after_ms) = wait_ms(&state.frames, self.limits.frames_per_second, now)
            {
                return Err(DeviceError::RateLimited {
                    capability,
                    retry_after_ms,
                });
            }
        }
        if let Some(retry_after_ms) = wait_ms(&state.calls, self.limits.calls_per_second, now) {
            return Err(DeviceError::RateLimited {
                capability,
                retry_after_ms,
            });
        }

        state.calls.push_back(now);
        if capability.is_frame() {
            state.frames.push_back(now);
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceRateWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceRateWindow")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// 淘汰滑出窗口的记录。`>=` 与 `admit` 里的判据一致：正好 1 秒整就算过期。
fn prune(queue: &mut std::collections::VecDeque<Instant>, now: Instant) {
    while let Some(oldest) = queue.front() {
        match now.checked_duration_since(*oldest) {
            Some(age) if age >= WINDOW => {
                queue.pop_front();
            }
            // `None` = 记录的时刻在 `now` 之后（测试里时钟被推回去）。
            // 保守起见当成"还没过期"，不淘汰 —— 宁可少放行一次。
            _ => break,
        }
    }
}

/// 还差多少毫秒才能再来一次；`None` = 现在就可以。
fn wait_ms(queue: &std::collections::VecDeque<Instant>, limit: u32, now: Instant) -> Option<u64> {
    // 上限 0 = 永远拒。这一支必须写在"空窗口"之前：空窗口的 `front()` 是
    // `None`，若靠 `?` 短路就会把"零配额"解释成"随便用"（这正是第一版测试
    // 抓到的 bug）。
    if limit == 0 {
        return Some(one_window_ms());
    }
    if u32::try_from(queue.len()).unwrap_or(u32::MAX) < limit {
        return None;
    }
    let oldest = queue.front()?;
    let left = match now.checked_duration_since(*oldest) {
        Some(age) => WINDOW.saturating_sub(age),
        // 时钟倒退（测试推进了 `Instant` 或系统时间跳变）：当成整窗都没过，
        // 宁可少放行一次，也不许凭一次倒退凭空多出配额。
        None => WINDOW,
    };
    Some(u64::try_from(left.as_millis()).unwrap_or(0).max(1))
}

/// 一个窗口长度的毫秒数（`u64`，至少 1 —— 0 会让 UI 画出"立刻可重试"）。
fn one_window_ms() -> u64 {
    u64::try_from(WINDOW.as_millis()).unwrap_or(1_000).max(1)
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

/// 能力声明端口：`ToolGateway` 的第一步。
///
/// 与 [`DeviceGate`] 的分工是刻意的，两个都要有、谁也不能替谁：
///
/// - [`CapabilityGate`] 判的是**静态声明层** —— 这份代码（项目清单、bot 的
///   `permissions`）有没有资格要这一类能力。它不看"现在有没有人点头"。
/// - [`DeviceGate`] 判的是**运行时授权层** —— 这一次会话、这一刻、对谁。
///
/// 少了前者，一个没声明 `ocr` 的 bot 只要用户授权过一个 `screen` 会话就能读屏；
/// 少了后者，声明了就永久可用，一键撤销形同虚设。
///
/// 实现必须是**无状态判定**（每次调用重新读声明），不许在构造时快照。
pub trait CapabilityGate: Send + Sync {
    /// 这份代码能不能要这个能力。
    fn authorize(&self, capability: DeviceCapability) -> Result<(), DeviceError>;
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

    /// 读屏幕文字（OCR）。
    ///
    /// 实现自己负责"先抓图再做识别"：调用方不该为了 OCR 先要一张 PNG
    /// （那张 PNG 会白白穿过一遍进程边界，还多一个落地/泄漏的机会）。
    /// 引擎不可用时返回 [`DeviceError::HostLimit`]，**不要**退回空字符串 ——
    /// 空文本会让模型以为屏幕上什么都没有。
    fn ocr(&self, request: &OcrRequest) -> Result<OcrText, DeviceError>;

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

    /// 只有截屏与 OCR 算"抓一帧"。
    #[test]
    fn only_screen_and_ocr_count_as_frames() {
        assert!(DeviceCapability::Screen.is_frame());
        assert!(DeviceCapability::Ocr.is_frame());
        for capability in [
            DeviceCapability::Input,
            DeviceCapability::Window,
            DeviceCapability::Clipboard,
        ] {
            assert!(!capability.is_frame(), "{capability} 不该算帧");
        }
    }

    /// 默认上限是 `docs/09` §13 问 7 定的 20 次/秒、2 帧/秒。
    #[test]
    fn the_default_limits_are_the_documented_ones() {
        let limits = DeviceRateLimits::default();
        assert_eq!(limits.calls_per_second, 20);
        assert_eq!(limits.frames_per_second, 2);
    }

    /// 第 21 次点击被拒，且被拒的那次**不占配额**。
    #[test]
    fn the_twenty_first_call_in_a_second_is_refused() {
        let window = DeviceRateWindow::new(DeviceRateLimits::default());
        let start = Instant::now();
        for index in 0..20 {
            let now = start + Duration::from_millis(index);
            assert!(
                window.admit(DeviceCapability::Input, now).is_ok(),
                "第 {} 次不该被拒",
                index + 1
            );
        }

        let refused = window
            .admit(DeviceCapability::Input, start + Duration::from_millis(20))
            .expect_err("第 21 次必须被拒");
        match refused {
            DeviceError::RateLimited {
                capability,
                retry_after_ms,
            } => {
                assert_eq!(capability, DeviceCapability::Input);
                // 最早那次在 t=0，refused 在 t=20ms：还要 ~980ms 才滑出去。
                assert!(
                    retry_after_ms > 900 && retry_after_ms <= 1000,
                    "retry_after_ms = {retry_after_ms}"
                );
            }
            other => panic!("期望 RateLimited，得到 {other:?}"),
        }

        // 拒绝不占配额：等一秒后一次成功，且窗口里只有这两条。
        assert!(
            window
                .admit(
                    DeviceCapability::Input,
                    start + Duration::from_millis(1_100)
                )
                .is_ok()
        );
    }

    /// 帧比总数更严：总数还剩 18 个名额，第 3 张截图仍然被拒。
    #[test]
    fn the_third_frame_in_a_second_is_refused_while_calls_are_still_plentiful() {
        let window = DeviceRateWindow::new(DeviceRateLimits::default());
        let start = Instant::now();
        assert!(window.admit(DeviceCapability::Screen, start).is_ok());
        assert!(
            window
                .admit(DeviceCapability::Ocr, start + Duration::from_millis(10))
                .is_ok()
        );
        // 两者共用帧配额：一张截图 + 一次 OCR 已经用满了 2 帧。
        match window
            .admit(DeviceCapability::Screen, start + Duration::from_millis(20))
            .expect_err("第 3 帧必须被拒")
        {
            DeviceError::RateLimited {
                capability,
                retry_after_ms,
            } => {
                assert_eq!(capability, DeviceCapability::Screen);
                assert!(retry_after_ms > 900, "retry_after_ms = {retry_after_ms}");
            }
            other => panic!("期望 RateLimited，得到 {other:?}"),
        }
        // 同时刻的点击不受帧配额影响（它不吃帧）。
        assert!(
            window
                .admit(DeviceCapability::Input, start + Duration::from_millis(20))
                .is_ok()
        );
    }

    /// 一秒之后配额自动回来（滑动窗口，不是"每秒钟清零一次"）。
    #[test]
    fn the_window_slides_instead_of_resetting() {
        let window = DeviceRateWindow::new(DeviceRateLimits::default());
        let start = Instant::now();
        for _ in 0..20 {
            window
                .admit(DeviceCapability::Input, start)
                .expect("前 20 次都该放行");
        }
        assert!(
            window.admit(DeviceCapability::Input, start).is_err(),
            "同一时刻的第 21 次必须被拒"
        );
        // 差 1 毫秒还不够（`>=` WINDOW 才算过期）。
        assert!(
            window
                .admit(DeviceCapability::Input, start + Duration::from_millis(999))
                .is_err(),
            "999ms 时最早那条还没滑出去"
        );
        assert!(
            window
                .admit(DeviceCapability::Input, start + WINDOW)
                .is_ok(),
            "正好 1 秒整应当放行"
        );
    }

    /// 上限是 0 时永远拒，且 `retry_after_ms` 不许给 0（否则 UI 会画
    /// "立刻可重试"然后立刻再被拒）。
    #[test]
    fn a_zero_limit_never_admits_and_never_promises_zero_wait() {
        let window = DeviceRateWindow::new(DeviceRateLimits {
            calls_per_second: 0,
            frames_per_second: 0,
        });
        let start = Instant::now();
        for capability in [DeviceCapability::Input, DeviceCapability::Screen] {
            match window
                .admit(capability, start)
                .expect_err("零配额必须一次都不放行")
            {
                DeviceError::RateLimited {
                    capability: refused,
                    retry_after_ms,
                } => {
                    assert_eq!(refused, capability);
                    // 关键：空窗口也要给一个正的等待时间。
                    assert_eq!(retry_after_ms, 1_000);
                }
                other => panic!("期望 RateLimited，得到 {other:?}"),
            }
        }
    }

    /// 时钟倒退（测试或系统时间跳变）时不 panic、不误放行一堆。
    #[test]
    fn a_backwards_clock_does_not_panic_or_flood_the_window() {
        let window = DeviceRateWindow::new(DeviceRateLimits {
            calls_per_second: 1,
            frames_per_second: 1,
        });
        let start = Instant::now();
        assert!(window.admit(DeviceCapability::Input, start).is_ok());
        // 比记录更早的时刻：`checked_duration_since` 给 None ⇒ 当成没过期。
        assert!(
            window
                .admit(DeviceCapability::Input, start - Duration::from_millis(500))
                .is_err(),
            "时钟倒退不该凭空多出配额"
        );
    }
}
