//! 把 QQ 官方事件归一化成本 crate 共用的 [`NormalizedInbound`]。
//!
//! # 会话命名空间：`openid` **不是** QQ 号
//!
//! 这是本适配器最容易出错的一处，写在这里给后来人：
//!
//! - QQ 官方（`q.qq.com` 开放平台）在事件里给的是 **`openid`**：群是
//!   `group_openid`，单聊是 `author.user_openid`。对同一个 QQ 群/同一个人、
//!   在**同一个机器人视角下**稳定，但**不是** QQ 号，也不跨机器人稳定。
//! - OneBot（`platforms::onebot`）给的是**真实 QQ 号**：群号直接就是
//!   `conversation_external_id`（十进制数字串），发送者也是 QQ 号。
//!
//! 所以两边的 `conversation_external_id` 是**两套互不相认的命名空间**：
//! `"123456789"` 在 OneBot 里是某个群，在 QQ 官方里可能是一个完全无关的
//! `group_openid`；反之亦然。落到库里就是两套**不同的会话**，不能互相合并、
//! 不能「先按群号找会话再按 openid 找」，也不能拿 OneBot 的历史去续 QQ 官方的
//! 上下文。判断「同一条消息」只能靠 `bot_platform_id` + `conversation_external_id`
//! 这对组合，而 QQ 侧的 `bot_platform_id` 恒为 `None`（见 [`crate::normalize`]）。
//!
//! 另外注意 **`READY.d.user.id` 是 20 位十进制数字串**（真实 bot 的 QQ 号形态），
//! 而事件里的 openid 是 32 位大写 HEX —— 两者不是一个东西，别互相代入。
//!
//! # 字段映射（2026-11-16 抓到真实事件后核对）
//!
//! | `NormalizedInbound` | 来源 |
//! |---|---|
//! | `external_id` | 事件 `d.id`（`ROBOT1.0_…` 形态的**官方消息 ID**，重投递不变） |
//! | `reply_to` | 同上 —— 被动回复要带的就是它 |
//! | `conversation_external_id` | 群：`d.group_openid`；单聊：`d.author.user_openid`；频道：`d.channel_id` |
//! | `sender_id` | 群：`d.author.member_openid`；单聊：`d.author.user_openid`；频道：`d.author.id` |
//! | `text` | `d.content` 剥掉 `<@OPENID>` 提及标记 |
//! | `received_at` | `d.timestamp`（ISO8601 带 `+08:00` 偏移）；解析失败退回本地时钟并 `warn!` |
//! | `raw` | 整帧 JSON（含 `op`/`t`/`s`/`d`，便于事后取证），**已脱敏** |
//! | `bot_platform_id` / `platform_message_id` | 恒 `None`（QQ 侧没有本地整型 ID） |
//!
//! # 事件名：两个都要认
//!
//! 实测（2026-11-16，真实群 + 真实私聊）群消息事件是 **`GROUP_MESSAGE_CREATE`**，
//! 而公开文档/社区资料里写的是 `GROUP_AT_MESSAGE_CREATE`。两个都处理：真实名字
//! 优先，文档名字保留兼容。单聊是 `C2C_MESSAGE_CREATE`（与文档一致）。
//!
//! # `message_scene.ext` 里带凭据，必须脱敏
//!
//! 真实的群/单聊事件都有：
//!
//! ```text
//! "message_scene": { "ext": ["msg_idx=REFIDX_…", "auth_token=ThYboCL9…"], "source": "default" }
//! ```
//!
//! `auth_token` 是平台推送凭据，**等同密钥**。它跟着 `raw` 一起落库、一起进日志，
//! 所以 [`normalize`] 在写 `raw` 之前会做一遍递归脱敏：任何含 `auth_token=` 的
//! 字符串都会变成 `auth_token=<redacted>`，键名正好是 `auth_token` 的字段也替换成
//! `<redacted>`。这一层是**唯一**的出口 —— 别在别处直接 clone 原始帧落库。
//!
//! # 关于 `AT_MESSAGE_CREATE`（频道 @ 消息）
//!
//! **未核实**。载荷里是否一定有 `channel_id`、`author.id` 是不是字符串、发送接口
//! 是否仍是 `/channels/{channel_id}/messages` 都未在真网关上验过。这里按
//! 「有 `channel_id` 就当频道会话」的保守口径处理，`kind = Channel`；发送侧对
//! 频道一律返回 [`crate::error::AdapterError::Unsupported`]，不猜。

