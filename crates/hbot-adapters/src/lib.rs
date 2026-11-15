//! `hbot-adapters`：平台适配器与受限 IO。
//!
//! 职责（`docs/03-directory.md`）：
//!
//! - **平台协议**：[`platforms`]（当前实现 OneBot 11）；
//! - **受限 HTTP**：[`http`]（超时、大小上限、拒绝内网，给工具层抓网页用）；
//! - **注入用的实现**：[`clock`]（系统时钟）。
//!
//! 依赖约束：本 crate 只依赖 `hbot-core` + 通用库，不依赖 `hbot-bot` /
//! `hbot-agent` / `hbot-store`。业务编排（supervisor、会话队列、去重落库）
//! 在 `hbot-bot`；本 crate 只负责协议翻译与传输。
//!
//! 错误统一为 [`AdapterError`]，边界处 `From<AdapterError> for HbotError` 转成
//! RPC 错误（code/消息统一，不在适配器里另造一套错误码）。
//!
//! # 快速上手
//!
//! ```no_run
//! # use hbot_adapters::platforms::onebot::{OneBotClient, OneBotConfig};
//! # async fn run() -> Result<(), hbot_adapters::AdapterError> {
//! let mut client = OneBotClient::start(OneBotConfig::new("ws://127.0.0.1:3001"))?;
//! while let Some(message) = client.next_event().await {
//!     println!("{}: {}", message.sender_id, message.text);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # 已知边界
//!
//! - 没有 TLS 后端：`wss://` 与 `https://` 明确报
//!   [`AdapterError::Unsupported`]，不静默降级成明文。
//! - 未接真实 NapCat 实例做端到端联调；测试用进程内 mock WebSocket 服务器。

#![warn(missing_docs)]

pub mod clock;
pub mod error;
pub mod http;
pub mod platforms;

pub use clock::SystemClock;
pub use error::AdapterError;
pub use http::{HttpTarget, Scheme, http_fetch, is_blocked_ip, parse_target};
pub use platforms::onebot;
