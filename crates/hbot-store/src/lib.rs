//! hbot-store — SQLite 持久层：迁移、连接、repository。
//!
//! ## 边界
//!
//! 只做存储（`docs/03-directory.md` §2.2、§5）：SQL schema 的唯一真源是
//! [`migrations`]（`src/migrations/*.sql`），SQL 语句只出现在 `repositories/`。这里不放
//! 业务规则（去重策略、重试节奏、审批）也不放 Agent 逻辑。
//!
//! ID 类型不在本 crate 定义：[`ids`] 只是 `hbot_core::ids` 的重导出（`AGENTS.md` §0.4
//! 「只有一份契约」）。行主键用 `InboundId` / `OutboundId`，**不用** `MessageId`——后者是
//! 生成代码看到的幂等键（`ctx.messageId`），与入站行 id 同值但概念不同。落库格式是
//! TEXT 上的 UUID 字符串。
//!
//! ## 可靠性契约（`docs/02-architecture.md` §4.1、§7）
//!
//! * **先落库再处理**：入站消息先 `insert_inbound_if_new`，再入队；
//! * **幂等**：`inbound` 上 `UNIQUE(bot_id, external_id)`，重复投递只返回一次 `Some`；
//! * **单 writer**：写走专用 `std::thread` + 有界 `mpsc`；续发/排空类读与写排队同序，
//!   状态面板类读走 2 条只读连接（见 [`Store`] 的「两条读路径」说明）；
//! * **不静默降级**：WAL / 外键 / 忙等窗口任一施加失败都返回错误。
//!
//! ## 调用约定（硬规则）
//!
//! 全部 API **同步阻塞**。`rusqlite` 没有 async 接口，SDK 也不能在 tokio worker 上阻塞，
//! 所以**在 async 上下文里必须把每次调用放进 `tokio::task::spawn_blocking`**：
//!
//! ```text
//! let inserted = tokio::task::spawn_blocking({
//!     let store = Arc::clone(&store);
//!     move || store.insert_inbound_if_new(message)
//! }).await??;
//! ```
//!
//! 直接在 `async fn` 里 `store.insert_inbound_if_new(...)` 会占住 tokio worker，表现为
//! Agent 流式输出停顿、心跳超时——这不是性能问题，是正确性问题。
//!
//! ## 外键落地顺序
//!
//! `foreign_keys=ON` 且不降级，因此写 `inbound` / `outbound` / `logs` 前必须先有
//! `bots` 行，写 `inbound` / `outbound` 前还必须有 `conversations` 行。适配器顺序：
//! 写 bot →（首次见到会话时）写 conversation → 写 inbound → 入队处理。
//!
//! ```no_run
//! use hbot_store::{NewInbound, Store};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = Store::open("hbot.db")?;
//! let message = NewInbound {
//!     id: "00000000-0000-0000-0000-000000000001".parse()?,
//!     bot_id: "00000000-0000-0000-0000-000000000002".parse()?,
//!     external_id: "platform-msg-1".to_owned(),
//!     conversation_id: "00000000-0000-0000-0000-000000000003".parse()?,
//!     payload_json: "{\"text\":\"hi\"}".to_owned(),
//!     received_at: 1_700_000_000_000,
//! };
//! if store.insert_inbound_if_new(message)?.is_some() {
//!     // 首次见到这条外部消息：入队处理。
//! }
//! # Ok(())
//! # }
//! ```
#![deny(unsafe_code)]

mod db;
mod error;
mod ids;
mod mappers;
mod migrations;
mod models;
mod repositories;
pub mod startup_trace;

#[cfg(test)]
mod device_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use db::{BUSY_TIMEOUT_MS, Store};
pub use error::{Result, StoreError};
pub use ids::{BotId, ConversationId, InboundId, JobId, OutboundId, ProjectId, WaitId};
pub use models::{
    BOT_STATUS_DEGRADED, BOT_STATUS_ERROR, BOT_STATUS_RUNNING, BOT_STATUS_STOPPED, BOT_STATUSES,
    Bot, BotStatusRow, Conversation, DeviceAuditRow, DeviceSessionRow, HistoryRow,
    InboundReplayRow, InboundRow, JobRow, KvRow, LogRow, NewAgentLog, NewDeviceAudit,
    NewDeviceSession, NewInbound, NewJob, NewKvEntry, NewLog, NewOutbound, NewWait,
    OUTBOUND_STATE_FAILED, OUTBOUND_STATE_PENDING, OUTBOUND_STATE_SENT, OutboundRow,
    OutboundTarget, Project, WaitRow, is_valid_bot_status,
};
