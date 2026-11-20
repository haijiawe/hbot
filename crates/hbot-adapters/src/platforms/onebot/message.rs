//! OneBot 11 消息段 ↔ 纯文本。
//!
//! OneBot 11 的 `message` 字段是「段数组」（`[{ "type": "text", "data": { ... } }]`），
//! 或者（部分实现/主动发送时）就是一个字符串。段数组能表达 `at`、图片、表情、引用等
//! **结构化语义**，纯文本表达不了。
//!
//! # 有损转换（重要）
//!
//! [`MessagePayload::to_plain_text`] 是**有损**的：它把消息压成一段给业务代码看的
//! 文本，丢掉的东西比留下的多。逐段说明（见 [`MessagePayload::to_plain_text`] 的
//! 实现与 `docs/02-architecture.md` §8 `MessageContext.text`）：
//!
//! | 段 | 纯文本渲染 | 丢失的信息 |
//! |---|---|---|
//! | `text` | 原文 | 无（这是唯一无损的段） |
//! | `at` | `@{qq}` | 不是真正的 @（生成代码回复时不会 @ 到人）；QQ 号只在文本里 |
//! | `image` | `[图片]` | `file` / `url` / 尺寸 / 是否闪照 |
//! | `face` | `[表情]` | 表情 id |
//! | `reply` | 空字符串 | 引用目标只在 [`MessagePayload::reply_to`] 里，不进文本 |
//! | 其它（`record`/`video`/`file`/`json`/`xml` …） | `[{type}]` | 全部内容 |
//!
//! 因此：
//!
//! - 业务需要 @、图片、引用时**必须**读结构化字段（[`Segment`] / [`MessagePayload::reply_to`]），
//!   不要解析 `to_plain_text()` 的输出；
//! - 原始事件 JSON 随 `NormalizedInbound::raw` 一起落库，需要时以它为准，
//!   `text` 只是给 LLM / 日志用的可读视图。

use std::path::Path;

use hbot_core::{AttachmentKind, AttachmentRef};

use crate::AdapterError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// 段类型：纯文本。
pub const SEGMENT_TEXT: &str = "text";
/// 段类型：@ 某人。
pub const SEGMENT_AT: &str = "at";
/// 段类型：图片。
pub const SEGMENT_IMAGE: &str = "image";
/// 段类型：QQ 表情。
pub const SEGMENT_FACE: &str = "face";
/// 段类型：引用回复。
pub const SEGMENT_REPLY: &str = "reply";
/// 段类型：普通文件附件（图片以外的文件走这个段）。
pub const SEGMENT_FILE: &str = "file";

/// 出站附件的大小上限：5 MiB。base64 之后约 6.7 MiB 文本，仍能塞进一条 WS 帧。
pub const MAX_OUTBOUND_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

/// 纯文本里 `at` 段渲染成 `@{qq}`；图片渲染成这个占位符。
const IMAGE_PLACEHOLDER: &str = "[图片]";
/// 纯文本里 `face` 段渲染成这个占位符。
const FACE_PLACEHOLDER: &str = "[表情]";

/// 一个 OneBot 消息段。
///
/// 对应 JSON 形状 `{ "type": "<kind>", "data": { ... } }`。`data` 保持不透明
/// [`Map`]，因为各实现的字段差异很大（例如 `image` 可能给 `file`、`url` 或
/// `file_id`，`at` 的 QQ 号可能是数字也可能是字符串）；用 [`Segment::data_str`]
/// 读取，数字会被转成字符串。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// 段类型（`text` / `at` / `image` / `face` / `reply` / …）。
    #[serde(rename = "type")]
    pub kind: String,
    /// 段内容；缺失时为空 map（部分实现会省掉 `data`）。
    #[serde(default)]
    pub data: Map<String, Value>,
}

impl Segment {
    /// 由类型与内容构造。
    pub fn new(kind: impl Into<String>, data: Map<String, Value>) -> Self {
        Self {
            kind: kind.into(),
            data,
        }
    }

    /// 构造 `text` 段。
    pub fn text(text: impl Into<String>) -> Self {
        let mut data = Map::new();
        data.insert("text".to_string(), Value::String(text.into()));
        Self::new(SEGMENT_TEXT, data)
    }

    /// 构造 `at` 段（`@qq`）。
    pub fn at(qq: impl Into<String>) -> Self {
        let mut data = Map::new();
        data.insert("qq".to_string(), Value::String(qq.into()));
        Self::new(SEGMENT_AT, data)
    }

    /// 构造 `reply` 段（引用 `message_id`）。
    pub fn reply(message_id: impl Into<String>) -> Self {
        let mut data = Map::new();
        data.insert("id".to_string(), Value::String(message_id.into()));
        Self::new(SEGMENT_REPLY, data)
    }