use hbot_core::domain::{ConversationKind, Timestamp};
use serde_json::Value;

use crate::error::AdapterError;
use crate::normalize::NormalizedInbound;
use crate::platforms::qq::event::{
    EVENT_AT_MESSAGE_CREATE, EVENT_C2C_MESSAGE_CREATE, EVENT_GROUP_AT_MESSAGE_CREATE,
    EVENT_GROUP_MESSAGE_CREATE, GatewayFrame,
};

/// 从一帧网关消息里取出归一化入站消息。
///
/// 返回 `Ok(None)` 的两种情况都是**正常**的：
///
/// - 不是 `op:0` 事件帧（心跳、ACK、Hello、Reconnect…）；
/// - 是事件帧但事件名与本适配器无关（`READY`、`RESUMED`、成员变更等）。
///
/// 只有「是消息事件但缺关键字段（`d.id` / `group_openid`）」才报错 —— 那种帧无法
/// 安全地转成会话消息，伪造一个 ID 会让去重和被动回复同时失效。
///
/// `received_at` 是**兜底**时间：优先用事件自带的 `d.timestamp`。
pub fn normalize(
    frame: &GatewayFrame,
    received_at: Timestamp,
) -> Result<Option<NormalizedInbound>, AdapterError> {
    if !frame.is_dispatch() {
        return Ok(None);
    }
    let event_name = frame
        .event_name()
        .ok_or_else(|| AdapterError::protocol("qq dispatch frame is missing the `t` event name"))?;
    let received_at = match event_timestamp(&frame.data) {
        Some(timestamp) => timestamp,
        None => {
            tracing::warn!(
                event = event_name,
                "qq event has no parsable `d.timestamp`; falling back to the local clock"
            );
            received_at
        }
    };
    match event_name {
        // 实测事件名。
        EVENT_GROUP_MESSAGE_CREATE => normalize_group(frame, received_at),
        // 文档/社区里的旧名，保留兼容。
        EVENT_GROUP_AT_MESSAGE_CREATE => normalize_group(frame, received_at),
        EVENT_C2C_MESSAGE_CREATE => normalize_c2c(frame, received_at),
        EVENT_AT_MESSAGE_CREATE => normalize_channel(frame, received_at),
        _ => Ok(None),
    }
}

