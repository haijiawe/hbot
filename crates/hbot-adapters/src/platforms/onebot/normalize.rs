//! OneBot 事件 → 平台无关的入站消息。
//!
//! 归一化只做「协议 → 领域」的形状转换，不碰数据库、不碰队列：
//!
//! ```text
//! OneBotEvent (协议方言)
//!   └─ normalize() ─→ NormalizedInbound
//!        ├─ external_id          幂等键（(bot_id, external_id) UNIQUE）
//!        ├─ conversation_external_id + kind
//!        ├─ sender_id / text / reply_to
//!        └─ raw (原始事件 JSON，落 inbound.payload 供排查)
//! ```
//!
//! # 幂等键（external_id）方案
//!
//! 不变量：`inbound` 表 `UNIQUE(bot_id, external_id)`，重复投递只处理后一次
//! （`docs/02-architecture.md` §4.1）。键的取值分两条路：
//!
//! 1. **有 `message_id`（首选）**：`external_id = message_id.to_string()`。
//!    这是平台给的单调 ID，天然唯一、重发也不变，是唯一推荐的去重键。
//!
//! 2. **缺 `message_id`（兜底）**：`"{user_id}-{time}-{fnv1a64:016x}"`，
//!    其中 FNV-1a 覆盖**整条事件的规范化 JSON**（`serde_json::Value` 重新序列化后的
//!    字节序；`Value::Object` 在本 workspace 是 `BTreeMap`，键序稳定）。
//!    - 为什么不是只哈希 `raw_message`：那样 `user_id` + 平台秒级 `time` + 文本
//!      三者相同的两条消息会得到同一个键，而 `UNIQUE(bot_id, external_id)` 会把
//!      **后一条当成重复丢掉**（丢消息，不是重复执行）。QQ 允许同一人同一秒发两条
//!      相同文本，这不是理论风险 —— 2026-11 的复查里就是按这条复现的。
//!    - 为什么整条 JSON 仍然满足"重投递要能去重"：同一事件的重投是**同一份
//!      JSON**，规范化后逐字节相同 ⇒ 同一个键。而"同人同秒同文本"的两条不同消息
//!      在 `message_seq` / `real_id` / 段数组 / `font` 等字段上必然不同（平台没有
//!      两个字段都不给的可能）⇒ 键不同。
//!    - **残余限制（必须知道）**：若某实现连 `message_seq`/`real_id` 都不给，且两帧
//!      JSON 逐字节相同，则无法区分 —— 此时会按重复丢弃。这是刻意选择的失败方向：
//!      重复处理一条消息的代价（一次多余回复）远小于静默丢一条用户消息。真遇到
//!      这种实现，正解是让它发 `message_id`，而不是在这里做启发式猜测。
//!    - 为什么不用 sha256/sha1/blake3：本 crate 的依赖里没有哈希库，
//!      为兜底路径引入新依赖（以及它的供应链）不划算；FNV-1a 8 字节足够
//!      区分"同一个人的相邻消息"（这里不是抗攻击场景，是防误撞）。
//!
//! 兜底键的值总是含 `-`，数字 `message_id` 不含，所以两条路径的键空间不会互相
//! 误撞（这是个小但重要的性质，测试里有断言）。

use serde_json::{Value, json};

use hbot_core::domain::{ConversationKind, InboundMessage, Timestamp};
use hbot_core::ids::{BotId, ConversationId, MessageId};

use super::event::{MessageType, OneBotEvent};
use crate::error::AdapterError;

/// FNV-1a 64 位偏移量（RFC 无编号，取自 FNV 规范）。
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 位质数。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 8 字节 FNV-1a 哈希。
///
/// 只用于缺 `message_id` 时的幂等键兜底，**不是**加密哈希：
/// 不抗碰撞、不抗碰撞攻击、不用于签名或安全判定。
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 缺 `message_id` 时的兜底幂等键。
///
/// 形如 `"123456-1700000000-1f2e3d4c5b6a7980"`；`fingerprint` 见
/// [`fallback_fingerprint`]，碰撞与重投递的性质见模块级说明。
pub fn fallback_external_id(user_id: i64, time_seconds: i64, fingerprint: &str) -> String {
    let hash = fnv1a_64(fingerprint.as_bytes());
    format!("{user_id}-{time_seconds}-{hash:016x}")
}

