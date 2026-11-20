//! 出站能力（C3.send / C3.notify）的契约类型。
//!
//! 两个能力共用一个出口（outbound 行 + 发送器），区别只在**目标从哪来**：
//!
//! - send：目标由调用方显式给出（`ctx.send(conversationId, text, { kind })`）；
//! - notify：目标写在项目清单里（`hbot.json` 的 `capabilities.notify`），
//!   调用方只给正文（`ctx.notify(text)`）。
//!
//! 为什么 notify 不让调用方给目标：它的语义是「把这条通知发给项目指定的
//! 管理员/工单会话」。目标一旦能在运行时指定，一段被注入的代码就能把 bot 读到的
//! 东西发到任意群 —— 那是外发数据泄漏，不是通知。所以目标必须是清单里的常量。
//!
//! 为什么 send 也不许猜会话类型：主动消息没有入站事件可继承
//! （`ConversationKind`），把群号猜成私聊就是发给另一个人。所以类型「显式优先、
//! 不猜」：调用方给了就用，没给就由宿主查 `conversations` 表，查不到直接报错
//! [`SendError::NoTarget`] 并提示补 `kind`，绝不给一个默认值蒙混过去。

use std::fmt;

use crate::domain::ConversationKind;

/// 一条主动消息正文的字节上限（UTF-8）。
///
/// 平台侧对单条消息各有上限，这里取一个保守的统一值：超过它的文本应该由项目自己
/// 分段发送。定上限的目的不是「平台收不下」，而是**别让一次 handler 调用把进程
/// 内存写穿** —— 正文最终会落库、进通道、进日志。
pub const MAX_SEND_TEXT_BYTES: usize = 8192;

/// 一条通知正文的字节上限（UTF-8）。与 [`MAX_SEND_TEXT_BYTES`] 同口径。
pub const MAX_NOTIFY_TEXT_BYTES: usize = 8192;

/// 目标会话外部 ID 的字节上限（UTF-8）。
///
/// 平台 ID 实测都在 64 字节以内（QQ 群号 5-12 位、频道 ID 32 字节、
/// Telegram chat id 是整数），128 是留余量后的闸。
pub const MAX_CONVERSATION_ID_BYTES: usize = 128;

/// 一条主动发送请求。
///
/// 由生成代码的 `ctx.send(...)` 构造，交给宿主的出站队列；`reply_to` 恒为
/// `None` —— 主动消息不得冒用旧事件 ID（`AGENTS.md` §4.1）。要被动回复用
/// `ctx.reply`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendRequest {
    /// 目标会话的平台外部 ID。
    pub conversation: String,
    /// 会话类型；`None`（或 [`ConversationKind::Unknown`]）= 调用方没给，
    /// 宿主需要去查 `conversations` 表，查不到就报 [`SendError::NoTarget`]。
    pub kind: Option<ConversationKind>,
    /// 正文。
    pub text: String,
}

impl SendRequest {
    /// 构造一条请求（不带会话类型）。
    #[must_use]
    pub fn new(conversation: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            conversation: conversation.into(),
            kind: None,
            text: text.into(),
        }
    }

    /// 显式指定会话类型（「不猜」的那一半）。
    #[must_use]
    pub fn with_kind(mut self, kind: ConversationKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// 真正生效的会话类型：`None` 与 `Unknown` 都算「调用方没给」。
    ///
    /// 把 `Unknown` 归进「没给」而不是当成一个真类型，是因为它本身就是
    /// 「平台没说」的占位值；拿它去构造目标等于用默认值猜。
    #[must_use]
    pub fn effective_kind(&self) -> Option<ConversationKind> {
        match self.kind {
            None | Some(ConversationKind::Unknown) => None,
            Some(kind) => Some(kind),
        }
    }
}

/// 一条通知请求。
///
/// 目标不在请求里：它来自清单的 [`NotifyTarget`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyRequest {
    /// 正文。
    pub text: String,
}

