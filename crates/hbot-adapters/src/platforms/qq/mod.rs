//! **QQ 官方机器人（QQ 开放平台）** 适配器。
//!
//! 和 OneBot 平台并列的第二个平台实现：OneBot 走「正向 WebSocket + 动作 JSON」，
//! QQ 官方走「HTTP OpenAPI（被动回复）+ WebSocket 网关（事件）」。
//!
//! ```text
//! bots.qq.com/app/getAppAccessToken   → access_token（缓存、单飞、提前续期）
//! api.sgroup.qq.com/gateway           → wss:// 网关地址
//! api.sgroup.qq.com/websocket/        → op:10 Hello → Identify/Resume → op:0 事件
//! api.sgroup.qq.com/v2/groups/{group_openid}/messages    ← 被动回复（群）
//! api.sgroup.qq.com/v2/users/{user_openid}/messages      ← 被动回复（单聊）
//! ```
//!
//! ```no_run
//! use hbot_adapters::qq::{QqClient, QqConfig};
//!
//! # async fn run() -> Result<(), hbot_adapters::AdapterError> {
//! let config = QqConfig::new("1024", "app-secret");
//! let mut client = QqClient::start(config)?;
//! while let Some(message) = client.next_event().await {
//!     // 被动回复：平台要求带上入站消息的 `msg_id`（就是事件的 `d.id`）。
//!     let _ = client.reply(&message, "收到").await;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # 文件分工
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `config.rs` | [`QqConfig`]：端点、intents、shard、超时、容量；Debug 脱敏 |
//! | `event.rs` | 网关帧解析（[`GatewayFrame`]）、op / 事件名常量、Identify/Resume/Heartbeat 载荷 |
//! | `normalize.rs` | 网关事件 → 共用 [`NormalizedInbound`]，`<@…>` 剥离，`auth_token` 脱敏 |
//! | `auth.rs` | token 缓存、单飞、提前续期（[`AccessToken`]、[`probe_access_token`]） |
//! | `api.rs` | OpenAPI 调用：`Authorization: QQBot …`、401/403 续期重试一次 |
//! | `send.rs` | 被动回复（[`QqSender`]）：`msg_id` + 递增 `msg_seq`；主动推送明确拒绝 |
//! | `client.rs` | 网关客户端（[`QqClient`]）：连接、心跳看门狗、重连、事件分发 |
//! | `http.rs` | 最小 HTTP/1.1 JSON 客户端（QQ 需要 POST，`crate::http` 是只读 GET） |
//!
//! # 和 OneBot 的会话命名空间**不通用**（重要）
//!
//! | | OneBot | QQ 官方 |
//! |---|---|---|
//! | 群 | `group_id`（QQ 群号，数字） | `group_openid`（32 位 HEX 串） |
//! | 单聊 | `user_id`（QQ 号，数字） | `user_openid`（32 位 HEX 串） |
//!
//! `openid` **不是 QQ 号**，是机器人维度下的不透明 ID：同一个真人在两个机器人下的
//! `openid` 不同，也没有任何 API 能把 `openid` 换回 QQ 号。所以：
//!
//! - 落库里 QQ 官方的会话与 OneBot 的会话是**两套**，不能互相认；
//! - 别拿 `openid` 去和 OneBot 的 `group_id`/`user_id` 比较或拼接；
//! - 需要跨平台识别同一个人时，只能用业务侧的显式绑定，不能指望 ID 相等。
//!
//! # 实测与「未核实」
//!
//! **实测**（2026-11-16，真凭据 + 真网关，见任务报告）：
//!
//! - `getAppAccessToken` 返回 `access_token`（长度 78）与 `expires_in`（6627，是**剩余**
//!   有效时间而不是固定 7200，所以续期必须按它算）；
//! - `/gateway` 返回 `wss://api.sgroup.qq.com/websocket/`；
//! - Hello 的 `d.heartbeat_interval` 实测 **41250ms**（不是文档里常见的 45000，禁止写死）；
//! - `intents = 33554432`（`1 << 25`）被接受，收到 `READY`；
//! - 群消息事件名实测是 `GROUP_MESSAGE_CREATE`（不是文档里的 `GROUP_AT_MESSAGE_CREATE`，
//!   两个名字本模块都认）；`@` 标记形如 `<@196AAC0A1E7A06AC0D5EEB7FCF778303>`；
//! - 被动回复 `POST /v2/{groups,users}/{openid}/messages` 可用，必须带 `msg_id`。
//!
//! **未核实**（代码里按「按协议/实测形状写，但不假装确定」处理）：
//!
//! 1. `intents` 各位的含义与权限：只确认 `1 << 25`（群 + C2C 消息）能收到两类消息事件，
//!    其它位（频道 `GUILDS`、私域等）需要单独申请权限，声明了没权限的位会被平台拒绝
//!    （close code 4914/4915 之类的说法**未核实**）。所以默认只开 `1 << 25`，扩位由
//!    [`QqConfig::with_intents`] 显式指定。
//! 2. 频道 `AT_MESSAGE_CREATE` 的字段形状（`channel_id`/`guild_id`/`member` 等）**未核实**，
//!    归一化里按「有 `channel_id` 就当 Channel 会话」尽力而为。
//! 3. 限流行为：429 的具体阈值、`X-RateLimit-*` 响应头、`retry_after` 语义**未核实**。
//!    本模块只把 429 映射成 [`crate::AdapterError::Limit`]，不做自动重试（避免放大限流）。
//! 4. 被动回复的有效期与次数上限：平台有「回复必须在若干分钟内 / 同一 `msg_id` 最多几条」的
//!    限制，**具体数字未核实**。`msg_seq` 表因此做成有界 LRU（默认 512 条）。
//! 5. Hello 之前是否可能先收到别的帧、`op:9` 之后是否需要 `op:2` 之外的字段**未核实**。
//! 6. 主动推送（不带 `msg_id`）的限额**未核实**，因此本适配器**不实现**它：
//!    见 [`QqSender::send_group_active`] / [`QqSender::send_c2c_active`]，恒返回
//!    [`crate::AdapterError::Unsupported`]，不伪造成功。

