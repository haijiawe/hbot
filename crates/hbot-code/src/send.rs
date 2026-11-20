//! 出站能力（C3.send / C3.notify）在沙箱里的宿主侧装配。
//!
//! 契约类型、错误码与参数闸都在 `hbot_core::bot_egress`（已冻结），本模块只管四件事：
//!
//! 1. [`SendSlot`] —— `Sandbox` 上的可替换槽位（与 [`crate::jobs::JobSlot`] 同一写法：
//!    出生为空，装配方在 isolate 起来之后按清单写进去）；
//! 2. **声明闸** —— `capabilities.send` 没写就不能主动发；`capabilities.notify` 没写
//!    就不能发通知；
//! 3. **目标解析** —— `send` 的会话类型「显式优先、不猜」：调用方给了 `kind` 就用，
//!    没给就交给端口去查库，查不到由端口报 [`SendFailure::NoTarget`]；
//! 4. **一个端口两个能力** —— 通知的目标在清单里，所以 `notify` 只是「按常量目标发一条」，
//!    走的仍然是同一个 [`SendQueue`]，宿主侧只有一条出站路径。
//!
//! # 为什么端口错误不带码
//!
//! 同一个端口被两个能力共用，而它们要报的码不同（`HBOT_SEND_*` / `HBOT_NOTIFY_*`）。
//! 端口返回 [`SendFailure`]（结构化原因），由槽位包成各自能力的错误类型 ——
//! 这样「一次落库失败」在 `ctx.send` 下是 `HBOT_SEND_FAILED`、在 `ctx.notify` 下
//! 是 `HBOT_NOTIFY_FAILED`，用户不用猜自己调的是哪个。
//!
//! # 通知不许在运行时指定目标
//!
//! 见 `hbot_core::bot_egress` 的模块文档：目标能从参数来，就等于给了一段
//! 「把已读内容发到任意群」的能力，那不是通知。

use std::sync::{Arc, Mutex, PoisonError};

use hbot_core::{
    AttachmentKind, AttachmentRequest, ConversationKind, NotifyError, NotifyRequest, NotifyTarget,
    SendError, SendRequest, validate_attachment, validate_notify, validate_notify_target,
    validate_send,
};

/// 宿主没装配出站队列时的说明（`HBOT_*_UNAVAILABLE` 前缀由各自的错误类型加）。
pub(crate) const UNASSEMBLED: &str = "宿主还没有可用的出站队列（bot 未启动或平台未连接）";

/// 出站端口失败的原因（**不带错误码**，见模块文档）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendFailure {
    /// 目标会话的类型无法确定：调用方没给 `kind`，库里也没有这个会话的记录。
    NoTarget {
        /// 目标会话外部 ID。
        conversation: String,
    },
    /// 其它失败（落库、通道关闭、适配器不可用）。
    Failed {
        /// 具体原因（人话，不含错误码）。
        reason: String,
    },
}

/// 把一条合法的出站请求交出去的端口。
///
/// 实现方是 `hbot-rpc`：它把请求丢进 supervisor 的 `replies` 通道，
/// 由 outbox 统一落库、重试、进死信 —— 沙箱**不**自己攒消息（攒在内存里的消息
/// 进程一崩就没了，而"先落库再发"是 `AGENTS.md` §4.1 的硬约束）。
pub trait SendQueue: Send + Sync {
    /// 发一条消息。`request.reply_to` 恒为 `None`（主动消息）。
    ///
    /// # Errors
    ///
    /// 目标类型无法确定时报 [`SendFailure::NoTarget`]，其余失败报
    /// [`SendFailure::Failed`]。
    fn send(&self, request: &SendRequest) -> Result<(), SendFailure>;

    /// 发一条带附件的消息。
    ///
    /// 与 [`Self::send`] 走**同一条**出站路径（同一个 outbound 表、同一套重试与死信），
    /// 差别只在载荷。`request.path` 到了这里还是**相对项目的路径**：解析成绝对路径、
    /// 校验它在项目根内、检查文件存在与大小上限，都是宿主在这个方法里做的事 ——
    /// 只靠参数闸会漏掉符号链接。
    ///
    /// # Errors
    ///
    /// 与 [`Self::send`] 同：目标类型无法确定报 [`SendFailure::NoTarget`]，其余失败
    /// 报 [`SendFailure::Failed`]。
    fn send_attachment(&self, request: &AttachmentRequest) -> Result<(), SendFailure>;
}

