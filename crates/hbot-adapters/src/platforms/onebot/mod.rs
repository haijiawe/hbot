//! OneBot 11 适配器（正向 WebSocket 客户端 + 事件归一化 + 出站队列）。
//!
//! 与 OneBot 11 标准（及其主流实现 NapCat / LLOneBot / Lagrange）对齐，
//! 模块分工：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`event`] | 线上事件 DTO，容错反序列化（缺字段、数字/字符串双方言） |
//! | [`message`] | 消息段 ↔ 纯文本；出站消息构造（含 `reply` 段） |
//! | [`normalize`] | 事件 → [`hbot_core::domain::InboundMessage`] 所需的字段与幂等键 |
//! | [`client`] | 正向 WS 连接、心跳看门狗、重连、动作收发、探测交接（probe → handoff） |
//! | [`send`] | 出站串行队列 + 有界重试 |
//! | [`backoff`] | 指数退避 1/2/4…30s ±20% 抖动（可注入种子） |
//!
//! 典型装配（组合根负责，不在本模块里）：
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use hbot_adapters::platforms::onebot::{
//! #     client::OneBotClient, client::OneBotConfig, send::{OutboundSender, RetryPolicy},
//! # };
//! # async fn build() -> Result<(), hbot_adapters::AdapterError> {
//! let client = OneBotClient::start(OneBotConfig::new("ws://127.0.0.1:3001").with_token("t"))?;
//! let outbound = OutboundSender::start(
//!     Arc::new(client.action_handle()),
//!     RetryPolicy::default_for_send(),
//!     Arc::new(hbot_adapters::clock::SystemClock),
//!     1024,
//! )?;
//! # let _ = outbound;
//! # Ok(())
//! # }
//! ```

pub mod backoff;
pub mod client;
pub mod event;
pub mod message;
pub mod normalize;
pub mod send;

pub use backoff::{Backoff, Jitter, NoJitter, SeededJitter};
pub use client::{
    ActionHandle, ConnectedSession, LIFECYCLE_CONFIRM_WINDOW, OneBotClient, OneBotConfig,
    probe_connection, probe_session, validate_ws_url,
};
pub use event::{MessageType, OneBotEvent, PostType, Sender};
pub use message::{
    MAX_OUTBOUND_ATTACHMENT_BYTES, MessagePayload, SEGMENT_FILE, Segment, SegmentKind,
    attachment_segment, build_outbound, outbound_json, outbound_json_with_attachment,
};
pub use normalize::{
    NormalizedInbound, fallback_external_id, fallback_fingerprint, fnv1a_64,
    is_synthetic_external_id, normalize,
};
pub use send::{ActionSender, OutboundSender, RetryPolicy, SendAttempt, SendReceipt};
