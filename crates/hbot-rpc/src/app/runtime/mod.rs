//! 运行时装配：把适配器、执行器、supervisor 拼成一个能跑的 bot。
//!
//! 这是 `bot_start` 的唯一实现路径（`docs/02-architecture.md` §4）：
//!
//! ```text
//! OneBotClient ──► BotAdapter（入站归一化 / 出站发送）
//!                      │
//!                      ▼
//!                 Supervisor ──► SessionQueues ──► SandboxHandler（QuickJS）
//!                      │
//!                      ▼
//!                   Outbox ──► BotAdapter::send
//! ```
//!
//! 为什么装配放在 `hbot-rpc` 而不是 `hbot-bot`：`hbot-bot` 不认识平台协议，也不认识
//! QuickJS（两个 crate 各自被测试独立驱动）。把"谁和谁接上"放在应用层，是新平台、
//! 新执行器接入时唯一要改的地方。

pub mod onebot;
pub mod sandbox_handler;

use hbot_core::HbotError;

/// 装配期错误：把各 crate 的错误归一成一句能看懂的说明。
///
/// 这里不新增错误码，只在需要时用 [`HbotError::invalid`] / `internal` 包一层：
/// wire 上的错误码只有 `docs/02-architecture.md` §9.4 那一张表。
pub fn setup_failed(what: &str, detail: impl std::fmt::Display) -> HbotError {
    HbotError::invalid(format!("{what}: {detail}"))
}
