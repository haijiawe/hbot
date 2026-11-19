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
