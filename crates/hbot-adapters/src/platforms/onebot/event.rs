//! OneBot 11 事件 DTO。
//!
//! 覆盖 `post_type` 的四个大类：`message` / `notice` / `request` / `meta_event`；
//! 消息事件再按 `message_type` 分 `private` / `group`。消息主链路只消费
//! `message` 事件，其余三类解析出来供日志与后续扩展使用。
//!
//! # 为什么要容错反序列化
//!
//! OneBot 11 是「实现之间的方言」：同一字段在不同实现里有不同形态，且历史上
//! 出过这些问题（都见过真实实例）：
//!
//! | 字段 | 方言 | 处理 |
//! |---|---|---|
//! | `message_id` | **可能缺失**（部分实现对某些事件不给） | [`OneBotEvent::message_id`] 是 `Option<i64>`，缺失走幂等键兜底（见 `normalize.rs`） |
//! | `user_id` / `group_id` / `target_id` | 数字**或字符串**（`"123"`） | 自定义 deserializer 接受 `123`、`"123"`、`" 123 "` |
//! | `message` | 段数组**或**纯字符串 | [`MessagePayload`] 的 `#[serde(untagged)]` 两种都收 |
//! | `post_type` / `message_type` | 可能出现未知值/新值 | 容忍枚举，未知归 `Unknown`，不因一个事件把连接搞崩 |
//! | `sender` | 可能整体缺失 | `Option<Sender>` |
//!
//! 容错的底线：**不 panic、不 `unwrap`**；单个字段坏了只影响该字段，
//! 调用方在归一化阶段（`normalize.rs`）对必需字段做校验并报错。

use serde::de::{Deserializer, Error as DeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::message::MessagePayload;

/// `post_type`：事件大类。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PostType {
    /// `message`：消息事件（主链路消费）。
    Message,
    /// `notice`：通知事件（进群、撤回、戳一戳等）。
    Notice,
    /// `request`：请求事件（加好友、加群）。
    Request,
    /// `meta_event`：元事件（生命周期、心跳）。
    MetaEvent,
    /// 未知/缺失的 `post_type`。
    #[default]
    Unknown,
}

impl PostType {
    /// 由线上的 `post_type` 值解析，未知值归 [`PostType::Unknown`]。
    pub fn from_wire(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "message" => Self::Message,
            "notice" => Self::Notice,
            "request" => Self::Request,
            "meta_event" => Self::MetaEvent,
            _ => Self::Unknown,
        }
    }

    /// 线上的 `post_type` 字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Notice => "notice",
            Self::Request => "request",
            Self::MetaEvent => "meta_event",
            Self::Unknown => "unknown",
        }
    }
}

impl<'de> Deserialize<'de> for PostType {
    /// 容忍非字符串值（例如某些实现的空 `post_type`）：无法识别都归 `Unknown`。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<Value>::deserialize(deserializer)?;
        Ok(match value.as_ref().and_then(Value::as_str) {
            Some(raw) => Self::from_wire(raw),
            None => Self::Unknown,
        })
    }
}

impl Serialize for PostType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// `message_type`：消息事件细分。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MessageType {
    /// `private`：私聊。
    Private,
    /// `group`：群聊。
    Group,
    /// 未知/缺失的 `message_type`。
    #[default]
    Unknown,
}

impl MessageType {
    /// 由线上的 `message_type` 值解析，未知值归 [`MessageType::Unknown`]。
    pub fn from_wire(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "private" => Self::Private,
            "group" => Self::Group,
            _ => Self::Unknown,
        }
    }

    /// 线上的 `message_type` 字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Unknown => "unknown",
        }
    }
}

impl<'de> Deserialize<'de> for MessageType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<Value>::deserialize(deserializer)?;
        Ok(match value.as_ref().and_then(Value::as_str) {
            Some(raw) => Self::from_wire(raw),
            None => Self::Unknown,
        })
    }
}

impl Serialize for MessageType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// 发送者信息（`sender` 字段）。
///
/// 三个字段都可能缺失或是字符串形态；`nickname` / `card` 为空串时保留为 `Some("")`，
/// 展示层自行兜底，不在这里做业务判断。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    /// 发送者 QQ 号（数字或字符串方言；缺失为 0，由归一化阶段校验）。
    #[serde(default, deserialize_with = "de_i64_tolerant")]
    pub user_id: i64,
    /// 昵称。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub nickname: Option<String>,
    /// 群名片（群消息才有意义）。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub card: Option<String>,
}