/// 这条消息是否 @ 了机器人。
///
/// 判据是事件里的 `d.mentions[].is_you == true`（**实测**真实帧带这个字段）。
/// **不要**用「正文里有没有 `<@…>`」或「`member_role` 是不是 admin」去猜：前者在
/// 群里 @ 别人时也成立，后者是机器人自己的权限角色，与「这条消息 @ 了谁」无关。
pub fn is_bot_mentioned(data: &Value) -> bool {
    data.get("mentions")
        .and_then(Value::as_array)
        .is_some_and(|mentions| {
            mentions.iter().any(|mention| {
                mention
                    .get("is_you")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        })
}

/// 群消息（`GROUP_MESSAGE_CREATE`，兼容 `GROUP_AT_MESSAGE_CREATE`）。
fn normalize_group(
    frame: &GatewayFrame,
    received_at: Timestamp,
) -> Result<Option<NormalizedInbound>, AdapterError> {
    let data = &frame.data;
    let (message_id, text) = message_identity(data, "group message")?;
    // 实测两者相同；`group_openid` 是官方口径，`group_id` 留作兜底。
    let conversation = string_field(data, "group_openid")
        .or_else(|| string_field(data, "group_id"))
        .ok_or_else(|| AdapterError::protocol("qq group message is missing `d.group_openid`"))?;
    let author = data.get("author");
    let sender = author
        .and_then(|author| string_field(author, "member_openid"))
        .or_else(|| author.and_then(|author| string_field(author, "id")))
        .or_else(|| {
            data.get("group_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Ok(Some(build(
        frame,
        message_id,
        text,
        ConversationKind::Group,
        conversation,
        sender,
        received_at,
    )))
}

/// 单聊消息（`C2C_MESSAGE_CREATE`）。
fn normalize_c2c(
    frame: &GatewayFrame,
    received_at: Timestamp,
) -> Result<Option<NormalizedInbound>, AdapterError> {
    let data = &frame.data;
    let (message_id, text) = message_identity(data, "c2c message")?;
    let author = data
        .get("author")
        .ok_or_else(|| AdapterError::protocol("qq c2c message is missing `d.author`"))?;
    let conversation = string_field(author, "user_openid")
        .or_else(|| string_field(author, "id"))
        .ok_or_else(|| {
            AdapterError::protocol("qq c2c message is missing `d.author.user_openid`")
        })?;
    let sender = string_field(author, "user_openid")
        .or_else(|| string_field(author, "id"))
        .unwrap_or_default();
    Ok(Some(build(
        frame,
        message_id,
        text,
        ConversationKind::Private,
        conversation,
        sender,
        received_at,
    )))
}

/// 频道 @ 消息（`AT_MESSAGE_CREATE`）。**字段形状未核实**，见模块文档。
fn normalize_channel(
    frame: &GatewayFrame,
    received_at: Timestamp,
) -> Result<Option<NormalizedInbound>, AdapterError> {
    let data = &frame.data;
    let (message_id, text) = message_identity(data, "channel message")?;
    let conversation = string_field(data, "channel_id")
        .ok_or_else(|| AdapterError::protocol("qq channel message is missing `d.channel_id`"))?;
    let sender = data
        .get("author")
        .and_then(|author| string_field(author, "id"))
        .unwrap_or_default();
    Ok(Some(build(
        frame,
        message_id,
        text,
        ConversationKind::Channel,
        conversation,
        sender,
        received_at,
    )))
}

/// 组装归一化结果（四条路径只有「会话/发送者/kind」不同）。
fn build(
    frame: &GatewayFrame,
    message_id: String,
    text: String,
    kind: ConversationKind,
    conversation_external_id: String,
    sender_id: String,
    received_at: Timestamp,
) -> NormalizedInbound {
    NormalizedInbound {
        bot_platform_id: None,
        reply_to: Some(message_id.clone()),
        external_id: message_id,
        conversation_external_id,
        kind,
        sender_id,
        text,
        platform_message_id: None,
        raw: sanitize_raw(&frame.raw),
        received_at,
    }
}

/// 取 `d.id`（官方消息 ID）与剥掉 @ 提及后的正文。
fn message_identity(data: &Value, context: &str) -> Result<(String, String), AdapterError> {
    let message_id = string_field(data, "id")
        .ok_or_else(|| AdapterError::protocol(format!("qq {context} is missing `d.id`")))?;
    let content = data.get("content").and_then(Value::as_str).unwrap_or("");
    Ok((message_id, strip_at_mentions(content)))
}

/// 取一个非空字符串字段（空串也算缺失 —— 官方帧里 `union_openid` 就常常是 `""`）。
fn string_field(object: &Value, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// 事件时间：`d.timestamp` 的 ISO8601（带偏移）。
fn event_timestamp(data: &Value) -> Option<Timestamp> {
    data.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| parse_iso8601_millis(text).map(Timestamp::from_unix_millis))
}

/// 剥掉 QQ 文本里的 @ 提及标记，其余原样保留。
///
/// 实测真实事件里 @ 机器人写成 `<@196AAC0A1E7A06AC0D5EEB7FCF778303>`（尖括号 +
/// `@` + **32 位 HEX**）—— 不是网上常见的 `<@!123456>`。两种都剥：
///
/// - `<@` + 32 位十六进制 + `>`（实测形态）；
/// - `<@` + 可选 `!` + 1–20 位十进制数字 + `>`（文档形态 `<@!123456>`）。
///
/// 规则（刻意写得窄，避免吃掉正常文本）：
///
/// - 其余 `<…>` 一律原样保留（`<@abc>`、`"1 < 2"` 都不能被吃掉）；
/// - 剥掉标记后再吞掉紧跟其后的**一个**空白字符（`"<@!1> 你好"` → `"你好"`）；
/// - 最后 `trim()` 首尾空白 —— 标记被剥掉后留下的边角空格同样没有意义。
pub fn strip_at_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0usize;
    let mut cursor = 0usize;
    while let Some(start) = find_mention_start(text, cursor) {
        match mention_end(text, start) {
            Some(end) => {
                out.push_str(&text[copied..start]);
                let mut next = end;
                if let Some(ch) = text[next..].chars().next() {
                    if ch.is_whitespace() {
                        next += ch.len_utf8();
                    }
                }
                copied = next;
                cursor = next;
            }
            // `<@` 开头但不是合法提及：别跳过，让下面按普通字符复制。
            None => cursor = start + 1,
        }
    }
    out.push_str(&text[copied..]);
    out.trim().to_owned()
}

/// 从 `from` 起找下一个 `<@`（返回 `<` 的字节位置）。
fn find_mention_start(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut offset = from;
    while offset + 1 < bytes.len() {
        let found = text[offset..].find('<')?;
        // `find` 给的偏移落在 ASCII 字节上，`position + 1` 一定是字符边界。
        let position = offset + found;
        if bytes.get(position + 1) == Some(&b'@') {
            return Some(position);
        }
        offset = position + 1;
    }
    None
}

/// 校验 `<@[!]ID>` 并返回 `>` 之后的字节位置。
fn mention_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = start + 2;
    if bytes.get(cursor) == Some(&b'!') {
        cursor += 1;
    }
    let id_start = cursor;
    while let Some(byte) = bytes.get(cursor) {
        if !byte.is_ascii_alphanumeric() {
            break;
        }
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'>') {
        return None;
    }
    let id = &text[id_start..cursor];
    let is_openid = id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit());
    let is_numeric = (1..=20).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit());
    (is_openid || is_numeric).then_some(cursor + 1)
}

