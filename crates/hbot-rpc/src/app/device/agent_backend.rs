//! Agent 工具用的设备后端装饰器：**先问"这一刻有没有生效的授权"，再交给真后端**。
//!
//! 为什么需要它（`docs/09-device-capability.md` §4）：
//!
//! 设备能力有两个消费者，判据层数不一样：
//!
//! | 消费者 | 判据 |
//! |---|---|
//! | bot 代码 `ctx.device.*` | `hbot-code` 的 `DeviceGate`（总开关 → 清单声明 → 设置子能力 → 会话 → 目标） |
//! | Agent 工具 `read_screen` | `ToolGateway` 的 `Capability` 步（**只有清单声明**）+ 逐次审批 |
//!
//! 少了这一层，撤销设备会话停得住 bot 代码、停不住 IDE 里的 Agent —— "限时会话 + 一键撤销"
//! 就只对一半消费者有效。所以 Agent 侧的后端不是裸 `hbot_device::platform_backend()`，
//! 而是包一层的这个结构体：每个方法先调 [`DeviceRegistry::authorize_tool`]，通过了才转发。
//!
//! 一个刻意的选择：**不在构造时快照授权**。装饰器每次调用都重问登记表，
//! 因为"用户刚点了撤销"必须在下一次调用上就生效（与清单声明层"每次重读"同一条理由）。
//!
//! 登记表已经没了（`Weak` 升级失败）时返回 [`DeviceError::Failed`] —— 放行是绝对不行的，
//! 拒绝的理由也说得清："授权登记表已销毁"。

use std::sync::{Arc, Weak};

use hbot_core::{
    ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapability, DeviceCapabilityReport,
    DeviceError, InputAction, OcrRequest, OcrText, Screenshot, ScreenshotRequest, WindowInfo,
};

use super::DeviceRegistry;

/// 把 [`DeviceRegistry`] 的会话判据盖在真后端上面的包装。
pub struct SessionScopedBackend {
    /// 真正的平台后端（`hbot_device::platform_backend()`）。它只执行、不判断。
    inner: Arc<dyn DeviceBackend>,
    /// 授权登记表。`Weak`：装饰器不该延长登记表的寿命（与 `DeviceRegistry::gate` 同理）。
    registry: Weak<DeviceRegistry>,
}

impl SessionScopedBackend {
    /// 包一层。
    pub fn new(inner: Arc<dyn DeviceBackend>, registry: &Arc<DeviceRegistry>) -> Self {
        Self {
            inner,
            registry: Arc::downgrade(registry),
        }
    }

    /// 每个能力方法的第一句：授权判据。
    fn authorize(&self, capability: DeviceCapability) -> Result<(), DeviceError> {
        let Some(registry) = self.registry.upgrade() else {
            return Err(DeviceError::Failed {
                message: "授权登记表已销毁，Agent 不再有权调用设备能力".to_owned(),
            });
        };
        registry.authorize_tool(capability)
    }
}

impl DeviceBackend for SessionScopedBackend {
    /// 能力报告**不加判据**：它只描述"这台机器能做到什么"，不是一次设备操作。
    ///
    /// 让模型知道"这台机器有 OCR"是合理的（否则它不会想到调 `read_screen`，
    /// 用户也就永远不会被问到）；能不能真的读屏由下面每个方法各自判。
    fn capabilities(&self) -> DeviceCapabilityReport {
        self.inner.capabilities()
    }

    fn screenshot(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        self.authorize(DeviceCapability::Screen)?;
        self.inner.screenshot(request)
    }

    fn ocr(&self, request: &OcrRequest) -> Result<OcrText, DeviceError> {
        self.authorize(DeviceCapability::Ocr)?;
        self.inner.ocr(request)
    }

    fn input(&self, action: &InputAction, allowed_apps: &[String]) -> Result<(), DeviceError> {
        // `authorize_tool` 会直接拒 `input`（它必须有窗口目标，Agent 侧没有）。
        // 这里仍然先判一次，让"为什么被拒"来自同一个地方。
        self.authorize(DeviceCapability::Input)?;
        self.inner.input(action, allowed_apps)
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError> {
        // 同上：`window` 需要目标，Agent 侧一律拒。
        self.authorize(DeviceCapability::Window)?;
        self.inner.windows()
    }

    fn clipboard(&self, op: &ClipboardOp) -> Result<ClipboardValue, DeviceError> {
        self.authorize(DeviceCapability::Clipboard)?;
        self.inner.clipboard(op)
    }
}