impl NotifyRequest {
    /// 构造一条通知。
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// 清单里声明的通知目标（`capabilities.notify`）。
///
/// `kind` 是**必填**的：通知是主动消息，没有入站事件可以继承类型，
/// 而发错类型（把群号当私聊）的后果是发给另一个人。清单少写这个字段要在
/// 加载时就失败，不能等到第一次 `ctx.notify` 才发现。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyTarget {
    /// 目标会话的平台外部 ID。
    pub conversation: String,
    /// 会话类型；不许是 [`ConversationKind::Unknown`]。
    pub kind: ConversationKind,
}

/// 主动发送的错误。
///
/// 与其它能力的码表一致：`NOT_DECLARED` 是「清单没开」（改 `hbot.json`），
/// `UNAVAILABLE` 是「宿主没装配」（去 IDE 设置里配），两者的补救动作不同，
/// 混用会让用户去错误的文件里找问题（`AGENTS.md` 坑 59）。
///
/// `NO_TARGET` 是这两个能力独有的：它既不是配置错也不是宿主缺失，而是
/// 「这个会话宿主从来没见过」，补救动作是调用时显式给 `kind`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// 清单没声明 `capabilities.send`。
    NotDeclared,
    /// 宿主没有装配出站队列（无头模式、测试、`hbot-bench`）。
    Unavailable {
        /// 具体原因（人话）。
        reason: String,
    },
    /// 参数不合法（正文、会话 ID）。
    Invalid {
        /// 具体哪里错了。
        message: String,
    },
    /// 目标会话类型不能确定：调用方没给 `kind`，库里也没有这个会话的记录。
    NoTarget {
        /// 目标会话外部 ID。
        conversation: String,
    },
    /// 发送期失败（入队、落库、通道已关闭）。
    Failed {
        /// 具体原因。
        message: String,
    },
}

impl SendError {
    /// 稳定的错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared => "HBOT_SEND_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_SEND_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_SEND_INVALID",
            Self::NoTarget { .. } => "HBOT_SEND_NO_TARGET",
            Self::Failed { .. } => "HBOT_SEND_FAILED",
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 每条都以错误码开头：生成代码侧只能靠字符串分辨原因（与 JobError 同口径）。
        write!(f, "{}", self.code())?;
        match self {
            Self::NotDeclared => {
                write!(f, ": 项目没有声明 capabilities.send，不能主动发消息")
            }
            Self::Unavailable { reason } => write!(f, ": {reason}"),
            Self::Invalid { message } | Self::Failed { message } => write!(f, ": {message}"),
            Self::NoTarget { conversation } => write!(
                f,
                ": 无法确定会话 {conversation:?} 的类型；这个会话还没有收到过消息，\
                 请在调用时显式给出 kind（private/group/channel）"
            ),
        }
    }
}

impl std::error::Error for SendError {}

/// 通知的错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotifyError {
    /// 清单没声明 `capabilities.notify`。
    NotDeclared,
    /// 宿主没有装配出站队列。
    Unavailable {
        /// 具体原因（人话）。
        reason: String,
    },
    /// 参数不合法（正文、目标会话）。
    Invalid {
        /// 具体哪里错了。
        message: String,
    },
    /// 发送期失败（入队、落库、通道已关闭）。
    Failed {
        /// 具体原因。
        message: String,
    },
}

impl NotifyError {
    /// 稳定的错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared => "HBOT_NOTIFY_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_NOTIFY_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_NOTIFY_INVALID",
            Self::Failed { .. } => "HBOT_NOTIFY_FAILED",
        }
    }
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())?;
        match self {
            Self::NotDeclared => {
                write!(f, ": 项目没有声明 capabilities.notify，不能发通知")
            }
            Self::Unavailable { reason } => write!(f, ": {reason}"),
            Self::Invalid { message } | Self::Failed { message } => write!(f, ": {message}"),
        }
    }
}

impl std::error::Error for NotifyError {}