/// 递归脱敏：`auth_token=…` 的值与 `auth_token` 字段都替换成 `<redacted>`。
fn sanitize_raw(raw: &Value) -> Value {
    match raw {
        Value::String(text) => Value::String(redact_auth_token(text)),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_raw).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    if key.eq_ignore_ascii_case("auth_token") {
                        (key.clone(), Value::String("<redacted>".to_owned()))
                    } else {
                        (key.clone(), sanitize_raw(value))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 把一行 `…auth_token=<secret>` 里的值换成 `<redacted>`（可能一行里有多段）。
fn redact_auth_token(text: &str) -> String {
    const MARKER: &str = "auth_token=";
    if !text.contains(MARKER) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(MARKER) {
        let (before, after) = rest.split_at(index + MARKER.len());
        out.push_str(before);
        // 值的边界：空白、`&`、`;`、`,`、`]`、`"`、`}` 都算结束。
        let end = after
            .find(|ch: char| {
                ch.is_whitespace() || matches!(ch, '&' | ';' | ',' | ']' | '"' | '}' | '\'')
            })
            .unwrap_or(after.len());
        out.push_str("<redacted>");
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// 解析 ISO8601 → Unix 毫秒。
///
/// 支持 `YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM|+HHMM)`；实测事件是
/// `2026-11-16T08:06:39+08:00`。不引入日期库（workspace 没引，`hbot-core` 也刻意
/// 不引，见 `domain.rs` 的注释），所以这里用 days-from-civil 算法自己算。
fn parse_iso8601_millis(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text.find(['T', 't', ' '])?;
    let (date, rest) = text.split_at(split);
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    let (clock, offset_minutes) = split_offset(&rest[1..])?;
    let mut clock_parts = clock.split(':');
    let hour: u32 = clock_parts.next()?.parse().ok()?;
    let minute: u32 = clock_parts.next()?.parse().ok()?;
    let seconds_text = clock_parts.next()?;
    if clock_parts.next().is_some() {
        return None;
    }
    let (seconds_text, fraction_ms) = match seconds_text.split_once('.') {
        Some((seconds, fraction)) => (seconds, parse_fraction_millis(fraction)?),
        None => (seconds_text, 0),
    };
    let second: u32 = seconds_text.parse().ok()?;

    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60
        - i64::from(offset_minutes) * 60
        + i64::from(second);
    Some(seconds * 1_000 + fraction_ms)
}

/// 拆出时钟部分与时区偏移（分钟，东八区为 `+480`）。
fn split_offset(rest: &str) -> Option<(&str, i32)> {
    if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        return Some((clock, 0));
    }
    let index = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let text = &offset[1..];
    let (hours, minutes) = match text.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if text.len() == 4 => (&text[..2], &text[2..]),
        None => (text, "0"),
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((clock, sign * (hours * 60 + minutes)))
}

/// 小数秒 → 毫秒（不足三位左补零，超过三位截断）。
fn parse_fraction_millis(fraction: &str) -> Option<i64> {
    if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut millis = 0i64;
    for (index, byte) in fraction.bytes().take(3).enumerate() {
        let place = 10i64.pow(2 - u32::try_from(index).ok()?);
        millis += i64::from(byte - b'0') * place;
    }
    Some(millis)
}

/// Howard Hinnant 的 days-from-civil：公历日期 → 距 1970-01-01 的天数。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ts: i64) -> Timestamp {
        Timestamp::from_unix_millis(ts)
    }

    /// 实测帧（2026-11-16 抓到的真实群消息，已去掉头像与 auth_token 值）。
    const GROUP_FRAME: &str = r#"{"op":0,"t":"GROUP_MESSAGE_CREATE","s":2,"d":{
        "author":{"bot":false,"id":"336390D2FCA8472A739B0B1B2B6A3A90",
            "member_nickname":"墨白","member_openid":"336390D2FCA8472A739B0B1B2B6A3A90",
            "member_role":"owner","union_openid":"","username":"墨白"},
        "content":"<@196AAC0A1E7A06AC0D5EEB7FCF778303> 你好",
        "group_id":"F9CBDDEE95273F1E42EF42B8C49D3578",
        "group_openid":"F9CBDDEE95273F1E42EF42B8C49D3578",
        "id":"ROBOT1.0_mPc6uqiVajet59qY2BykbzzIFtfjm6dD-GiarTv",
        "mentions":[{"bot":true,"id":"196AAC0A1E7A06AC0D5EEB7FCF778303","is_you":true,
            "member_openid":"196AAC0A1E7A06AC0D5EEB7FCF778303","scope":"single"}],
        "message_scene":{"ext":["msg_idx=REFIDX_x","auth_token=SECRET-AUTH-TOKEN"],"source":"default"},
        "message_type":0,"timestamp":"2026-11-16T08:06:39+08:00"
    }}"#;

    #[test]
    fn strips_hex_openid_mentions_from_real_frames() {
        // 实测形态：尖括号 + @ + 32 位 HEX。
        assert_eq!(
            strip_at_mentions("<@196AAC0A1E7A06AC0D5EEB7FCF778303> 你好"),
            "你好"
        );
        // 文档形态：尖括号 + @! + 数字。
        assert_eq!(strip_at_mentions("<@!123456> 你好"), "你好");
        assert_eq!(strip_at_mentions("你好 <@!123456>"), "你好");
        assert_eq!(
            strip_at_mentions("你好 <@196AAC0A1E7A06AC0D5EEB7FCF778303> 世界"),
            "你好 世界"
        );
        assert_eq!(
            strip_at_mentions(
                "<@196AAC0A1E7A06AC0D5EEB7FCF778303><@196AAC0A1E7A06AC0D5EEB7FCF778303>你好"
            ),
            "你好"
        );
        // 不合法/不像提及的内容必须原样保留。
        assert_eq!(strip_at_mentions("<@abc> 你好"), "<@abc> 你好");
        assert_eq!(
            strip_at_mentions("<@196AAC0A1E7A06AC0D5EEB7FCF77830> 你好"),
            "<@196AAC0A1E7A06AC0D5EEB7FCF77830> 你好"
        );
        assert_eq!(strip_at_mentions("1 < 2 和 a@b"), "1 < 2 和 a@b");
        assert_eq!(strip_at_mentions("a < b > c"), "a < b > c");
        assert_eq!(strip_at_mentions(""), "");
    }

    #[test]
    fn group_message_maps_openid_not_qq_number() {
        let frame = GatewayFrame::parse(GROUP_FRAME).expect("valid frame");
        let message = normalize(&frame, at(0))
            .expect("normalize")
            .expect("message event");
        assert_eq!(
            message.external_id,
            "ROBOT1.0_mPc6uqiVajet59qY2BykbzzIFtfjm6dD-GiarTv"
        );
        assert_eq!(message.reply_to, Some(message.external_id.clone()));
        assert_eq!(
            message.conversation_external_id,
            "F9CBDDEE95273F1E42EF42B8C49D3578"
        );
        assert_eq!(message.kind, ConversationKind::Group);
        assert_eq!(message.sender_id, "336390D2FCA8472A739B0B1B2B6A3A90");
        assert_eq!(message.text, "你好");
        assert_eq!(message.bot_platform_id, None);
        assert_eq!(message.platform_message_id, None);
        // 时间取事件自带的 d.timestamp（+08:00），不是兜底值。
        // 2026-11-16T08:06:39+08:00 == 2026-11-16T00:06:39Z == 1794787599000ms
        assert_eq!(message.received_at.unix_millis(), 1_794_787_599_000);
        assert_eq!(
            message.raw["t"],
            Value::String("GROUP_MESSAGE_CREATE".to_owned())
        );
        assert_eq!(message.raw["s"], Value::from(2));
    }

    #[test]
    fn raw_redacts_the_message_scene_auth_token() {
        let frame = GatewayFrame::parse(GROUP_FRAME).expect("valid frame");
        let message = normalize(&frame, at(0))
            .expect("normalize")
            .expect("message event");
        let serialized = serde_json::to_string(&message.raw).expect("serialize raw");
        assert!(
            !serialized.contains("SECRET-AUTH-TOKEN"),
            "auth_token leaked into raw: {serialized}"
        );
        assert!(serialized.contains("auth_token=<redacted>"));
        // 同一数组里的非敏感项要保留（排查问题还要靠它）。
        assert!(serialized.contains("msg_idx=REFIDX_x"));
        // 原始帧本身没被改（脱敏只作用于落地副本）。
        assert!(GROUP_FRAME.contains("SECRET-AUTH-TOKEN"));
    }

    #[test]
    fn redaction_covers_standalone_fields_and_multiple_markers() {
        let raw = serde_json::json!({
            "auth_token": "SECRET",
            "note": "a auth_token=SECRET1 b auth_token=SECRET2&c=1",
            "nested": [{"ext": "auth_token=SECRET3"}],
        });
        let sanitized = sanitize_raw(&raw);
        let serialized = serde_json::to_string(&sanitized).expect("serialize");
        for secret in ["SECRET1", "SECRET2", "SECRET3"] {
            assert!(
                !serialized.contains(secret),
                "{secret} leaked: {serialized}"
            );
        }
        assert!(
            !serialized.contains("\"SECRET\""),
            "field leaked: {serialized}"
        );
        assert!(serialized.contains("b auth_token=<redacted>&c=1"));
    }

    #[test]
    fn c2c_message_uses_the_user_openid_as_conversation() {
        let frame = GatewayFrame::parse(
            r#"{"op":0,"t":"C2C_MESSAGE_CREATE","s":3,"d":{
                "author":{"bot":false,"id":"336390D2FCA8472A739B0B1B2B6A3A90",
                    "union_openid":"","user_openid":"336390D2FCA8472A739B0B1B2B6A3A90",
                    "username":"墨白"},
                "content":"你好",
                "id":"ROBOT1.0_.VlvKOAVkSw6.WZBH1J-SWM54Jg1hrX9",
                "message_scene":{"ext":["msg_idx=REFIDX_y"],"source":"default"},
                "message_type":0,"timestamp":"2026-11-16T08:06:42+08:00"
            }}"#,
        )
        .expect("valid frame");
        let message = normalize(&frame, at(5))
            .expect("normalize")
            .expect("message event");
        assert_eq!(message.kind, ConversationKind::Private);
        assert_eq!(
            message.conversation_external_id,
            "336390D2FCA8472A739B0B1B2B6A3A90"
        );
        assert_eq!(message.sender_id, "336390D2FCA8472A739B0B1B2B6A3A90");
        assert_eq!(message.text, "你好");
    }

    #[test]
    fn legacy_group_event_name_is_still_understood() {
        let frame = GatewayFrame::parse(
            r#"{"op":0,"t":"GROUP_AT_MESSAGE_CREATE","d":{
                "id":"MSGID-1","content":"<@!123456> 你好","group_openid":"GROUP-OPENID-A",
                "author":{"id":"AUTHOR-ID","member_openid":"MEMBER-OPENID-B"}
            }}"#,
        )
        .expect("valid frame");
        let message = normalize(&frame, at(7))
            .expect("normalize")
            .expect("message event");
        assert_eq!(message.conversation_external_id, "GROUP-OPENID-A");
        assert_eq!(message.sender_id, "MEMBER-OPENID-B");
        assert_eq!(message.text, "你好");
    }

    #[test]
    fn bot_mention_is_read_from_mentions_not_from_the_text() {
        let frame = GatewayFrame::parse(GROUP_FRAME).expect("valid frame");
        assert!(is_bot_mentioned(&frame.data));
        // 正文里有 `<@…>` 但 mentions 里没有 is_you —— 不算 @ 机器人。
        let other = serde_json::json!({
            "content": "<@196AAC0A1E7A06AC0D5EEB7FCF778303> 你好",
            "mentions": [{"id":"196AAC0A1E7A06AC0D5EEB7FCF778303","is_you":false}]
        });
        assert!(!is_bot_mentioned(&other));
        assert!(!is_bot_mentioned(&serde_json::json!({"content":"你好"})));
    }

    #[test]
    fn channel_message_is_marked_as_a_channel() {
        let frame = GatewayFrame::parse(
            r#"{"op":0,"t":"AT_MESSAGE_CREATE","d":{
                "id":"MSGID-3","content":"<@!9> hi","channel_id":"CHANNEL-1",
                "author":{"id":"USER-1"}
            }}"#,
        )
        .expect("valid frame");
        let message = normalize(&frame, at(5))
            .expect("normalize")
            .expect("message event");
        assert_eq!(message.kind, ConversationKind::Channel);
        assert_eq!(message.conversation_external_id, "CHANNEL-1");
        assert_eq!(message.text, "hi");
    }

    #[test]
    fn control_frames_and_unrelated_events_are_ignored() {
        for text in [
            r#"{"op":10,"d":{"heartbeat_interval":41250}}"#,
            r#"{"op":11}"#,
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"s"}}"#,
            r#"{"op":0,"t":"RESUMED","d":{}}"#,
            r#"{"op":0,"t":"GROUP_ADD_ROBOT","d":{}}"#,
        ] {
            let frame = GatewayFrame::parse(text).expect("valid frame");
            assert_eq!(
                normalize(&frame, at(1)).expect("normalize"),
                None,
                "expected `{text}` to be ignored"
            );
        }
    }

    #[test]
    fn message_events_without_identity_are_protocol_errors() {
        let missing_id =
            GatewayFrame::parse(r#"{"op":0,"t":"GROUP_MESSAGE_CREATE","d":{"group_openid":"G"}}"#)
                .expect("valid frame");
        let error = normalize(&missing_id, at(1)).expect_err("missing d.id must fail");
        assert!(error.to_string().contains("d.id"));

        let missing_conversation = GatewayFrame::parse(
            r#"{"op":0,"t":"GROUP_MESSAGE_CREATE","d":{"id":"M","content":"hi"}}"#,
        )
        .expect("valid frame");
        let error = normalize(&missing_conversation, at(1)).expect_err("missing group_openid");
        assert!(error.to_string().contains("group_openid"));
    }

    #[test]
    fn event_timestamp_parses_iso8601_with_offsets() {
        // 同一个瞬间的两种写法必须相等。
        let shanghai = parse_iso8601_millis("2026-11-16T08:06:39+08:00").expect("offset form");
        let utc = parse_iso8601_millis("2026-11-16T00:06:39Z").expect("zulu form");
        assert_eq!(shanghai, utc);
        // 纪元锚点。
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:00Z"), Some(0));
        // 非闰年 2 月末到 3 月初正好一天。
        let end_of_feb = parse_iso8601_millis("2026-02-28T00:00:00Z").expect("feb");
        let start_of_mar = parse_iso8601_millis("2026-03-01T00:00:00Z").expect("mar");
        assert_eq!(start_of_mar - end_of_feb, 86_400_000);
        // 毫秒与无冒号偏移。
        assert_eq!(
            parse_iso8601_millis("2026-11-16T00:06:39.5Z"),
            Some(utc + 500)
        );
        assert_eq!(
            parse_iso8601_millis("2026-11-16T08:06:39+0800"),
            Some(shanghai)
        );
        for bad in [
            "",
            "2026-11-16",
            "2026-13-01T00:00:00Z",
            "2026-11-16T25:00:00Z",
            "2026-11-16T00:00:00+25:00",
            "not a timestamp",
        ] {
            assert_eq!(parse_iso8601_millis(bad), None, "expected `{bad}` to fail");
        }
    }

    #[test]
    fn missing_timestamp_falls_back_to_the_local_clock() {
        let frame = GatewayFrame::parse(
            r#"{"op":0,"t":"C2C_MESSAGE_CREATE","d":{
                "id":"M","content":"hi","author":{"user_openid":"U"}}}"#,
        )
        .expect("valid frame");
        let message = normalize(&frame, at(4_242))
            .expect("normalize")
            .expect("message event");
        assert_eq!(message.received_at, at(4_242));
    }
}