/// 槽位内部状态。
#[derive(Default)]
struct SendSlotState {
    send_declared: bool,
    target: Option<NotifyTarget>,
    queue: Option<Arc<dyn SendQueue>>,
}

/// 出站槽位（一个 isolate 一个）。
pub(crate) struct SendSlot {
    state: Mutex<SendSlotState>,
}

impl SendSlot {
    /// 空槽位：什么都没声明、没有端口。
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(SendSlotState::default()),
        }
    }

    /// 记下 `capabilities.send` 是否声明。
    ///
    /// **每次整体覆盖**：清单是唯一真源，热重载换清单后下一次调用立刻按新值判。
    pub(crate) fn set_send_declared(&self, declared: bool) {
        self.lock().send_declared = declared;
    }

    /// 装入清单里的通知目标；`None` = 没声明 `capabilities.notify`。
    pub(crate) fn set_notify_target(&self, target: Option<NotifyTarget>) {
        self.lock().target = target;
    }

    /// 装入（或替换）出站端口。
    pub(crate) fn set_queue(&self, queue: Arc<dyn SendQueue>) {
        self.lock().queue = Some(queue);
    }

    /// 是否已经装配了端口。
    #[must_use]
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().queue.is_some()
    }

    /// `capabilities.send` 是否声明。
    #[must_use]
    pub(crate) fn is_send_declared(&self) -> bool {
        self.lock().send_declared
    }

    /// 清单里的通知目标（`None` = 没声明）。
    #[must_use]
    pub(crate) fn notify_target(&self) -> Option<NotifyTarget> {
        self.lock().target.clone()
    }

    /// 主动发一条消息：参数闸 -> 声明闸 -> 装配闸 -> 端口。
    pub(crate) fn send(
        &self,
        conversation: &str,
        text: &str,
        kind: Option<ConversationKind>,
    ) -> Result<(), SendError> {
        let mut request = SendRequest::new(conversation, text);
        request.kind = kind;
        // 参数闸在最前：正文超长不该被报成"没声明 send"。
        validate_send(&request)?;

        let (declared, queue) = {
            let state = self.lock();
            (state.send_declared, state.queue.clone())
        };
        if !declared {
            return Err(SendError::NotDeclared);
        }
        let Some(queue) = queue else {
            return Err(SendError::Unavailable {
                reason: UNASSEMBLED.to_owned(),
            });
        };
        queue.send(&request).map_err(|failure| match failure {
            SendFailure::NoTarget { conversation } => SendError::NoTarget { conversation },
            SendFailure::Failed { reason } => SendError::Failed { message: reason },
        })
    }

    /// 主动发一条带附件的消息：参数闸 -> 声明闸 -> 装配闸 -> 端口。
    ///
    /// 附件也算主动消息，所以过的是同一道 capabilities.send 闸 —— 不另开一个
    /// capabilities.attachments：多一个开关只会多一种「文本能发、文件不能发」的
    /// 半残状态，而它们最终是同一条 outbound 行。
    pub(crate) fn send_attachment(
        &self,
        conversation: &str,
        kind: Option<ConversationKind>,
        attachment: AttachmentKind,
        path: &str,
    ) -> Result<(), SendError> {
        let mut request = AttachmentRequest::new(conversation, attachment, path);
        request.kind = kind;
        validate_attachment(&request)?;

        let (declared, queue) = {
            let state = self.lock();
            (state.send_declared, state.queue.clone())
        };
        if !declared {
            return Err(SendError::NotDeclared);
        }
        let Some(queue) = queue else {
            return Err(SendError::Unavailable {
                reason: UNASSEMBLED.to_owned(),
            });
        };
        queue
            .send_attachment(&request)
            .map_err(|failure| match failure {
                SendFailure::NoTarget { conversation } => SendError::NoTarget { conversation },
                SendFailure::Failed { reason } => SendError::Failed { message: reason },
            })
    }

    /// 发一条通知：正文闸 -> 声明闸 -> 目标闸 -> 装配闸 -> 端口。
    ///
    /// 目标来自清单，所以这里**没有** `kind` 参数，也没有「猜目标」的分支。
    pub(crate) fn notify(&self, text: &str) -> Result<(), NotifyError> {
        let request = NotifyRequest::new(text);
        validate_notify(&request)?;

        let target = self.notify_target();
        let Some(target) = target else {
            return Err(NotifyError::NotDeclared);
        };
        // 清单加载时已经校验过一遍；这里再判一次是因为槽位可以被程序化装填
        // （测试、未来的云端宿主），不肯把"类型写错了"的清单当成可用目标。
        validate_notify_target(&target)?;

        let queue = self.lock().queue.clone();
        let Some(queue) = queue else {
            return Err(NotifyError::Unavailable {
                reason: UNASSEMBLED.to_owned(),
            });
        };

        let request = SendRequest::new(target.conversation.clone(), text).with_kind(target.kind);
        queue.send(&request).map_err(|failure| match failure {
            // 目标类型已由清单给定，端口不该再报 NoTarget；真报了说明端口实现
            // 有 bug，把它当普通失败报出来（保留原文以便排障），不假装成功。
            SendFailure::NoTarget { conversation } => NotifyError::Failed {
                message: format!("通知目标 {conversation:?} 的会话类型仍然无法确定"),
            },
            SendFailure::Failed { reason } => NotifyError::Failed { message: reason },
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SendSlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for SendSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SendSlot")
            .field("send", &self.is_send_declared())
            .field("notify", &self.notify_target().is_some())
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 解析生成代码给的 `kind` 字符串。
///
/// 只认四个值，其余一律 [`SendError::Invalid`]：静默忽略一个拼错的 `kind`
/// 会让宿主退化成"去查库"，而用户以为自己已经指定过了。
pub(crate) fn parse_conversation_kind(raw: &str) -> Result<ConversationKind, SendError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "private" => Ok(ConversationKind::Private),
        "group" => Ok(ConversationKind::Group),
        "channel" => Ok(ConversationKind::Channel),
        "unknown" => Ok(ConversationKind::Unknown),
        other => Err(SendError::Invalid {
            message: format!("kind 只能是 private / group / channel，收到 {other:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use super::*;

    /// 记下端口收到的请求；需要时改成一律失败。
    #[derive(Default)]
    struct Recording {
        requests: Mutex<Vec<SendRequest>>,
        attachments: Mutex<Vec<AttachmentRequest>>,
        failure: Mutex<Option<SendFailure>>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn taken(&self) -> Vec<SendRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn taken_attachments(&self) -> Vec<AttachmentRequest> {
            self.attachments
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn fail_with(&self, failure: SendFailure) {
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
        }
    }

    impl SendQueue for Recording {
        fn send_attachment(&self, request: &AttachmentRequest) -> Result<(), SendFailure> {
            self.attachments
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            match self
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        }

        fn send(&self, request: &SendRequest) -> Result<(), SendFailure> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            match self
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        }
    }

    /// 一个声明了 `send` 与 `notify`、并装好端口的槽位。
    fn assembled() -> (SendSlot, Arc<Recording>) {
        let slot = SendSlot::new();
        let port = Recording::new();
        slot.set_send_declared(true);
        slot.set_notify_target(Some(NotifyTarget {
            conversation: "-100200".into(),
            kind: ConversationKind::Group,
        }));
        slot.set_queue(port.clone());
        (slot, port)
    }

    #[test]
    fn send_passes_the_request_to_the_port() {
        let (slot, port) = assembled();
        slot.send("123456", "你好", Some(ConversationKind::Private))
            .expect("声明了且有端口就必须发得出去");
        let requests = port.taken();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].conversation, "123456");
        assert_eq!(requests[0].text, "你好");
        assert_eq!(
            requests[0].effective_kind(),
            Some(ConversationKind::Private)
        );
    }

    #[test]
    fn send_attachment_passes_the_request_to_the_port() {
        let (slot, port) = assembled();
        slot.send_attachment(
            "123456",
            Some(ConversationKind::Private),
            AttachmentKind::Image,
            "assets/logo.png",
        )
        .expect("声明了且有端口就必须发得出去");
        let requests = port.taken_attachments();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].conversation, "123456");
        assert_eq!(requests[0].attachment, AttachmentKind::Image);
        // 槽位不碰路径：相对项目的路径要原样交给宿主去核验（它才知道根在哪）。
        assert_eq!(requests[0].path, "assets/logo.png");
        assert_eq!(
            requests[0].effective_kind(),
            Some(ConversationKind::Private)
        );
        assert!(port.taken().is_empty(), "附件不该走纯文本那条端口方法");
    }

    #[test]
    fn an_attachment_is_refused_before_it_is_declared() {
        let slot = SendSlot::new();
        let error = slot
            .send_attachment("123456", None, AttachmentKind::File, "report.pdf")
            .expect_err("没声明就不能发");
        assert_eq!(error.code(), "HBOT_SEND_NOT_DECLARED");
    }

    #[test]
    fn a_bad_attachment_path_is_invalid_even_when_not_declared() {
        // 与纯文本同口径：路径坏了就该报 INVALID，别把用户引去清单加 send。
        let slot = SendSlot::new();
        let error = slot
            .send_attachment("123456", None, AttachmentKind::File, "../../etc/passwd")
            .expect_err("越界路径必须被拒");
        assert_eq!(error.code(), "HBOT_SEND_INVALID");
    }

    #[test]
    fn an_attachment_is_unavailable_when_the_host_has_no_queue() {
        let slot = SendSlot::new();
        slot.set_send_declared(true);
        let error = slot
            .send_attachment("123456", None, AttachmentKind::Image, "assets/logo.png")
            .expect_err("没有端口就必须报不可用");
        assert_eq!(error.code(), "HBOT_SEND_UNAVAILABLE");
    }

    #[test]
    fn an_attachment_port_failure_becomes_send_failed() {
        let (slot, port) = assembled();
        port.fail_with(SendFailure::Failed {
            reason: "附件读不出来".into(),
        });
        let error = slot
            .send_attachment("123456", None, AttachmentKind::File, "report.pdf")
            .expect_err("端口失败必须上报");
        assert_eq!(error.code(), "HBOT_SEND_FAILED");
        assert!(error.to_string().contains("附件读不出来"), "{error}");
    }

    #[test]
    fn send_is_refused_before_it_is_declared() {
        let slot = SendSlot::new();
        let error = slot
            .send("123456", "你好", None)
            .expect_err("没声明就不能发");
        assert_eq!(error.code(), "HBOT_SEND_NOT_DECLARED");
    }

    #[test]
    fn send_is_unavailable_when_the_host_has_no_queue() {
        let slot = SendSlot::new();
        slot.set_send_declared(true);
        let error = slot
            .send("123456", "你好", None)
            .expect_err("没有端口就必须报不可用");
        assert_eq!(error.code(), "HBOT_SEND_UNAVAILABLE");
        assert!(error.to_string().contains("出站队列"), "{error}");
    }

    #[test]
    fn bad_text_is_reported_as_invalid_even_when_not_declared() {
        // 参数闸在声明闸之前：正文坏了就该报 INVALID，不该把用户引到清单去加 send。
        let slot = SendSlot::new();
        let error = slot
            .send("123456", "   ", None)
            .expect_err("空白正文必须被拒");
        assert_eq!(error.code(), "HBOT_SEND_INVALID");
    }

    #[test]
    fn the_kind_is_optional_and_the_port_decides() {
        // 没给 kind 时请求里的 kind 是 None —— 槽位**不**填默认值，
        // 由端口去查库或报 NoTarget（「不猜」这条判据落在端口）。
        let (slot, port) = assembled();
        port.fail_with(SendFailure::NoTarget {
            conversation: "123456".into(),
        });
        let error = slot
            .send("123456", "你好", None)
            .expect_err("端口报 NoTarget 时必须原样本报");
        assert_eq!(error.code(), "HBOT_SEND_NO_TARGET");
        assert_eq!(port.taken()[0].kind, None);
    }

    #[test]
    fn a_port_failure_becomes_send_failed() {
        let (slot, port) = assembled();
        port.fail_with(SendFailure::Failed {
            reason: "通道已关闭".into(),
        });
        let error = slot
            .send("123456", "你好", None)
            .expect_err("端口失败必须上报");
        assert_eq!(error.code(), "HBOT_SEND_FAILED");
        assert!(error.to_string().contains("通道已关闭"), "{error}");
    }

    #[test]
    fn notify_is_refused_before_the_target_is_declared() {
        let slot = SendSlot::new();
        let error = slot.notify("任务跑完了").expect_err("没声明就不能发通知");
        assert_eq!(error.code(), "HBOT_NOTIFY_NOT_DECLARED");
    }

    #[test]
    fn notify_uses_the_declared_target() {
        let (slot, port) = assembled();
        slot.notify("任务跑完了").expect("声明了就必须发得出去");
        let requests = port.taken();
        assert_eq!(requests.len(), 1);
        // 目标与会话类型都必须来自清单，而不是调用方。
        assert_eq!(requests[0].conversation, "-100200");
        assert_eq!(requests[0].effective_kind(), Some(ConversationKind::Group));
        assert_eq!(requests[0].text, "任务跑完了");
    }

    #[test]
    fn a_hostile_text_is_still_caught_before_the_declaration_gate() {
        let slot = SendSlot::new();
        let error = slot.notify("   ").expect_err("空白通知必须被拒");
        assert_eq!(error.code(), "HBOT_NOTIFY_INVALID");
    }

    #[test]
    fn notify_reports_unavailable_when_no_queue_is_assembled() {
        let slot = SendSlot::new();
        slot.set_notify_target(Some(NotifyTarget {
            conversation: "-100200".into(),
            kind: ConversationKind::Group,
        }));
        let error = slot
            .notify("任务跑完了")
            .expect_err("没有端口就必须报不可用");
        assert_eq!(error.code(), "HBOT_NOTIFY_UNAVAILABLE");
    }

    #[test]
    fn a_target_without_a_concrete_kind_is_never_used() {
        // 即使有人绕过清单校验把 unknown 塞进槽位，notify 也不许拿它当目标。
        let slot = SendSlot::new();
        let port = Recording::new();
        slot.set_notify_target(Some(NotifyTarget {
            conversation: "-100200".into(),
            kind: ConversationKind::Unknown,
        }));
        slot.set_queue(port.clone());
        let error = slot.notify("任务跑完了").expect_err("unknown 目标必须被拒");
        assert_eq!(error.code(), "HBOT_NOTIFY_INVALID");
        assert!(port.taken().is_empty(), "坏目标不许落到端口上");
    }

    #[test]
    fn parse_conversation_kind_accepts_the_documented_values() {
        assert_eq!(
            parse_conversation_kind("private"),
            Ok(ConversationKind::Private)
        );
        assert_eq!(
            parse_conversation_kind("group"),
            Ok(ConversationKind::Group)
        );
        assert_eq!(
            parse_conversation_kind("channel"),
            Ok(ConversationKind::Channel)
        );
        // 大小写与首尾空白由解析器消化，别让用户为了一个空格排障。
        assert_eq!(
            parse_conversation_kind(" Group "),
            Ok(ConversationKind::Group)
        );
        assert_eq!(
            parse_conversation_kind("unknown"),
            Ok(ConversationKind::Unknown)
        );
    }

    #[test]
    fn parse_conversation_kind_rejects_typos() {
        let error = parse_conversation_kind("gruop").expect_err("拼错的 kind 必须被拒");
        assert!(error.to_string().contains("gruop"), "{error}");
        assert!(parse_conversation_kind("").is_err());
    }
}
