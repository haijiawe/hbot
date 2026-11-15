//! `hbot-adapters`：平台适配器与受限 IO。
//!
//! 职责（`docs/03-directory.md`）：
//!
//! - **平台协议**：[`platforms`]（当前实现 OneBot 11）；
//! - **受限 HTTP**：[`http`]（超时、大小上限、拒绝内网，给工具层抓网页用）；
//! - **注入用的实现**：[`clock`]（只再导出 `hbot_core::SystemClock`；实现上移到
//!   `hbot-core` 是为了不让组合根为了一个时钟依赖整个适配器 crate）。
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
//! # TLS
//!
//! `wss://`（OneBot 正向 WS）与 `https://`（[`http_fetch`]）都走 rustls，
//! 共用 `tls` 模块里的同一份客户端配置，**默认校验对端证书**：信任 Mozilla
//! 打包的公共根，加上调用方通过 `OneBotConfig::with_extra_root_certificates` /
//! [`http::FetchOptions::extra_roots`] 显式追加的根证书（私有 CA / 自签部署）。
//! 校验失败一律报 [`AdapterError::Tls`]，**不会**退回明文、也没有"跳过校验"开关。
//!
//! # 已知边界
//!
//! - 只做服务端证书校验，不支持客户端证书（mTLS）——托管式 OneBot 不要求它。
//! - 未接真实 NapCat 实例做端到端联调；`tests/tls_sockets.rs` 用进程内生成的自签
//!   证书起真实 TLS 监听器（Handshake → 事件 → `send_group_msg` → 断线重连全套）。

#![warn(missing_docs)]

pub mod clock;
pub mod error;
pub mod http;
pub mod platforms;
// TLS 配置是内部实现细节：对外只通过 `OneBotConfig` / `FetchOptions` 暴露追加根证书。
mod tls;

pub use clock::SystemClock;
pub use error::AdapterError;
pub use http::{
    FetchOptions, HttpTarget, Scheme, http_fetch, http_fetch_with, is_blocked_ip, parse_target,
};
pub use platforms::onebot;