/// 一个 OneBot 11 事件（四个大类的并集）。
///
/// 所有字段都 `#[serde(default)]`：缺失字段取默认值，不让一个字段缺失导致
/// 整条事件解析失败——解析失败意味着连接层要断开重连，代价远大于字段降级。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OneBotEvent {
    /// 事件大类。
    #[serde(default)]
    pub post_type: PostType,
    /// 平台侧事件时间（Unix 秒）。
    #[serde(default, deserialize_with = "de_opt_i64_tolerant")]
    pub time: Option<i64>,
    /// 机器人自己的 QQ 号（`self_id`）。
    #[serde(default, deserialize_with = "de_opt_i64_tolerant")]
    pub self_id: Option<i64>,
    /// 消息类型（仅 `message` 事件有意义）。
    #[serde(default)]
    pub message_type: Option<MessageType>,
    /// 消息子类型（`friend` / `group` / `normal` / `anonymous` …），保持字符串。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub sub_type: Option<String>,
    /// 平台消息 ID；**可能缺失**，缺失时幂等键走兜底（见 `normalize.rs`）。
    #[serde(default, deserialize_with = "de_opt_i64_tolerant")]
    pub message_id: Option<i64>,
    /// 发送者 QQ 号（数字或字符串方言；缺失为 0）。
    #[serde(default, deserialize_with = "de_i64_tolerant")]
    pub user_id: i64,
    /// 群号（群消息才有）。
    #[serde(default, deserialize_with = "de_opt_i64_tolerant")]
    pub group_id: Option<i64>,
    /// 平台给的原始消息文本（CQ 码字符串，**可能与 `message` 不一致**）。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub raw_message: Option<String>,
    /// 消息内容：段数组或纯字符串。
    #[serde(default)]
    pub message: Option<MessagePayload>,
    /// 发送者信息。
    #[serde(default)]
    pub sender: Option<Sender>,
    /// 通知类型（`notice_type`）。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub notice_type: Option<String>,
    /// 请求类型（`request_type`）。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub request_type: Option<String>,
    /// 元事件类型（`meta_event_type`：`lifecycle` / `heartbeat`）。
    #[serde(default, deserialize_with = "de_opt_string_tolerant")]
    pub meta_event_type: Option<String>,
    /// 被操作者 QQ 号（`notice` 事件等）。
    #[serde(default, deserialize_with = "de_opt_i64_tolerant")]
    pub target_id: Option<i64>,
}

impl OneBotEvent {
    /// 是否消息事件（主链路唯一消费的事件类别）。
    pub fn is_message(&self) -> bool {
        self.post_type == PostType::Message
    }

    /// 是否心跳/生命周期相关元事件（用于刷新心跳看门狗）。
    pub fn is_heartbeat_like(&self) -> bool {
        if self.post_type != PostType::MetaEvent {
            return false;
        }
        matches!(
            self.meta_event_type.as_deref().map(str::trim),
            Some("heartbeat" | "lifecycle")
        )
    }

    /// 有损纯文本；`message` 缺失时退回 `raw_message`（CQ 码原文，见模块级说明）。
    pub fn plain_text(&self) -> String {
        match &self.message {
            Some(payload) => payload.to_plain_text(),
            None => self.raw_message.clone().unwrap_or_default(),
        }
    }

    /// 引用回复的目标 `message_id`（`reply` 段）。
    pub fn reply_to(&self) -> Option<String> {
        self.message.as_ref().and_then(MessagePayload::reply_to)
    }

    /// 平台事件时间（Unix 秒），缺失为 `None`。
    pub fn event_time_seconds(&self) -> Option<i64> {
        self.time
    }
}

/// 容错读取整数：接受数字、字符串（可带空白）、以及 `1.0` 这种浮点写法。
///
/// 解析不了返回 `None` 而不是报错——坏字段不应该让整条事件（乃至连接）失败。
fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| text.trim().parse::<f64>().ok().map(|float| float as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// `user_id` 这类必需但可能残缺的整数：缺失/坏了给 0，由归一化阶段校验。
fn de_i64_tolerant<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = Option::<Value>::deserialize(deserializer).map_err(D::Error::custom)?;
    Ok(value.as_ref().and_then(value_to_i64).unwrap_or(0))
}

/// 可选整数字段（`message_id` / `group_id` / `self_id` …）：缺失或坏了给 `None`。
fn de_opt_i64_tolerant<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer).map_err(D::Error::custom)?;
    Ok(value.as_ref().and_then(value_to_i64))
}

/// 可选字符串字段：数字/布尔会被转成字符串，`null`/对象给 `None`。
fn de_opt_string_tolerant<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer).map_err(D::Error::custom)?;
    Ok(match value {
        Some(Value::String(text)) => Some(text),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::Bool(flag)) => Some(flag.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerant_ints_accept_numbers_strings_and_whitespace() {
        let event: OneBotEvent = serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "user_id": " 123 ",
            "group_id": 987,
            "message_id": "456",
            "self_id": 10000
        }))
        .expect("event parses");
        assert_eq!(event.user_id, 123);
        assert_eq!(event.group_id, Some(987));
        assert_eq!(event.message_id, Some(456));
        assert_eq!(event.self_id, Some(10000));
    }

    #[test]
    fn unknown_post_type_does_not_fail() {
        let event: OneBotEvent =
            serde_json::from_value(serde_json::json!({ "post_type": "future_event" }))
                .expect("unknown post_type tolerated");
        assert_eq!(event.post_type, PostType::Unknown);
        assert!(!event.is_message());
    }

    #[test]
    fn missing_fields_are_defaults() {
        let event: OneBotEvent = serde_json::from_value(serde_json::json!({})).expect("parses");
        assert_eq!(event.post_type, PostType::Unknown);
        assert_eq!(event.user_id, 0);
        assert!(event.message_id.is_none());
        assert!(event.message.is_none());
    }

    #[test]
    fn heartbeat_detection() {
        let heartbeat: OneBotEvent = serde_json::from_value(serde_json::json!({
            "post_type": "meta_event",
            "meta_event_type": "heartbeat",
            "time": 1700000000
        }))
        .expect("parses");
        assert!(heartbeat.is_heartbeat_like());
        assert!(!heartbeat.is_message());

        let lifecycle: OneBotEvent = serde_json::from_value(serde_json::json!({
            "post_type": "meta_event",
            "meta_event_type": "lifecycle"
        }))
        .expect("parses");
        assert!(lifecycle.is_heartbeat_like());
    }
}