/// 校验一个平台会话外部 ID。
///
/// 返回的是人话（不含错误码）：两个能力把它包进各自的错误类型，
/// 免得同一条规则在两处各写一遍。
///
/// # Errors
///
/// 空、纯空白、超长、含控制字符、首尾有空白都报错。
pub fn validate_conversation(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("目标会话 ID 不能为空".into());
    }
    if id.len() > MAX_CONVERSATION_ID_BYTES {
        return Err(format!(
            "目标会话 ID 有 {} 字节，超过上限 {MAX_CONVERSATION_ID_BYTES}",
            id.len()
        ));
    }
    if id.chars().any(|c| c.is_control()) {
        return Err(format!("目标会话 ID {id:?} 含控制字符"));
    }
    if id.trim() != id {
        return Err(format!("目标会话 ID {id:?} 首尾有空白"));
    }
    Ok(())
}

/// 校验正文（两个能力共用同一套规则，只有上限与名称不同）。
///
/// 允许 `\n` 与 `\t`：多行消息是常态。其余控制字符一律拒 ——
/// 它们只会让日志与平台协议各自解析出不同结果。
fn check_text(text: &str, max: usize, label: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{label}不能为空"));
    }
    if text.len() > max {
        return Err(format!(
            "{label}有 {} 字节，超过上限 {max}（请自己分段发送）",
            text.len()
        ));
    }
    if let Some(bad) = text
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(format!("{label}含不允许的控制字符 {:#x}", u32::from(bad)));
    }
    Ok(())
}

/// 校验一条主动发送请求。
///
/// # Errors
///
/// 正文或目标会话不合法都报 [`SendError::Invalid`]。
pub fn validate_send(request: &SendRequest) -> Result<(), SendError> {
    validate_conversation(&request.conversation)
        .map_err(|message| SendError::Invalid { message })?;
    check_text(&request.text, MAX_SEND_TEXT_BYTES, "消息正文")
        .map_err(|message| SendError::Invalid { message })
}

/// 校验一条通知请求。
///
/// # Errors
///
/// 正文不合法报 [`NotifyError::Invalid`]。
pub fn validate_notify(request: &NotifyRequest) -> Result<(), NotifyError> {
    check_text(&request.text, MAX_NOTIFY_TEXT_BYTES, "通知正文")
        .map_err(|message| NotifyError::Invalid { message })
}