/// 兜底幂等键的哈希输入：整条事件的规范化 JSON。
///
/// 两个性质都是被测钉住的（`tests/onebot_normalize.rs`）：
///
/// - **重投递稳定**：同一事件的同一份 JSON，`serde_json::Value` 重新序列化后
///   字节序固定（对象是 `BTreeMap`，键序与解析顺序无关）⇒ 同一个键。
/// - **不同到达不撞**：同人同秒同文本的两条不同消息，在 `message_seq` /
///   `real_id` / 段数组等字段上不同 ⇒ 不同的键（旧实现只哈希 `raw_message`，
///   这两条会塌成同一个键、后一条被 `UNIQUE` 静默丢掉）。
///
/// `raw` 为 `Value::Null`（调用方没给原始 JSON）时退回 `raw_message`，再退回渲染
/// 后的文本：保持对老调用方的兼容，这时行为与旧实现一致（有损，但不会 panic）。
pub fn fallback_fingerprint(raw: &Value, raw_message: Option<&str>, text: &str) -> String {
    match raw {
        Value::Null => raw_message.unwrap_or(text).to_string(),
        // `to_string` 对 `Value` 不会失败（JSON 一定能序列化回字符串），
        // 但仍按 Result 处理：万一失败就退回文本，绝不 panic（兜底路径不求完美）。
        value => {
            serde_json::to_string(value).unwrap_or_else(|_| raw_message.unwrap_or(text).to_string())
        }
    }
}

/// 归一化后的入站消息：平台无关，交给 supervisor 落库 + 入队。
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedInbound {
    /// 机器人自己的平台 ID（事件里的 `self_id`）；平台没给时为 `None`。
    ///
    /// 落库时用配置里的 bot_id（UUID），这里保留平台 ID 便于排查/发送定位。
    pub bot_platform_id: Option<i64>,
    /// 幂等键（半个去重键），见模块级说明。
    pub external_id: String,
    /// 平台侧会话标识：群聊是群号，私聊是对方 QQ 号。
    pub conversation_external_id: String,
    /// 会话类型。
    pub kind: ConversationKind,
    /// 发送者平台 ID（字符串形态，落 `inbound.sender_id`）。
    pub sender_id: String,
    /// 有损纯文本（见 `message.rs` 的有损表）。
    pub text: String,
    /// 被动回复应引用的平台消息 ID（`reply` 段），用于回填 `reply_to`。
    pub reply_to: Option<String>,
    /// 平台消息 ID（缺失时为 `None`；此时 `external_id` 是兜底键）。
    pub platform_message_id: Option<i64>,
    /// 原始事件 JSON（不透明，落 `inbound.payload_json` 便于排查）。
    pub raw: Value,
    /// 收到时间。
    pub received_at: Timestamp,
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
pub fn conversation_kind_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Private => "private",
        ConversationKind::Group => "group",
        ConversationKind::Channel => "channel",
        ConversationKind::Unknown => "unknown",
    }
}

/// 归一化一条消息事件。
///
/// 只接受 `post_type == message`；notice/request/meta_event 返回
/// [`AdapterError::Protocol`]（它们不进消息队列，由调用方按需处理）。
///
/// `received_at` 由调用方注入（`hbot_core::Clock`），不在这里读系统时间，
/// 这样测试可以固定时间、重放也稳定。
pub fn normalize(
    event: &OneBotEvent,
    raw: Value,
    received_at: Timestamp,
) -> Result<NormalizedInbound, AdapterError> {
    if !event.is_message() {
        return Err(AdapterError::protocol(format!(
            "not a message event: post_type={}",
            event.post_type.as_str()
        )));
    }
    if event.user_id <= 0 {
        // user_id 缺失或坏掉时 DTO 给 0；这里必须拦，不能把 0 当成真用户。
        return Err(AdapterError::protocol(
            "message event without usable user_id (missing or non-numeric)",
        ));
    }
    let sender_id = event.user_id.to_string();
    let (kind, conversation_external_id) = conversation_of(event)?;
    let text = event.plain_text();
    let reply_to = event.reply_to();
    let time_seconds = event.time.unwrap_or(0);
    let external_id = match event.message_id {
        Some(message_id) => message_id.to_string(),
        None => fallback_external_id(
            event.user_id,
            time_seconds,
            // 兜底哈希覆盖**整条事件**（见 fallback_fingerprint 的注释）：
            // 只哈希 raw_message 会让"同人同秒同文本"的两条消息撞成同一个键。
            &fallback_fingerprint(&raw, event.raw_message.as_deref(), text.as_str()),
        ),
    };
    Ok(NormalizedInbound {
        bot_platform_id: event.self_id,
        external_id,
        conversation_external_id,
        kind,
        sender_id,
        text,
        reply_to,
        platform_message_id: event.message_id,
        raw,
        received_at,
    })
}

