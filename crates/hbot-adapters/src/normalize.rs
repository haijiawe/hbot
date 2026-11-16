//! 平台无关的归一化入站消息（所有平台适配器共用）。
//!
//! 这个类型原本定义在 `platforms::onebot::normalize`（当时只有 OneBot 一个平台）。
//! 2026-11 新增 QQ 官方机器人适配器时**移动**到这里，而不是复制第二份：
//! `NormalizedInbound` 是「平台协议 → 领域入站消息」的公共出口，字段一分成两份，
//! 落库、去重键与生成代码里 `ctx.*` 的取值就会跟着分叉。
//!
//! 与平台强相关的部分仍留在各自模块里：
//!
//! | 平台 | 幂等键（`external_id`）方案 | 位置 |
//! |---|---|---|
//! | OneBot 11 | `message_id`；缺失时 `{user_id}-{time}-{fnv1a64(raw)}` | `platforms::onebot::normalize` |
//! | QQ 官方机器人 | 事件 `d.id`（官方消息 ID，重投递不变） | `platforms::qq::normalize` |
//!
//! **不要**在这里加平台方言字段（QQ 的 `group_openid`、OneBot 的 `message_seq`…），
//! 那些属于各平台的 DTO / `raw`，塞进来会让另一个平台的构造点全部要改。
//!
//! 字段语义（与 `inbound` 表列一一对应）：
//!
//! ```text
//! NormalizedInbound
//!   ├─ external_id                 幂等键（(bot_id, external_id) UNIQUE）
//!   ├─ conversation_external_id + kind
//!   ├─ sender_id / text / reply_to
//!   └─ raw                         原始事件 JSON，落 inbound.payload 供排查
//! ```

use std::fmt;

use serde_json::{Value, json};

use hbot_core::domain::{ConversationKind, InboundMessage, Timestamp};
use hbot_core::ids::{BotId, ConversationId, MessageId};
use hbot_core::redact::{RedactedJson, redact_arg};

/// 归一化后的入站消息：平台无关，交给 supervisor 落库 + 入队。
#[derive(Clone, PartialEq)]
pub struct NormalizedInbound {
    /// 机器人自己的平台 ID（OneBot 的 `self_id`）；平台没给时为 `None`。
    ///
    /// 落库时用配置里的 bot_id（UUID），这里保留平台 ID 便于排查/发送定位。
    /// QQ 官方平台不下发数字形态的机器人 ID（`READY` 里是 appId 之外的 openid），
    /// 所以 QQ 侧恒为 `None`。
    pub bot_platform_id: Option<i64>,
    /// 幂等键（半个去重键），方案见模块级表格。
    pub external_id: String,
    /// 平台侧会话标识。
    ///
    /// - OneBot：群聊是群号，私聊是对方 QQ 号；
    /// - QQ 官方机器人：**`group_openid` / `user_openid`，不是 QQ 号**（见
    ///   `platforms::qq::normalize` 的模块说明）。
    pub conversation_external_id: String,
    /// 会话类型。
    pub kind: ConversationKind,
    /// 发送者平台 ID（字符串形态，落 `inbound.sender_id`）。
    pub sender_id: String,
    /// 有损纯文本（OneBot 见 `platforms::onebot::message` 的有损表；
    /// QQ 官方平台只做 @ 前缀剥离，见 `platforms::qq::normalize`）。
    pub text: String,
    /// 被动回复应引用的平台消息 ID（OneBot 的 `reply` 段；QQ 的 `d.id`）。
    pub reply_to: Option<String>,
    /// 平台消息 ID 的**数字**形态（缺失时为 `None`，此时 `external_id` 是兜底键）。
    ///
    /// 只有 OneBot 系实现保证它是数字。QQ 官方平台的消息 ID 是不透明字符串，
    /// 因此 QQ 侧恒为 `None`，原值放在 [`NormalizedInbound::external_id`] 与
    /// [`NormalizedInbound::reply_to`] 里（都是 `String`，不会丢信息）。
    pub platform_message_id: Option<i64>,
    /// 原始事件 JSON（不透明，落 `inbound.payload_json` 便于排查）。
    pub raw: Value,
    /// 收到时间。
    pub received_at: Timestamp,
}

/// 手写 `Debug`（**不要**换回 `#[derive(Debug)]`）：RT-06。
///
/// `raw` 是平台原始事件，凭据就在里面：QQ 官方的 `message_scene.ext` 带
/// `auth_token=…`，某些 OneBot 实现会把整个 HTTP 请求头（含 `Authorization`）
/// 塞进事件。派生实现会在适配器日志与"归一化失败"的错误链里把它打出来。
///
/// `raw` 走 [`RedactedJson`]（递归、按键名判定），`text` / 各 ID 过
/// [`redact_arg`]（正常文本与 ID 原样保留，只有嵌了 `token=…` 才抹）——
/// 不是整段 `<redacted>`，否则排查入站事件就没有依据了。
impl fmt::Debug for NormalizedInbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NormalizedInbound")
            .field("bot_platform_id", &self.bot_platform_id)
            .field("external_id", &redact_arg(&self.external_id))
            .field(
                "conversation_external_id",
                &redact_arg(&self.conversation_external_id),
            )
            .field("kind", &self.kind)
            .field("sender_id", &redact_arg(&self.sender_id))
            .field("text", &redact_arg(&self.text))
            .field("reply_to", &self.reply_to.as_deref().map(redact_arg))
            .field("platform_message_id", &self.platform_message_id)
            .field("raw", &RedactedJson(&self.raw))
            .field("received_at", &self.received_at)
            .finish()
    }
}

impl NormalizedInbound {
    /// 落 `inbound.payload_json` 的结构化载荷。
    ///
    /// 生成代码的 `ctx.text` / `ctx.senderId` / `ctx.replyTo` 从这里取；`raw` 原样
    /// 嵌在 `raw` 字段下，避免把平台字段与运行时字段混在同一层。
    pub fn payload_json(&self) -> Value {
        json!({
            "text": self.text,
            "senderId": self.sender_id,
            "conversationExternalId": self.conversation_external_id,
            "kind": conversation_kind_str(self.kind),
            "messageId": self.platform_message_id,
            "replyTo": self.reply_to,
            "botPlatformId": self.bot_platform_id,
            "raw": self.raw,
        })
    }

    /// 组装成 `hbot_core::domain::InboundMessage`（`id` / `conversation_id` / `bot_id`
    /// 由调用方从库里取，适配器不生成 UUID）。
    pub fn to_inbound(
        &self,
        id: MessageId,
        bot_id: BotId,
        conversation_id: ConversationId,
    ) -> InboundMessage {
        InboundMessage {
            id,
            bot_id,
            conversation_id,
            external_id: self.external_id.clone(),
            sender_id: self.sender_id.clone(),
            payload: self.payload_json(),
            received_at: self.received_at,
            processed_at: None,
        }
    }
}

/// `ConversationKind` → 稳定的 snake_case 字符串（与 `hbot-core` 的 serde 表示一致）。
///
/// 这是**唯一一份**适配器侧的映射：`hbot-rpc` 里另有一份用于渲染的
/// `conversation_kind_str`（`docs` 注明了"取值一致"），新平台适配器一律复用本函数，
/// 不要再写第二份/第三份表。
pub fn conversation_kind_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Private => "private",
        ConversationKind::Group => "group",
        ConversationKind::Channel => "channel",
        ConversationKind::Unknown => "unknown",
    }
}