/// 校验清单里声明的通知目标。
///
/// # Errors
///
/// 会话 ID 不合法、或类型写成 `unknown` 都报 [`NotifyError::Invalid`]。
pub fn validate_notify_target(target: &NotifyTarget) -> Result<(), NotifyError> {
    validate_conversation(&target.conversation)
        .map_err(|message| NotifyError::Invalid { message })?;
    if target.kind == ConversationKind::Unknown {
        return Err(NotifyError::Invalid {
            message: format!(
                "通知目标 {:?} 的会话类型写成了 unknown；主动消息没有入站事件可以继承类型，\
                 必须写清 private/group/channel",
                target.conversation
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_send() -> SendRequest {
        SendRequest::new("123456", "你好")
    }

    #[test]
    fn send_request_records_the_explicit_kind() {
        let request = ok_send().with_kind(ConversationKind::Group);
        assert_eq!(request.effective_kind(), Some(ConversationKind::Group));
        assert_eq!(validate_send(&request), Ok(()));
    }

    #[test]
    fn unknown_kind_counts_as_not_given() {
        // 显式传 Unknown 与不传等价：两者都要求宿主去查库，而不是拿 Unknown 当目标。
        let request = ok_send().with_kind(ConversationKind::Unknown);
        assert_eq!(request.kind, Some(ConversationKind::Unknown));
        assert_eq!(request.effective_kind(), None);
    }

    #[test]
    fn empty_text_is_invalid() {
        let request = SendRequest::new("123456", "   ");
        let error = validate_send(&request).expect_err("空白正文必须被拒");
        assert_eq!(error.code(), "HBOT_SEND_INVALID");
        assert!(error.to_string().contains("不能为空"), "{error}");
    }

    #[test]
    fn text_over_the_limit_is_invalid() {
        let request = SendRequest::new("123456", "a".repeat(MAX_SEND_TEXT_BYTES + 1));
        let error = validate_send(&request).expect_err("超长正文必须被拒");
        // 报错要带上真实字节数，用户才知道该分几段。
        assert!(
            error
                .to_string()
                .contains(&(MAX_SEND_TEXT_BYTES + 1).to_string()),
            "{error}"
        );
    }

    #[test]
    fn text_at_the_limit_is_accepted() {
        let request = SendRequest::new("123456", "a".repeat(MAX_SEND_TEXT_BYTES));
        assert_eq!(validate_send(&request), Ok(()));
    }

    #[test]
    fn multiline_text_is_allowed() {
        // 换行与制表符是正常内容，不能被当成控制字符拒掉。
        let request = SendRequest::new("123456", "第一行\n第二行\t缩进");
        assert_eq!(validate_send(&request), Ok(()));
    }

    #[test]
    fn control_characters_are_rejected() {
        let request = SendRequest::new("123456", "带\u{7}响铃");
        let error = validate_send(&request).expect_err("控制字符必须被拒");
        assert!(error.to_string().contains("控制字符"), "{error}");
    }

    #[test]
    fn conversation_is_required() {
        let request = SendRequest::new("   ", "你好");
        let error = validate_send(&request).expect_err("空会话 ID 必须被拒");
        assert_eq!(error.code(), "HBOT_SEND_INVALID");
    }

    #[test]
    fn conversation_id_over_the_limit_is_invalid() {
        let request = SendRequest::new("1".repeat(MAX_CONVERSATION_ID_BYTES + 1), "你好");
        assert!(validate_send(&request).is_err());
    }

    #[test]
    fn conversation_id_with_surrounding_space_is_invalid() {
        // 首尾空白多半是粘贴带进来的，静默 trim 会让「发给谁」变得不可预期。
        let error = validate_conversation(" 123456").expect_err("首尾空白必须被拒");
        assert!(error.contains("空白"), "{error}");
    }

    #[test]
    fn empty_notify_text_is_invalid() {
        let error = validate_notify(&NotifyRequest::new("")).expect_err("空白通知必须被拒");
        assert_eq!(error.code(), "HBOT_NOTIFY_INVALID");
    }

    #[test]
    fn notify_target_needs_a_concrete_kind() {
        let target = NotifyTarget {
            conversation: "123456".into(),
            kind: ConversationKind::Unknown,
        };
        let error = validate_notify_target(&target).expect_err("unknown 类型必须被拒");
        assert!(error.to_string().contains("unknown"), "{error}");
    }

    #[test]
    fn notify_target_with_a_concrete_kind_passes() {
        let target = NotifyTarget {
            conversation: "-100123".into(),
            kind: ConversationKind::Channel,
        };
        assert_eq!(validate_notify_target(&target), Ok(()));
    }

    #[test]
    fn notify_target_round_trips_through_the_manifest_shape() {
        // 清单里写的是 camelCase；字段名对不上会在加载期待炸，这里先用序列化钉住形状。
        let json = r#"{"conversation":"-100123","kind":"channel"}"#;
        let target: NotifyTarget = serde_json::from_str(json).expect("清单形状必须可解析");
        assert_eq!(target.conversation, "-100123");
        assert_eq!(target.kind, ConversationKind::Channel);
        // 多余的字段必须被拒（与 Capabilities 的 deny_unknown_fields 同口径）。
        assert!(
            serde_json::from_str::<NotifyTarget>(r#"{"conversation":"1","kind":"group","x":1}"#)
                .is_err()
        );
    }

    #[test]
    fn every_error_code_is_prefixed_and_distinct() {
        let send = [
            SendError::NotDeclared,
            SendError::Unavailable {
                reason: "无".into(),
            },
            SendError::Invalid {
                message: "无".into(),
            },
            SendError::NoTarget {
                conversation: "1".into(),
            },
            SendError::Failed {
                message: "无".into(),
            },
        ];
        for error in &send {
            assert!(error.code().starts_with("HBOT_SEND_"), "{}", error.code());
            assert!(error.to_string().starts_with(error.code()), "{error}");
        }
        let notify = [
            NotifyError::NotDeclared,
            NotifyError::Unavailable {
                reason: "无".into(),
            },
            NotifyError::Invalid {
                message: "无".into(),
            },
            NotifyError::Failed {
                message: "无".into(),
            },
        ];
        for error in &notify {
            assert!(error.code().starts_with("HBOT_NOTIFY_"), "{}", error.code());
            assert!(error.to_string().starts_with(error.code()), "{error}");
        }
        // 两个码族不许撞车：生成代码靠前缀分辨是哪个能力。
        assert!(
            !send
                .iter()
                .any(|error| error.code().starts_with("HBOT_NOTIFY_"))
        );
        assert!(
            !notify
                .iter()
                .any(|error| error.code().starts_with("HBOT_SEND_"))
        );
    }

    #[test]
    fn no_target_error_tells_the_caller_to_pass_kind() {
        let error = SendError::NoTarget {
            conversation: "g1".into(),
        };
        let text = error.to_string();
        assert!(text.contains("kind"), "{text}");
        assert!(text.contains("g1"), "{text}");
    }
}

/// 附件路径的字节上限（UTF-8）。
///
/// 路径是**项目内相对路径**（`asserts/logo.png` 这种），不是绝对路径：绝对路径等于把
/// 宿主机器的目录结构写进项目，换一台机器就发不出去；更要紧的是它绕过了「项目根内」
/// 这条边界（与 `C3.file` 的路径闸同一个理由）。
pub const MAX_ATTACHMENT_PATH_BYTES: usize = 512;

/// 一个附件在发送时的**平台无关**分类。
///
/// 只有两种，因为平台侧真正不同的处理也只有两种：图片按图片段发（可内联预览），
/// 其它一律按文件发。刻意**不做**音频/视频/表情分类：「按扩展名猜 MIME」是本项目
/// 反复拒绝的那类猜测，猜错的表现是对方收到一个打不开的文件，而不是一个报错。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    /// 图片（`ctx.sendImage`）。
    Image,
    /// 任意文件（`ctx.sendFile`）。
    File,
}

impl AttachmentKind {
    /// 落库与日志用的稳定字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }

    /// 从落库值解析；不认识的值返回 `None`（**不**猜成 `File`）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "image" => Some(Self::Image),
            "file" => Some(Self::File),
            _ => None,
        }
    }
}

/// 一条带附件的主动发送请求。
///
/// 与 [[`SendRequest`]] 同源（同一个出站行、同一条重试与回执路径），区别只有两点：
/// 正文可以为空，且多了一个 `attachment` 与 `path`。
///
/// 为什么不把附件塞进 `SendRequest`：那会让「发文本」这条最常用的路径也背上
/// 路径校验与磁盘 IO 的参数，而它的两个字段（正文/会话）本来就已经冻结。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRequest {
    /// 目标会话的平台外部 ID。
    pub conversation: String,
    /// 会话类型；语义与 [[`SendRequest::kind`]] 完全一样（显式优先、不猜）。
    pub kind: Option<ConversationKind>,
    /// 附件分类。
    pub attachment: AttachmentKind,
    /// 项目根内的相对路径（POSIX 风格或平台原生都接受，但必须是相对的）。
    pub path: String,
}

impl AttachmentRequest {
    /// 构造一条请求（不带会话类型）。
    #[must_use]
    pub fn new(
        conversation: impl Into<String>,
        attachment: AttachmentKind,
        path: impl Into<String>,
    ) -> Self {
        Self {
            conversation: conversation.into(),
            kind: None,
            attachment,
            path: path.into(),
        }
    }

    /// 显式指定会话类型。
    #[must_use]
    pub fn with_kind(mut self, kind: ConversationKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// 真正生效的会话类型（`Unknown` 与 `None` 都算没给，与 `SendRequest` 同口径）。
    #[must_use]
    pub fn effective_kind(&self) -> Option<ConversationKind> {
        match self.kind {
            None | Some(ConversationKind::Unknown) => None,
            Some(kind) => Some(kind),
        }
    }
}

/// 落库形态的附件引用：**已经解析好**的绝对路径 + 分类。
///
/// 与 [`AttachmentRequest`] 的区别：那是「用户代码给了什么」（相对路径、可能越界），
/// 这是「宿主核验并冻结之后是什么」（绝对路径、一定在项目根内且文件存在）。
/// 出站行里存的就是它，重启续发时不需要重新解析路径。
///
/// 为什么不存相对路径：续发时项目可能已经换了根目录（`projects.path` 可变），
/// 重新解析会指向另一个文件；而附件的语义是「发这个文件」。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentRef {
    /// 附件分类。
    pub kind: AttachmentKind,
    /// 解析后的绝对路径（宿主核验过的）。
    pub path: String,
}

impl AttachmentRef {
    /// 构造一条引用。
    #[must_use]
    pub fn new(kind: AttachmentKind, path: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
        }
    }

    /// 序列化成出站行的 `attachment_json`。
    ///
    /// 不用 `unwrap()`：这两个字段只有字符串与枚举，序列化不可能失败；真失败了说明
    /// 类型定义被改坏，那是编译期就该红的事。用 `expect` 也违反库代码约定，所以给
    /// 一个不可能走到的兜底值（空对象），由调用方的 `from_json` 报错。
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// 从出站行的 `attachment_json` 解析。
    ///
    /// # Errors
    ///
    /// 字段缺失、类型不对或分类值不认识时返回 [`SendError::Invalid`]。
    pub fn from_json(raw: &str) -> Result<Self, SendError> {
        serde_json::from_str(raw).map_err(|error| SendError::Invalid {
            message: format!("附件描述无法解析（{error}）：{raw}"),
        })
    }
}