/// 由 `message_type` 决定会话类型与会话外部 ID。
///
/// - `group`：必须有 `group_id`，会话 ID 是群号；
/// - `private`：会话 ID 是对方 QQ 号；
/// - 未知/缺失：若带 `group_id` 就按群处理（部分实现漏 `message_type`），
///   否则按私聊处理但类型记 `Unknown`，交给上层展示兜底。
fn conversation_of(event: &OneBotEvent) -> Result<(ConversationKind, String), AdapterError> {
    match event.message_type.unwrap_or(MessageType::Unknown) {
        MessageType::Group => {
            let group_id = event.group_id.filter(|id| *id > 0).ok_or_else(|| {
                AdapterError::protocol("group message event without usable group_id")
            })?;
            Ok((ConversationKind::Group, group_id.to_string()))
        }
        MessageType::Private => Ok((ConversationKind::Private, event.user_id.to_string())),
        MessageType::Unknown => match event.group_id.filter(|id| *id > 0) {
            Some(group_id) => Ok((ConversationKind::Group, group_id.to_string())),
            None => Ok((ConversationKind::Unknown, event.user_id.to_string())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::onebot::event::OneBotEvent;

    fn parse(value: Value) -> OneBotEvent {
        serde_json::from_value(value).expect("fixture parses")
    }

    #[test]
    fn fnv1a_matches_known_vectors() {
        // FNV-1a 64 的标准测试向量。
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn group_message_normalizes() {
        let event = parse(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "message_id": 77,
            "user_id": 42,
            "group_id": 1001,
            "self_id": 9,
            "time": 1700000000,
            "raw_message": "hi",
            "message": [{ "type": "text", "data": { "text": "hi" } }]
        }));
        let normalized = normalize(
            &event,
            serde_json::json!({}),
            Timestamp::from_unix_millis(1),
        )
        .expect("normalizes");
        assert_eq!(normalized.external_id, "77");
        assert_eq!(normalized.conversation_external_id, "1001");
        assert_eq!(normalized.kind, ConversationKind::Group);
        assert_eq!(normalized.sender_id, "42");
        assert_eq!(normalized.text, "hi");
        assert_eq!(normalized.bot_platform_id, Some(9));
    }

    #[test]
    fn fallback_key_is_stable_and_distinct() {
        let base = serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 42,
            "time": 1700000000,
            "raw_message": "same"
        });
        let first =
            normalize(&parse(base.clone()), Value::Null, Timestamp::default()).expect("normalizes");
        let again =
            normalize(&parse(base.clone()), Value::Null, Timestamp::default()).expect("normalizes");
        assert_eq!(
            first.external_id, again.external_id,
            "same event → same key"
        );

        let mut other = base.clone();
        other["raw_message"] = serde_json::json!("different");
        let changed =
            normalize(&parse(other), Value::Null, Timestamp::default()).expect("normalizes");
        assert_ne!(first.external_id, changed.external_id);

        // 兜底键含 '-'，数字 message_id 不含：两条键空间不会误撞。
        assert!(first.external_id.contains('-'));
        assert!(!first.external_id.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn fallback_key_uses_whole_event_so_same_second_arrivals_do_not_collide() {
        // 缺 message_id 的两种真实形态：同一人同一秒、同样文本，只有 `message_seq`
        // 不同（NapCat / go-cqhttp 的群消息就带这个字段）。
        let arrival = serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 42,
            "time": 1700000000,
            "raw_message": "same",
            "font": 0,
            "message_seq": 100
        });
        let first = normalize(
            &parse(arrival.clone()),
            arrival.clone(),
            Timestamp::default(),
        )
        .expect("normalizes");
        // 重投递：同一份 JSON（哪怕接收时间不同）→ 同一个键，去重仍然生效。
        let redelivery = normalize(
            &parse(arrival.clone()),
            arrival.clone(),
            Timestamp::default(),
        )
        .expect("normalizes");
        assert_eq!(
            first.external_id, redelivery.external_id,
            "same raw event must dedupe on re-delivery"
        );

        // 第二条真实消息：文本、用户、秒全都一样，只有 seq 不同。
        let mut next_arrival = arrival.clone();
        next_arrival["message_seq"] = serde_json::json!(101);
        let second = normalize(
            &parse(next_arrival.clone()),
            next_arrival,
            Timestamp::default(),
        )
        .expect("normalizes");
        assert_ne!(
            first.external_id, second.external_id,
            "distinct arrivals in the same second must not collapse into one key"
        );

        // 键序无关：`Value` 重新序列化走 BTreeMap，字段顺序不影响指纹。
        let reordered: Value = serde_json::from_str(
            r#"{"message_seq":100,"font":0,"raw_message":"same","time":1700000000,
                "user_id":42,"message_type":"private","post_type":"message"}"#,
        )
        .expect("reordered json parses");
        assert_eq!(
            fallback_fingerprint(&reordered, Some("same"), "same"),
            fallback_fingerprint(&arrival, Some("same"), "same"),
            "field order must not change the fingerprint"
        );
    }
}
