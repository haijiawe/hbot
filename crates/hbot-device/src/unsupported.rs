//! 非 Windows 平台的后端：**明确拒绝一切设备调用**。
//!
//! 这个文件存在的理由不是"凑一个实现"，而是让"没实现的平台"表现为**可诊断的拒绝**
//! 而不是编译失败或运行时静默失败（`docs/09` §6.4 对 Wayland / 无会话的要求同理）：
//!
//! - 编译期：`cargo check --target x86_64-unknown-linux-gnu` 能过，非 Windows 分支不会腐烂；
//! - 运行期：宿主拿到的是 [`DeviceError::HostLimit`]，文案直接给 UI 用，用户看得懂
//!   "这个宿主没有设备能力实现"而不是一个空指针崩溃或"截了一张全黑的图"。
//!
//! 为什么不做 Linux 实现：窄版（`docs/09` §9.2）的出口标准是 Windows 单平台先通，
//! X11 还能用 XTest，Wayland 下注入输入基本没有通用路径（得要 portal + 用户逐个确认），
//! 那是一个独立的活儿，不该混进窄版。

use hbot_core::device::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapabilityReport, DeviceError, InputAction,
    OcrRequest, OcrText, Screenshot, ScreenshotRequest, WindowInfo,
};

/// 每条拒绝共用的说明文字（`capabilities().detail` 与每个方法的错误都是它，避免两处漂移）。
const NO_DEVICE_SUPPORT: &str = "这个宿主没有设备能力实现";

/// 非 Windows 平台的设备后端：所有能力都是 `false`，所有调用都返回
/// [`DeviceError::HostLimit`]。
///
/// 零字段结构体，所以 `Send + Sync` 是自动的，没有 `unsafe impl`、没有全局可变状态。
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedBackend;

impl UnsupportedBackend {
    /// 造一个后端。永不失败 —— 拒绝发生在调用时，不是构造时。
    pub fn new() -> Self {
        Self
    }

    /// 统一的拒绝值。
    fn refuse<T>() -> Result<T, DeviceError> {
        Err(DeviceError::HostLimit {
            detail: NO_DEVICE_SUPPORT.to_string(),
        })
    }

    /// 显示器清单：这个平台没有实现，所以是空的。
    ///
    /// 与其它方法不同，这里**不报错**：返回值是 [`crate::displays`] 的契约（`Vec` 而不是
    /// `Result`），"没有实现"和"没有屏"在非 Windows 上本来就是一件事，空清单足够让调用方
    /// 知道"这个宿主没有显示器可截"。真要报错请走 `screenshot`。
    pub fn displays() -> Vec<crate::DisplayInfo> {
        Vec::new()
    }
}

impl DeviceBackend for UnsupportedBackend {
    fn capabilities(&self) -> DeviceCapabilityReport {
        DeviceCapabilityReport {
            screen: false,
            ocr: false,
            input: false,
            window: false,
            clipboard: false,
            detail: Some(NO_DEVICE_SUPPORT.to_string()),
        }
    }

    fn screenshot(&self, _request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        Self::refuse()
    }

    fn ocr(&self, _request: &OcrRequest) -> Result<OcrText, DeviceError> {
        Self::refuse()
    }

    fn input(&self, _action: &InputAction, _allowed_apps: &[String]) -> Result<(), DeviceError> {
        Self::refuse()
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError> {
        Self::refuse()
    }

    fn clipboard(&self, _op: &ClipboardOp) -> Result<ClipboardValue, DeviceError> {
        Self::refuse()
    }
}