/// 校验一条附件请求的**纯参数**部分（不碰磁盘）。
///
/// 这里只做「形状」判断：路径必须是相对的、不许有 `..` 段、不许有 NUL、长度不超限。
/// 真正「解析到项目根内 + 文件存在 + 大小不超限」由宿主侧在**入队前**做一次
/// （那里才知道项目根与真实文件大小），而且必须做 —— 只靠这里会漏掉符号链接。
///
/// # Errors
///
/// 参数不合法时返回 [[`SendError::Invalid`]]。
pub fn validate_attachment(request: &AttachmentRequest) -> Result<(), SendError> {
    if request.conversation.trim().is_empty() {
        return Err(SendError::Invalid {
            message: "附件的目标会话 ID 不能为空".to_owned(),
        });
    }
    if request.conversation.len() > MAX_CONVERSATION_ID_BYTES {
        return Err(SendError::Invalid {
            message: format!(
                "目标会话 ID 超过 {} 字节上限（实际 {}）",
                MAX_CONVERSATION_ID_BYTES,
                request.conversation.len()
            ),
        });
    }

    let path = request.path.as_str();
    if path.trim().is_empty() {
        return Err(SendError::Invalid {
            message: format!("{} 的路径不能为空", function_name(request.attachment)),
        });
    }
    if path.len() > MAX_ATTACHMENT_PATH_BYTES {
        return Err(SendError::Invalid {
            message: format!(
                "附件路径超过 {} 字节上限（实际 {}）",
                MAX_ATTACHMENT_PATH_BYTES,
                path.len()
            ),
        });
    }
    if path.contains('\0') {
        return Err(SendError::Invalid {
            message: "附件路径里不能有 NUL 字节".to_owned(),
        });
    }
    if is_absolute_like(path) {
        return Err(SendError::Invalid {
            message: format!("附件路径必须是项目根内的相对路径，不能是绝对路径：{path:?}"),
        });
    }
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(SendError::Invalid {
            message: format!("附件路径里不能有 .. 段（会逃出项目根）：{path:?}"),
        });
    }
    Ok(())
}

