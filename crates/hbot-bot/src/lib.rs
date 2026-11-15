//! hbot-bot — 平台无关的 bot 运行时：supervisor、每会话队列、outbox、入站去重。
//!
//! ## 边界
//!
//! 本 crate **不含平台协议**（OneBot/Webhook/Telegram 在 `hbot-adapters`），也不含
//! QuickJS / Agent 循环（在 `hbot-code` / `hbot-agent`）。它只负责把
//! “适配器送来一条事件”变成“处理器按会话串行执行一次、回复按序发出”
//! （`docs/03-directory.md` §2：`crates/hbot-bot` 一行）。
//!
//! ## 主链路
//!
//! ```text
//! BotAdapter::subscribe ─┐
//!                        ▼
//!              read_loop（supervisor）
//!                        │  Inbox::admit          先落库（UNIQUE 去重）
//!                        ▼
//!         SessionQueues::enqueue / try_enqueue  每会话有界队列、串行 worker
//!                        │  RecordingHandler::on_message
//!                        ▼
//!          HandlerExecutor::on_message（下一批接 QuickJS）
//!                        │  Outbox::enqueue        先落库 pending
//!                        ▼
//!          BotAdapter::send（串行 + 指数退避 + 上限）
//! ```
//!
//! 四段不变量（`AGENTS.md` §4.1）在代码里的落点：
//!
//! | 不变量 | 落点 |
//! |---|---|
//! | 先落库再处理 | [`dedupe::Inbox::admit`] 在入队之前 |
//! | 重复投递只执行一次 | `inbound` 唯一约束 + [`dedupe::Admission::Duplicate`] |
//! | 每会话串行、不同会话并行 | [`queue::SessionQueues`] |
//! | 队列有界、满则背压不丢 | [`queue::QueueError::Backpressure`] + [`queue::SessionQueues::enqueue`] |
//!
//! ## 启动/停止顺序
//!
//! 见 [`supervisor`] 的模块文档：先建 bot/conversation 行、再订阅、再恢复 outbox、
//! 再重放上次没处理完的入站消息、最后才读事件（`AGENTS.md` §7.1 的旧坑）；停止是
//! 停止读取 → 排空 → 取消 worker → flush。
//!
//! ## 依赖注入
//!
//! 时钟、ID、适配器、执行器全部由组合根注入（[`Clock`] / [`IdGenerator`] /
//! [`BotAdapter`] / [`HandlerExecutor`]）；本 crate 不产生随机数、不读系统时间，测试因此
//! 完全确定性。

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod adapter;
pub mod bot;
pub mod dedupe;
pub mod event;
pub mod handler;
mod ids;
pub mod outbox;
pub mod queue;
mod store_ops;
pub mod supervisor;

pub use adapter::{AdapterSubscription, BotAdapter, OutboundEnvelope};
pub use bot::BotHandle;
pub use dedupe::{Admission, Inbox};
pub use event::{ConversationRef, PlatformEvent};
pub use handler::{HandlerExecutor, MessageCtx};
pub use ids::conversation_id_for;
pub use outbox::{
    NoPendingOutbox, OutboundRequest, Outbox, OutboxConfig, OutboxStats, PendingOutbound,
    PendingOutboxSource,
};
pub use queue::{FailureTracker, QueueConfig, QueueError, SessionKey, SessionQueues};
pub use supervisor::{
    BotSnapshot, BotSpec, InjectOutcome, StartupStep, Supervisor, SupervisorDeps,
};
