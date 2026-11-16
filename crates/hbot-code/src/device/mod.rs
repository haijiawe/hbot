//! 设备能力在 QuickJS 沙箱里的宿主侧装配（C6）。
//!
//! 设计见 `docs/09-device-capability.md`。这里只有两件事：
//!
//! 1. [`DeviceHandles`] —— 宿主注入的一次装配：判据端口 [`DeviceGate`] +
//!    执行端口 [`DeviceBackend`] + 白名单。两个端口本身在
//!    `hbot_core::device`（**已冻结**，本 crate 只实现调用侧）。
//! 2. [`DeviceSlot`] —— `Sandbox` 上的**可替换**槽位。宿主函数**每次调用**都来
//!    这里读一次，所以撤销/换闸对下一个 `dispatch` 立刻生效，而不是只对下一个
//!    isolate 生效（`docs/09` §4.2）。
//!
//! 判据的顺序**不在这里**，在 [`gate`]：两个消费者（生成代码的 `ctx.device.*`
//! 与 Agent 工具的 `ToolGateway`）必须走同一份顺序，见 `docs/09` §2。
//!
//! # 为什么槽位不是 `OnceLock`
//!
//! [`OnceLock`] 只能写一次，而设备会话会被撤销、设置会被改、测试要换闸。用
//! `Mutex<Option<..>>` 换来"每次调用读当前值"，代价是一次几乎不可能竞争的小
//! 锁；`Arc<dyn ..>` 的克隆是引用计数加法，不碰后端。
//!
//! [`OnceLock`]: std::sync::OnceLock

pub mod gate;

use std::sync::{Arc, Mutex, PoisonError};

pub use gate::DeviceFailure;

use hbot_core::{
    BotId, DeviceBackend, DeviceCapability, DeviceError, DeviceGate, DeviceTarget, SandboxMode,
    Screenshot,
};

/// 一帧截图的默认/上限字节数（4 MiB）。
///
/// 填进 [`hbot_core::ScreenshotRequest::max_bytes`]；handler 可以用 `maxBytes`
/// 要更小的帧，但**不能**更大 —— 上限是宿主定的，不是调用方定的。
pub const MAX_SCREEN_BYTES: usize = 4 * 1024 * 1024;

/// `key(text)` 一次最多注入的文本字节数。
///
/// 键盘注入是逐字符的宿主调用，一兆的字符串只会把事件预算耗光；这个上限让
/// handler 在参数检查阶段就拿到明确错误，而不是等到超时。
pub const MAX_INPUT_TEXT_BYTES: usize = 8 * 1024;

/// `clipboardWrite(text)` 一次最多写入的字节数（1 MiB）。
///
/// 剪贴板是进程外的共享状态：写进去的东西会活到下一次有人覆盖它，所以上限比
/// 截图小得多，也不想让 `ctx.device` 变成"把整个堆塞进剪贴板"的通道。
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// `ctx.device.screen()` 的返回值（JS 侧对象的宿主表示）。
///
/// PNG 是 **base64 文本**：QuickJS 里没有 Node 的 `Buffer`，把二进制塞进
/// `Uint8Array` 只会让 handler 自己再转一次；base64 是唯一"传过去就能用"的形状
/// （`docs/09` §8.3 的 `png: Uint8Array` 是文档版，落地时按 §8.3 的注记改成
/// 字符串）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenFrame {
    /// 物理像素宽。
    pub width: u32,
    /// 物理像素高。
    pub height: u32,
    /// PNG 的 base64（标准字母表，带 `=` 补齐）。
    pub png_base64: String,
    /// 这一帧来自哪个显示器（`0` = 主屏）。
    pub monitor: u32,
}

impl From<Screenshot> for ScreenFrame {
    fn from(screenshot: Screenshot) -> Self {
        Self {
            width: screenshot.width,
            height: screenshot.height,
            png_base64: base64_encode(&screenshot.png),
            monitor: screenshot.monitor,
        }
    }
}