/// 这个附件是哪个 API 发出来的（错误文案要用），不参与判定。
fn function_name(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::Image => "sendImage",
        AttachmentKind::File => "sendFile",
    }
}

/// 三种「看起来是相对路径、其实是绝对路径」的形状。
///
/// 与 `hbot-workspace` 的 `has_drive_prefix` 同口径，但**不依赖那个 crate**：
/// core 不许依赖 workspace（依赖方向）。
fn is_absolute_like(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    // `C:foo` 也是「盘符相对」，Windows 上它照样能跳出项目根。
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    fn request(conversation: &str, kind: AttachmentKind, path: &str) -> AttachmentRequest {
        AttachmentRequest::new(conversation, kind, path)
    }

    #[test]
    fn attachment_kind_round_trips_and_refuses_unknown_values() {
        for kind in [AttachmentKind::Image, AttachmentKind::File] {
            assert_eq!(AttachmentKind::parse(kind.as_str()), Some(kind));
        }
        // 不认识的值必须是 None：猜成 File 会让一张图静默发成文件。
        for raw in ["", "Image", "video", "image ", "file\n"] {
            assert_eq!(AttachmentKind::parse(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn a_nested_relative_path_is_accepted() {
        let req = request("123456", AttachmentKind::Image, "assets/img/logo.png");
        assert_eq!(validate_attachment(&req), Ok(()));
        // Windows 风格的分隔符也接受：项目在不同平台上写的路径习惯不同。
        let req = request("123456", AttachmentKind::File, "assets\\img\\logo.png");
        assert_eq!(validate_attachment(&req), Ok(()));
    }

    #[test]
    fn absolute_paths_are_rejected_in_every_shape() {
        for path in [
            "/etc/passwd",
            "\\\\server\\share\\a.png",
            "C:\\Windows\\win.ini",
            "c:relative-but-still-outside.png",
        ] {
            let req = request("1", AttachmentKind::File, path);
            let error = validate_attachment(&req).expect_err("绝对路径必须被拒");
            assert_eq!(error.code(), "HBOT_SEND_INVALID");
            assert!(error.to_string().contains("绝对路径"), "{error}");
        }
    }

    #[test]
    fn parent_segments_are_rejected() {
        for path in [
            "../secret.png",
            "assets/../../secret.png",
            "..\\secret.png",
            "..",
        ] {
            let req = request("1", AttachmentKind::File, path);
            let error = validate_attachment(&req).expect_err(".. 段必须被拒");
            assert_eq!(error.code(), "HBOT_SEND_INVALID");
            assert!(error.to_string().contains(".."), "{error}");
        }
        // 名字里含两个点但不是 .. 段的不该被误伤。
        let req = request("1", AttachmentKind::File, "assets/a..b.png");
        assert_eq!(validate_attachment(&req), Ok(()));
    }

    #[test]
    fn empty_and_oversized_parameters_are_rejected() {
        let empty_conversation = request("", AttachmentKind::File, "a.png");
        assert_eq!(
            validate_attachment(&empty_conversation).unwrap_err().code(),
            "HBOT_SEND_INVALID"
        );

        let empty_path = request("1", AttachmentKind::Image, "   ");
        let error = validate_attachment(&empty_path).unwrap_err();
        assert!(error.to_string().contains("sendImage"), "{error}");

        let long_path = "a".repeat(MAX_ATTACHMENT_PATH_BYTES + 1);
        let error =
            validate_attachment(&request("1", AttachmentKind::File, &long_path)).unwrap_err();
        assert!(error.to_string().contains("上限"), "{error}");

        let long_conversation = "1".repeat(MAX_CONVERSATION_ID_BYTES + 1);
        let error =
            validate_attachment(&request(&long_conversation, AttachmentKind::File, "a.png"))
                .unwrap_err();
        assert!(error.to_string().contains("上限"), "{error}");
    }

    #[test]
    fn a_nul_byte_in_the_path_is_rejected() {
        let error =
            validate_attachment(&request("1", AttachmentKind::File, "a\0b.png")).unwrap_err();
        assert!(error.to_string().contains("NUL"), "{error}");
    }

    #[test]
    fn effective_kind_treats_unknown_as_not_given() {
        let req = request("1", AttachmentKind::File, "a.png");
        assert_eq!(req.effective_kind(), None);
        assert_eq!(
            req.clone()
                .with_kind(ConversationKind::Unknown)
                .effective_kind(),
            None
        );
        assert_eq!(
            req.with_kind(ConversationKind::Group).effective_kind(),
            Some(ConversationKind::Group)
        );
    }
}
