//! hbot-core：领域模型、端口 trait、错误与 ID。
//!
//! 边界（见 `docs/03-directory.md` §2）：
//!
//! - 只有实体、值类型、端口 trait、错误；
//! - **不做 IO**，**不依赖 workspace 内其他 crate**（只依赖 serde / thiserror / uuid）；
//! - 时间与 UUID 经 [`ports::Clock`] / [`ports::IdGenerator`] 注入，便于测试与重放。
//!
//! 模块划分：
//!
//! - [`ids`]：`ProjectId` / `BotId` / `ConversationId` / `SessionId` / `MessageId` /
//!   `InboundId` / `OutboundId` / `ApprovalId` / `EditId` / `SearchId` newtype；
//! - [`errors`]：`HbotError` 与 RPC 错误码映射（`code()`）；
//! - [`domain`]：实体与枚举，只有数据，没有业务规则；
//! - [`ports`]：同步端口 trait（时钟、ID 生成、事件出口）。

#![warn(missing_docs)]

pub mod domain;
pub mod errors;
pub mod ids;
pub mod ports;

pub use domain::{
    ApprovalDecision, Bot, BotStatus, Conversation, ConversationKind, InboundMessage,
    OutboundMessage, OutboundState, Project, SandboxMode, Timestamp, ToolCall, ToolCallStatus,
};
pub use errors::HbotError;
pub use ids::{
    ApprovalId, BotId, ConversationId, EditId, InboundId, MessageId, OutboundId, ProjectId,
    SearchId, SessionId,
};
pub use ports::{Clock, EventSink, IdGenerator};
