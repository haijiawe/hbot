//! 平台适配器实现。
//!
//! 每个平台一个模块，只负责"平台协议 ↔ 归一化消息"的翻译，不含业务编排
//! （`docs/03-directory.md`：适配器不承载 supervisor/队列）。新增平台的步骤见
//! `AGENTS.md` §8。
//!
//! 平台间**共用**的东西不放在平台模块里：
//!
//! - [`crate::normalize::NormalizedInbound`]：归一化入站消息（落库/去重/生成代码
//!   的公共出口）；
//! - [`crate::error::AdapterError`]：错误；
//! - [`crate::tls`]：`wss://` / `https://` 的同一份 rustls 客户端配置。
//!
//! | 平台 | 模块 | 传输 | 说明 |
//! |---|---|---|---|
//! | OneBot 11（NapCat / LLOneBot / Lagrange） | [`onebot`] | 正向 WebSocket | 社区实现，群号/QQ 号即会话 ID |
//! | QQ 官方机器人（QQ 开放平台） | [`qq`] | 官方 WS 网关 + HTTPS OpenAPI | 会话 ID 是 `group_openid` / `user_openid`，**不是 QQ 号** |
//! | 本地试跑 | [`local`] | **无**（不联网） | 入站靠 `bot_inject`，出站回复只写 run log；给 IDE 试跑用，不用于生产 |

pub mod local;
pub mod onebot;
pub mod qq;