    /// 段的类型（未知类型归入 [`SegmentKind::Other`]）。
    pub fn kind_enum(&self) -> SegmentKind {
        SegmentKind::from_wire(&self.kind)
    }

    /// 读 `data` 里的字符串字段；数字会被转成十进制字符串，缺失或非标量返回 `None`。
    ///
    /// 兼容写法差异：`{"qq": 123}`（数字）与 `{"qq": "123"}`（字符串）等价。
    pub fn data_str(&self, key: &str) -> Option<String> {
        match self.data.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            _ => None,
        }
    }
}

/// 段类型枚举（便于 `match`，未知类型保留原始字符串）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// `text`：纯文本。
    Text,
    /// `at`：@ 某人。
    At,
    /// `image`：图片。
    Image,
    /// `face`：QQ 表情。
    Face,
    /// `reply`：引用回复。
    Reply,
    /// 其它段（`record` / `video` / `file` / `json` / `xml` / 自定义）。
    Other(String),
}

impl SegmentKind {
    /// 由线上的 `type` 字符串解析；大小写不敏感，未知值进 [`SegmentKind::Other`]。
    pub fn from_wire(kind: &str) -> Self {
        let lower = kind.trim().to_ascii_lowercase();
        match lower.as_str() {
            SEGMENT_TEXT => Self::Text,
            SEGMENT_AT => Self::At,
            SEGMENT_IMAGE => Self::Image,
            SEGMENT_FACE => Self::Face,
            SEGMENT_REPLY => Self::Reply,
            _ => Self::Other(lower),
        }
    }

    /// 线上的 `type` 字符串（[`SegmentKind::Other`] 返回其内部值）。
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text => SEGMENT_TEXT,
            Self::At => SEGMENT_AT,
            Self::Image => SEGMENT_IMAGE,
            Self::Face => SEGMENT_FACE,
            Self::Reply => SEGMENT_REPLY,
            Self::Other(other) => other,
        }
    }
}

/// OneBot 的 `message` 字段：字符串或段数组。
///
/// `#[serde(untagged)]` 让两种形状都能反序列化：先按 `String` 试，失败再按
/// `Vec<Segment>`；这也是 OneBot 11 规范里 `message` 的两种合法形式。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessagePayload {
    /// 纯字符串消息（部分实现、以及我们自己发出的纯文本）。
    Text(String),
    /// 段数组（结构化消息）。
    Segments(Vec<Segment>),
}

impl MessagePayload {
    /// 展开成段数组；字符串消息会包成一个 `text` 段。
    ///
    /// 返回拥有所有权的 `Vec`（字符串分支必须分配，保持签名统一）。
    pub fn segments(&self) -> Vec<Segment> {
        match self {
            Self::Text(text) => vec![Segment::text(text.clone())],
            Self::Segments(segments) => segments.clone(),
        }
    }

    /// 有损地渲染成纯文本（见模块级「有损转换」表）。
    ///
    /// 规则：`text` 原文；`at` → `@{qq}`；`image` → `[图片]`；`face` → `[表情]`；
    /// `reply` → 空（引用目标请用 [`Self::reply_to`]）；未知段 → `[{type}]`。
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for segment in self.segments() {
            match segment.kind_enum() {
                SegmentKind::Text => {
                    if let Some(text) = segment.data_str("text") {
                        out.push_str(&text);
                    }
                }
                SegmentKind::At => {
                    // 有损：这里只是可读文本，不是真正的 mention；
                    // 需要回复时 @ 到人必须由出站 `at` 段完成。
                    let qq = segment.data_str("qq").unwrap_or_default();
                    out.push('@');
                    out.push_str(&qq);
                }
                SegmentKind::Image => out.push_str(IMAGE_PLACEHOLDER),
                SegmentKind::Face => out.push_str(FACE_PLACEHOLDER),
                // 有损：引用目标不重复进正文，见 reply_to()。
                SegmentKind::Reply => {}
                SegmentKind::Other(kind) => {
                    out.push('[');
                    out.push_str(&kind);
                    out.push(']');
                }
            }
        }
        out
    }

    /// 引用回复的目标 `message_id`（第一个 `reply` 段的 `id`）。
    ///
    /// 值为字符串，因为不同实现的 `message_id` 宽度/形态不一致；文本渲染里
    /// 不出现它（见模块级有损表）。
    pub fn reply_to(&self) -> Option<String> {
        self.segments().iter().find_map(|segment| {
            if segment.kind_enum() == SegmentKind::Reply {
                segment.data_str("id")
            } else {
                None
            }
        })
    }

    /// 是否没有任何可见内容（空段数组 / 空字符串 / 只有引用段）。
    pub fn is_empty_text(&self) -> bool {
        self.to_plain_text().is_empty()
    }
}