pub mod client;
pub mod config;
pub mod event;
pub mod normalize;
pub mod send;

mod api;
mod auth;
mod http;

pub use client::{QqClient, validate_ws_url};
pub use config::{
    DEFAULT_API_BASE, DEFAULT_CONNECT_TIMEOUT, DEFAULT_EVENT_CAPACITY, DEFAULT_GATEWAY_PATH,
    DEFAULT_INTENTS, DEFAULT_MSG_SEQ_CAPACITY, DEFAULT_REQUEST_TIMEOUT, DEFAULT_SHARD,
    DEFAULT_TOKEN_REFRESH_MARGIN, DEFAULT_TOKEN_URL, ENV_APP_ID, ENV_APP_SECRET, QqConfig,
};
pub use event::{
    EVENT_AT_MESSAGE_CREATE, EVENT_C2C_MESSAGE_CREATE, EVENT_GROUP_AT_MESSAGE_CREATE,
    EVENT_GROUP_MESSAGE_CREATE, EVENT_READY, EVENT_RESUMED, GatewayFrame, OP_DISPATCH,
    OP_HEARTBEAT, OP_HEARTBEAT_ACK, OP_HELLO, OP_IDENTIFY, OP_INVALID_SESSION, OP_RECONNECT,
    OP_RESUME,
};
pub use normalize::{is_bot_mentioned, normalize, strip_at_mentions};
pub use send::{QqSender, SendReceipt};

pub use crate::normalize::NormalizedInbound;

pub use auth::{AccessToken, TokenProbe, probe_access_token};

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_are_usable() {
        // 钉住 re-export 面：外部用户只 `use hbot_adapters::qq::*` 就能到位。
        let config = super::QqConfig::new("1024", "secret");
        assert_eq!(config.intents(), super::DEFAULT_INTENTS);
        assert_eq!(config.shard(), super::DEFAULT_SHARD);
        assert_eq!(super::OP_HELLO, 10);
        assert_eq!(super::OP_HEARTBEAT_ACK, 11);
        assert_eq!(super::EVENT_GROUP_MESSAGE_CREATE, "GROUP_MESSAGE_CREATE");
    }
}