/// 标准 base64（RFC 4648 §4）编码，带 `=` 补齐。
///
/// 手写而不是加依赖：一个 30 行的编解码器不值得为它新增一棵依赖树，而
/// `xtask/src/deps.rs` 的依赖表是白名单制。只做编码 —— 解码没人用，不写。
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    // 每个 3 字节组变成 4 个字符（最后一组用 `=` 补齐）。
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).map_or(0, |byte| u32::from(*byte));
        let third = chunk.get(2).map_or(0, |byte| u32::from(*byte));
        let triple = (first << 16) | (second << 8) | third;

        out.push(char::from(ALPHABET[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3f) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((triple >> 6) & 0x3f) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(triple & 0x3f) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

/// 宿主注入的一次设备装配：判据、执行、白名单。
///
/// 三个字段都是 `Arc`/小向量，[`Clone`] 是引用计数加法 —— 每次 `ctx.device.*`
/// 调用都克隆一份，为的是不跨 `await` 持有槽位的锁。
#[derive(Clone)]
pub struct DeviceHandles {
    gate: Arc<dyn DeviceGate>,
    backend: Arc<dyn DeviceBackend>,
    /// `DeviceBackend::input` 的最后一道前台比对用的白名单，**闸已经批过**的
    /// 那份（`hbot_core::device::DeviceBackend::input` 的文档）。空 = 一个都不
    /// 允许，后端据此拒掉注入（fail-closed）。
    allowed_apps: Vec<String>,
}

impl DeviceHandles {
    /// 一份装配。`allowed_apps` 默认为空，需要时用 [`DeviceSlot::set_apps`] 注入。
    #[must_use]
    pub fn new(gate: Arc<dyn DeviceGate>, backend: Arc<dyn DeviceBackend>) -> Self {
        Self {
            gate,
            backend,
            allowed_apps: Vec::new(),
        }
    }

    /// 判据端口。
    #[must_use]
    pub fn gate(&self) -> &dyn DeviceGate {
        &*self.gate
    }

    /// 执行端口。
    #[must_use]
    pub fn backend(&self) -> &dyn DeviceBackend {
        &*self.backend
    }

    /// 执行端口的共享句柄，交给 `spawn_blocking` 用。
    #[must_use]
    pub fn backend_arc(&self) -> Arc<dyn DeviceBackend> {
        Arc::clone(&self.backend)
    }

    /// 闸批过的应用白名单。
    #[must_use]
    pub fn allowed_apps(&self) -> &[String] {
        &self.allowed_apps
    }
}

impl std::fmt::Debug for DeviceHandles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 端口没有 `Debug`（trait object），所以只报形状：有没有白名单。
        formatter
            .debug_struct("DeviceHandles")
            .field("allowed_apps", &self.allowed_apps)
            .finish_non_exhaustive()
    }
}

/// `Sandbox` 上的设备槽：**可替换**的装配 + 本次事件的机器人身份。
///
/// `Send + Sync` 是硬要求（`Sandbox` 要能放进 `Arc` 交给 RPC 的执行器），
/// 两个 [`Mutex`] 都是短临界区，绝不跨 `await`。
#[derive(Default)]
pub(crate) struct DeviceSlot {
    inner: Mutex<SlotInner>,
    /// 本次事件的 `botId`。`build_event` 每次 `dispatch` 写入一次；判据要它才能
    /// 回答"哪个机器人的设备会话"。
    bot: Mutex<Option<BotId>>,
}

#[derive(Default)]
struct SlotInner {
    handles: Option<DeviceHandles>,
    /// 主人注入的应用白名单，先于/后于 [`DeviceSlot::set_device`] 都能生效。
    apps: Vec<String>,
}

impl DeviceSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 装入（或替换）一份装配。已经设置过的白名单保留。
    pub(crate) fn set_device(&self, gate: Arc<dyn DeviceGate>, backend: Arc<dyn DeviceBackend>) {
        let mut inner = self.lock();
        let apps = inner.apps.clone();
        inner.handles = Some(DeviceHandles {
            gate,
            backend,
            allowed_apps: apps,
        });
    }

    /// 设置应用白名单；装配已经装过就同时更新它。
    pub(crate) fn set_apps(&self, apps: Vec<String>) {
        let mut inner = self.lock();
        inner.apps = apps;
        let apps = inner.apps.clone();
        if let Some(handles) = inner.handles.as_mut() {
            handles.allowed_apps = apps;
        }
    }

    /// 记录本次事件的机器人（没有 `botId` 的事件写 `None`）。
    pub(crate) fn set_current_bot(&self, bot: Option<BotId>) {
        *self.bot.lock().unwrap_or_else(PoisonError::into_inner) = bot;
    }

    /// 当前的装配（没有就是 `None`，调用方据此报 `HBOT_DEVICE_UNAVAILABLE`）。
    pub(crate) fn handles(&self) -> Option<DeviceHandles> {
        self.lock().handles.clone()
    }

    /// 是否装配过设备能力（只读档下的报错文案靠它区分两种"没有"）。
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().handles.is_some()
    }

    /// **一份闸**的入口：read-only → 是否装配 → 机器人身份 → 清单/设置/会话。
    ///
    /// 顺序是 `docs/09` §4 的硬规则，`gate::authorize` 是它唯一实现；这里只负责
    /// 把槽位里的三样东西递过去。
    pub(crate) fn authorize(
        &self,
        mode: SandboxMode,
        capability: DeviceCapability,
        target: &DeviceTarget,
    ) -> Result<DeviceHandles, DeviceFailure> {
        let handles = self.handles();
        let bot = *self.bot.lock().unwrap_or_else(PoisonError::into_inner);
        gate::authorize(
            mode,
            handles.as_ref().map(DeviceHandles::gate),
            bot.as_ref(),
            capability,
            target,
        )?;
        // `authorize` 已经在 `handles` 为 `None` 时返回 `Unavailable`，走不到这里；
        // 仍然不用 `expect` 是因为"静默 panic 在宿主线程"比一个错误码难查。
        handles.ok_or(DeviceFailure::Unavailable)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for DeviceSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("DeviceSlot")
            .field("assembled", &inner.handles.is_some())
            .field("allowed_apps", &inner.apps)
            .finish_non_exhaustive()
    }
}