/// 把出站纯文本编成 OneBot 可发送的 `message` 字段。
///
/// 有回复目标时在前面加 `reply` 段（OneBot 11 的引用语义就是
/// `{"type":"reply","data":{"id":"<message_id>"}}`）；纯文本走字符串形式，
/// 若带引用则必须用段数组。
pub fn build_outbound(text: &str, reply_to: Option<&str>) -> MessagePayload {
    match reply_to {
        None | Some("") => MessagePayload::Text(text.to_string()),
        Some(id) => MessagePayload::Segments(vec![Segment::reply(id), Segment::text(text)]),
    }
}

/// 出站消息序列化成 OneBot `message` 字段的 JSON 值。
pub fn outbound_json(text: &str, reply_to: Option<&str>) -> Value {
    let payload = build_outbound(text, reply_to);
    serde_json::to_value(payload).unwrap_or_else(|_| {
        // `MessagePayload` 只含 String / Vec<Segment>，序列化不会失败；
        // 仍不 panic：退化成字符串形式，由发送侧的错误重试兜底。
        json!({ "type": "text", "data": { "text": text } })
    })
}

/// 把宿主核验过的附件读成 OneBot 消息段。
///
/// 路径**已经是绝对路径**：宿主在入队前把用户给的相对路径解析并校验过（在项目根内、
/// 文件存在、大小不超限），这里不再按用户输入重新解析 —— 再做一次相对路径解析只会
/// 让「入队时合法、发送时越界」的两个判据打架。
///
/// 读文件失败 / 超限都返回 `Err`：**不许**静默降级成只发正文（用户会以为图发出去了）。
/// 错误文案只给文件名，不带绝对路径（AGENTS.md 坑 47：这条 message 会进日志/模型上下文）。
pub fn attachment_segment(attachment: &AttachmentRef) -> Result<Segment, AdapterError> {
    let path = Path::new(&attachment.path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let metadata = std::fs::metadata(path)
        .map_err(|error| AdapterError::protocol(format!("附件不可读（{name}）：{error}")))?;
    if metadata.len() > MAX_OUTBOUND_ATTACHMENT_BYTES {
        return Err(AdapterError::Limit {
            message: format!(
                "附件 {name} 有 {} 字节，超过 {} 字节上限",
                metadata.len(),
                MAX_OUTBOUND_ATTACHMENT_BYTES
            ),
        });
    }
    let bytes = std::fs::read(path)
        .map_err(|error| AdapterError::protocol(format!("附件不可读（{name}）：{error}")))?;
    // OneBot 11 的本地文件用 `base64://` 前缀内联，比让平台自己去读 `file://` 更可控：
    // 后者要求平台进程能访问同一个路径（NapCat 与 HBot 可能不在同一台机器上）。
    let encoded = format!("base64://{}", hbot_core::base64_encode(&bytes));
    let mut data = Map::new();
    data.insert("file".to_string(), Value::String(encoded));
    let kind = match attachment.kind {
        AttachmentKind::Image => SEGMENT_IMAGE,
        AttachmentKind::File => {
            // `file` 段没有文件名时多数实现在客户端显示成一串数字 id。
            data.insert("name".to_string(), Value::String(name.to_string()));
            SEGMENT_FILE
        }
    };
    Ok(Segment::new(kind, data))
}

/// 出站消息（正文 + 可选附件）序列化成 OneBot `message` 字段。
///
/// 没有附件时行为与 [`outbound_json`] 完全一致（纯文本走字符串形式）。有附件时
/// 一律用段数组：`reply` 段（若有）→ `text` 段（正文非空才加）→ 附件段。空正文 +
/// 附件是合法的（用户只说「把这个发给张三」）。
pub fn outbound_json_with_attachment(
    text: &str,
    reply_to: Option<&str>,
    attachment: Option<&AttachmentRef>,
) -> Result<Value, AdapterError> {
    let Some(attachment) = attachment else {
        return Ok(outbound_json(text, reply_to));
    };
    let mut segments = Vec::new();
    if let Some(id) = reply_to.filter(|id| !id.is_empty()) {
        segments.push(Segment::reply(id));
    }
    if !text.is_empty() {
        segments.push(Segment::text(text));
    }
    segments.push(attachment_segment(attachment)?);
    serde_json::to_value(MessagePayload::Segments(segments))
        .map_err(|error| AdapterError::protocol(format!("出站消息序列化失败：{error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(value: Value) -> MessagePayload {
        serde_json::from_value(value).expect("fixture parses")
    }

    #[test]
    fn renders_supported_segments_lossily() {
        let payload = segments(serde_json::json!([
            { "type": "text", "data": { "text": "hi " } },
            { "type": "at", "data": { "qq": "42" } },
            { "type": "image", "data": { "file": "a.png", "url": "http://x/a.png" } },
            { "type": "face", "data": { "id": "14" } },
            { "type": "reply", "data": { "id": "999" } }
        ]));
        assert_eq!(payload.to_plain_text(), "hi @42[图片][表情]");
        assert_eq!(payload.reply_to().as_deref(), Some("999"));
    }

    #[test]
    fn renders_unknown_segment_with_kind() {
        let payload = segments(serde_json::json!([
            { "type": "record", "data": { "file": "a.amr" } }
        ]));
        assert_eq!(payload.to_plain_text(), "[record]");
    }

    #[test]
    fn accepts_plain_string_message() {
        let payload = segments(serde_json::json!("hello"));
        assert_eq!(payload, MessagePayload::Text("hello".to_string()));
        assert_eq!(payload.to_plain_text(), "hello");
    }

    #[test]
    fn at_qq_may_be_number_or_string() {
        for qq in [serde_json::json!(42), serde_json::json!("42")] {
            let payload = segments(serde_json::json!([{ "type": "at", "data": { "qq": qq } }]));
            assert_eq!(payload.to_plain_text(), "@42");
        }
    }

    /// 附件段的字节来源是真实文件：`base64://` 前缀与内容是这里唯一的判据。
    fn temp_file(label: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "hbot-attach-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, bytes).expect("写临时附件");
        path
    }

    #[test]
    fn an_image_attachment_becomes_a_base64_segment() {
        let path = temp_file("image", b"\x89PNG");
        let reference =
            AttachmentRef::new(AttachmentKind::Image, path.to_string_lossy().into_owned());
        let value = outbound_json_with_attachment("看图", None, Some(&reference)).expect("序列化");
        let array = value.as_array().expect("有附件时必须是段数组");
        assert_eq!(array.len(), 2, "正文段 + 附件段");
        assert_eq!(array[0]["type"], "text");
        assert_eq!(array[1]["type"], "image");
        assert_eq!(array[1]["data"]["file"], "base64://iVBORw==");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_body_still_sends_the_attachment_alone() {
        let path = temp_file("bare", b"hi");
        let reference =
            AttachmentRef::new(AttachmentKind::File, path.to_string_lossy().into_owned());
        let value = outbound_json_with_attachment("", None, Some(&reference)).expect("序列化");
        let array = value.as_array().expect("段数组");
        assert_eq!(array.len(), 1, "空正文不产生 text 段");
        assert_eq!(array[0]["type"], "file");
        assert_eq!(array[0]["data"]["file"], "base64://aGk=");
        // 文件名进 `name`，否则多数客户端显示成一串 id。
        assert!(
            array[0]["data"]["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("hbot-attach-bare-"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_attachment_file_is_an_error_not_a_text_only_send() {
        let reference = AttachmentRef::new(
            AttachmentKind::Image,
            std::env::temp_dir()
                .join("hbot-attach-does-not-exist.png")
                .to_string_lossy()
                .into_owned(),
        );
        let error = outbound_json_with_attachment("看图", None, Some(&reference))
            .expect_err("读不到就该失败");
        let message = error.to_string();
        assert!(
            message.contains("hbot-attach-does-not-exist.png"),
            "错误文案要带上文件名：{message}"
        );
        // 不许把绝对路径带出去（AGENTS.md 坑 47）。
        assert!(
            !message.contains(&*std::env::temp_dir().to_string_lossy()),
            "错误文案不许带目录：{message}"
        );
    }

    #[test]
    fn an_attachment_over_the_cap_is_refused_with_the_limit() {
        let oversized = vec![0u8; (MAX_OUTBOUND_ATTACHMENT_BYTES + 1) as usize];
        let path = temp_file("huge", &oversized);
        let reference =
            AttachmentRef::new(AttachmentKind::File, path.to_string_lossy().into_owned());
        let error =
            outbound_json_with_attachment("", None, Some(&reference)).expect_err("超限就该失败");
        let message = error.to_string();
        assert!(message.contains("字节上限"), "错误文案要说上限：{message}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn build_outbound_adds_reply_segment() {
        let payload = build_outbound("pong", Some("123"));
        let json = serde_json::to_value(&payload).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!([
                { "type": "reply", "data": { "id": "123" } },
                { "type": "text", "data": { "text": "pong" } }
            ])
        );
        assert_eq!(
            build_outbound("pong", None),
            MessagePayload::Text("pong".to_string())
        );
    }

    #[test]
    fn empty_reply_id_falls_back_to_plain_text() {
        assert_eq!(
            build_outbound("x", Some("")),
            MessagePayload::Text("x".to_string())
        );
    }
}
