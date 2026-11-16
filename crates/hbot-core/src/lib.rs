//! hbot-core：领域模型、端口 trait、错误与 ID。
//!
//! 边界（见 `docs/03-directory.md` §2）：
//!
//! - 只有实体、值类型、端口 trait、错误，以及一个**默认实现**：[`SystemClock`]；
//! - **不做 IO**（[`SystemClock`] 只读 `std::time::SystemTime`，不碰文件/网络/SDK），
//!   **不依赖 workspace 内其他 crate**（只依赖 serde / thiserror / uuid）；
//! - 时间与 UUID 经 [`ports::Clock`] / [`ports::IdGenerator`] 注入，便于测试与重放。
//!
//! 模块划分：
//!
//! - [`ids`]：`ProjectId` / `BotId` / `ConversationId` / `SessionId` / `MessageId` /
//!   `InboundId` / `OutboundId` / `ApprovalId` / `EditId` / `SearchId` newtype；
//! - [`errors`]：`HbotError` 与 RPC 错误码映射（`code()`）；
//! - [`domain`]：实体与枚举，只有数据，没有业务规则；
//! - [`command`]：受控命令执行的契约类型（`CommandSpec` / `CommandOutput` /
//!   `CommandDenied`），实现放在 `hbot-command`，见 `docs/adr/0003-command-isolation.md`；
//! - [`ports`]：同步端口 trait（时钟、ID 生成、事件出口、命令执行）+ [`SystemClock`]。
//!   `SystemClock` 放这里是因为读系统时间不是平台适配；放适配器层会逼着组合根
//!   （如 `hbot-desktop`）为了一个时钟依赖整个 `hbot-adapters`，那是错误的依赖方向。
//! - [`paths`]：应用数据目录的唯一算法（桌面端与后台端必须落到同一个库）。
//! - [`device`]：设备能力（C6）的契约类型与两个端口（[`device::DeviceGate`] 判据、
//!   [`device::DeviceBackend`] 执行）。实现分别在 `hbot-rpc` 与 `hbot-device`，
//!   设计见 `docs/09-device-capability.md`。

#![warn(missing_docs)]

pub mod command;
pub mod debug;
pub mod device;
pub mod domain;
pub mod errors;
pub mod ids;
pub mod paths;
pub mod ports;
pub mod redact;

pub use command::{
    CommandDenied, CommandOutput, CommandSpec, CommandStatus, OutputStream, Platform,
};
pub use debug::{Breakpoints, DebugAction, DebugFrame, DebugScope, DebugSink, PausedAt};
pub use device::{
    ClipboardOp, ClipboardValue, DeviceActor, DeviceAuditEntry, DeviceBackend, DeviceCapability,
    DeviceCapabilityReport, DeviceError, DeviceGate, DeviceSessionState, DeviceTarget, InputAction,
    MouseButton, Screenshot, ScreenshotRequest, WindowInfo,
};
pub use domain::{
    ApprovalDecision, Bot, BotStatus, Conversation, ConversationKind, InboundMessage,
    OutboundMessage, OutboundState, Project, SandboxMode, Timestamp, ToolCall, ToolCallStatus,
};
pub use errors::HbotError;
pub use ids::{
    ApprovalId, BotId, ConversationId, EditId, InboundId, MessageId, OutboundId, ProjectId,
    SearchId, SessionId,
};
pub use paths::{APP_DIR, data_dir, data_dir_for};
pub use ports::{Clock, CommandRunner, EventSink, IdGenerator, SystemClock};
pub use redact::{
    REDACTED, RedactedArgs, RedactedEnv, RedactedJson, is_secret_key, redact_arg, redact_args,
    redact_text, redact_value,
};