/// 把后端给出的 `DeviceError` 保留原样（它的 `code()` 就是 handler 看到的前缀）。
///
/// 只在这里做一次"是不是超上限"的判断：`DeviceBackend::screenshot` 不负责判断
/// 上限（`docs/09` §5 ②），但真返回一帧超限的 PNG 时必须拒，而不是把它塞进
/// JS 堆。
pub(crate) fn check_frame_size(png_bytes: usize, limit: usize) -> Result<(), DeviceError> {
    if png_bytes > limit {
        return Err(DeviceError::TooLarge { limit_bytes: limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 §10 的两个标准向量，外加空输入与 1/2 字节的补齐分支。
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_keeps_every_byte_distinct() {
        // 0x00/0xff/0x80 是最容易被手写编码器弄丢的三类字节。
        let bytes: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&bytes);
        assert_eq!(encoded.len(), bytes.len().div_ceil(3) * 4);
        assert!(encoded.chars().all(|c| c.is_ascii_graphic() || c == '='));
        assert_eq!(&encoded[..4], "AAEC");
    }

    #[test]
    fn a_frame_over_the_cap_is_refused_not_truncated() {
        assert!(check_frame_size(4, 4).is_ok());
        let error = check_frame_size(5, 4).expect_err("超一字节也要拒");
        assert_eq!(error.code(), "HBOT_DEVICE_TOO_LARGE");
    }

    #[test]
    fn the_slot_starts_empty_and_remembered_apps_survive_a_later_install() {
        let slot = DeviceSlot::new();
        assert!(!slot.is_assembled());
        slot.set_apps(vec!["code.exe".to_owned()]);

        // 先设应用、后装装配：白名单必须带进装配，否则后端的最后一道比对永远
        // 拒绝（fail-closed 过头 = 功能不可用）。
        struct AllowAll;
        impl DeviceGate for AllowAll {
            fn authorize(
                &self,
                _bot: &BotId,
                _capability: DeviceCapability,
                _target: &DeviceTarget,
            ) -> Result<(), DeviceError> {
                Ok(())
            }
        }
        struct Nothing;
        impl DeviceBackend for Nothing {
            fn capabilities(&self) -> hbot_core::DeviceCapabilityReport {
                hbot_core::DeviceCapabilityReport::default()
            }
            fn screenshot(
                &self,
                _request: &hbot_core::ScreenshotRequest,
            ) -> Result<Screenshot, DeviceError> {
                Err(DeviceError::Failed {
                    message: "no".to_owned(),
                })
            }
            fn input(
                &self,
                _action: &hbot_core::InputAction,
                _allowed_apps: &[String],
            ) -> Result<(), DeviceError> {
                Ok(())
            }
            fn windows(&self) -> Result<Vec<hbot_core::WindowInfo>, DeviceError> {
                Ok(Vec::new())
            }
            fn clipboard(
                &self,
                _op: &hbot_core::ClipboardOp,
            ) -> Result<hbot_core::ClipboardValue, DeviceError> {
                Ok(hbot_core::ClipboardValue {
                    text: String::new(),
                })
            }
        }
        slot.set_device(Arc::new(AllowAll), Arc::new(Nothing));
        assert!(slot.is_assembled());
        let handles = slot.handles().expect("装配过");
        assert_eq!(handles.allowed_apps(), ["code.exe".to_owned()]);

        // 再设一次应用白名单：已经装过的装配立刻看到新值。
        slot.set_apps(vec!["notepad.exe".to_owned()]);
        let handles = slot.handles().expect("装配过");
        assert_eq!(handles.allowed_apps(), ["notepad.exe".to_owned()]);
    }
}
