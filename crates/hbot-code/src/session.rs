//! 会话等待（C2.session）在沙箱里的宿主侧装配：`ctx.session.*` 与 `onResume`。
//!
//! 契约类型、错误码与参数闸都在 `hbot_core::bot_session`（已冻结），本模块只管三件事：
//!
//! 1. [`SessionSlot`] —— `Sandbox` 上的可替换槽位（与 [`crate::jobs::JobSlot`] 同一写法：
//!    出生为空，装配方在 isolate 起来之后按清单写进去）；
//! 2. **声明闸** —— 清单 `capabilities.state.session` 没开就报 `HBOT_SESSION_NOT_DECLARED`，
//!    开了但宿主没端口就报 `HBOT_SESSION_UNAVAILABLE`（两个码分开是有意的：改清单 vs 去装配）；
//! 3. **等待端口** —— [`WaitQueue`]：真正落库的是宿主（`hbot-rpc` 落到 `bot_waits`），
//!    沙箱只负责「把一次合法的 prompt 交出去」。沙箱自己绝不攒队列：进程崩了，攒在内存里的
//!    等待就没了 —— 而那正是 `C2.session` 要修的那个旧项目缺陷（`docs/00` P0-3）。
//!
//! # 顺序：先解析参数，后判装配
//!
//! 与 `crate::runtime::jobs_api` 同一顺序，理由也相同：坏参数先报错，才不会把「键写错了」
//! 变成「宿主没装配等待后端」，也不会让一次坏注册白占一次库写。
//!
//! # 为什么这里的身份是从当前事件借来的
//!
//! 等待的归属键是 `(bot_id, conversation_id, sender_id)`，而后两个只有**当前这条消息事件**
//! 知道。所以 `build_event` 每次都把身份写进槽位（与 `JobSlot::set_current_message` 同一手法），
//! 定时器 / 作业 / 恢复事件则**显式清空**它们：那三种事件没有入站身份，不写 `conversation`
//! 就报错，而不是沿用上一个人的会话。

use std::sync::{Arc, Mutex, PoisonError};

use hbot_core::{SessionError, WaitHit, WaitSpec};

/// 宿主没装配等待后端时的说明（`HBOT_SESSION_UNAVAILABLE` 前缀由 [`unavailable`] 加）。
pub(crate) const UNASSEMBLED: &str = "宿主还没有可用的会话等待后端（bot 未启动或存储未就绪）";

/// 恢复事件没有回复目标时，`ctx.reply` 抛出的码。
///
/// 与作业的 `HBOT_JOBS_NO_TARGET` 同一条理由：回复目标来自等待行，行上没有就不知道
/// 发给谁；猜一个会话发出去收不回来。
pub(crate) const NO_TARGET_CODE: &str = "HBOT_SESSION_NO_TARGET";

/// 把一次合法的等待注册交出去的端口。
///
/// 实现方是 `hbot-rpc`：它按 `bot_waits` 的部分唯一索引兜住「同会话一个未结等待」，
/// 并把冲突翻译成 `SessionError::Pending`。
///
/// 返回 `Err` 时正文已经以 `HBOT_SESSION_*` 开头（由 [`SessionError`] 的 `Display` 生成），
/// 沙箱把它原样抛给 handler，不自己再包一层。
pub trait WaitQueue: Send + Sync {
    /// 注册一条等待，返回落库后的行 id 字符串（只用于日志与 `onResume` 的对账）。
    ///
    /// `conversation` 是当前事件的会话（`externalId`），`owner` 是发送者。
    ///
    /// # Errors
    ///
    /// 同会话已有未结等待（`HBOT_SESSION_PENDING`）或存储失败（`HBOT_SESSION_FAILED`）。
    fn register(&self, spec: &WaitSpec, conversation: &str, owner: &str) -> Result<String, String>;

    /// 读这个会话当前未结的那条等待（`ctx.session.current()`）。
    ///
    /// 没有时返回 `Ok(None)`（正常路径，不是错误）。
    fn current(&self, conversation: &str) -> Result<Option<WaitHit>, String>;

    /// 取消这个会话未结的那条等待；返回是否真的取消了一行（幂等）。
    fn cancel(&self, conversation: &str) -> Result<bool, String>;
}

/// 槽位内部状态：声明开关 + 可替换的端口 + 本事件的身份。
#[derive(Default)]
struct SessionSlotState {
    declared: bool,
    queue: Option<Arc<dyn WaitQueue>>,
    /// 本事件属于哪个会话（平台 `externalId`）；定时器 / 作业 / 恢复事件为 `None`。
    conversation: Option<String>,
    /// 本事件的发送者（`senderId`）；同样只有消息事件有。
    sender: Option<String>,
}

/// 会话等待槽位（一个 isolate 一个）。
pub(crate) struct SessionSlot {
    state: Mutex<SessionSlotState>,
}

impl SessionSlot {
    /// 空槽位：没声明、没端口、没身份。
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(SessionSlotState::default()),
        }
    }

    /// 装声明开关（`hbot.json` 的 `capabilities.state.session`）。
    pub(crate) fn set_declared(&self, declared: bool) {
        self.lock().declared = declared;
    }

    /// 装入（或替换）等待端口。
    pub(crate) fn set_queue(&self, queue: Arc<dyn WaitQueue>) {
        self.lock().queue = Some(queue);
    }

    /// 是否已经装配了端口。
    #[must_use]
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().queue.is_some()
    }

    /// 清单是否声明了会话能力。
    #[must_use]
    pub(crate) fn is_declared(&self) -> bool {
        self.lock().declared
    }

    /// 记下本事件的身份。
    ///
    /// **每次整体覆盖**，包括覆盖成 `None`：定时器 / 作业 / 恢复事件没有入站身份，
    /// 清空它才不会让一次 `prompt` 悄悄挂到上一条消息的会话上。
    pub(crate) fn set_current(&self, conversation: Option<String>, sender: Option<String>) {
        let mut state = self.lock();
        state.conversation = conversation;
        state.sender = sender;
    }

    /// 一次 `ctx.session.prompt(...)`：参数闸 -> 声明闸 -> 身份 -> 装配闸 -> 端口。
    pub(crate) fn prompt(&self, spec: &WaitSpec) -> Result<String, SessionError> {
        hbot_core::bot_session::validate_spec(spec)?;

        let (declared, queue, conversation, sender) = {
            let state = self.lock();
            (
                state.declared,
                state.queue.clone(),
                state.conversation.clone(),
                state.sender.clone(),
            )
        };
        if !declared {
            return Err(SessionError::NotDeclared {
                capability: "session",
            });
        }
        let (Some(conversation), Some(sender)) = (conversation, sender) else {
            return Err(SessionError::Invalid {
                message: String::from(
                    "ctx.session.prompt 只能在消息事件里调用：等待要有归属（哪个会话、谁在等），而定时器 / 作业 / 恢复事件没有入站身份",
                ),
            });
        };
        let Some(queue) = queue else {
            return Err(SessionError::Unavailable {
                reason: unavailable(UNASSEMBLED),
            });
        };
        queue
            .register(spec, &conversation, &sender)
            .map_err(|message| SessionError::Failed { message })
    }

    /// `ctx.session.current()`：这个会话当前未结的等待（没有就是 `null`）。
    pub(crate) fn current(&self) -> Result<Option<WaitHit>, SessionError> {
        let (declared, queue, conversation) = {
            let state = self.lock();
            (
                state.declared,
                state.queue.clone(),
                state.conversation.clone(),
            )
        };
        if !declared {
            return Err(SessionError::NotDeclared {
                capability: "session",
            });
        }
        let Some(conversation) = conversation else {
            return Err(SessionError::Invalid {
                message: String::from(
                    "ctx.session.current 只能在消息事件里调用（它读的是当前会话）",
                ),
            });
        };
        let Some(queue) = queue else {
            return Err(SessionError::Unavailable {
                reason: unavailable(UNASSEMBLED),
            });
        };
        queue
            .current(&conversation)
            .map_err(|message| SessionError::Failed { message })
    }

    /// `ctx.session.cancel()`：取消这个会话未结的等待（幂等，返回是否命中）。
    pub(crate) fn cancel(&self) -> Result<bool, SessionError> {
        let (declared, queue, conversation) = {
            let state = self.lock();
            (
                state.declared,
                state.queue.clone(),
                state.conversation.clone(),
            )
        };
        if !declared {
            return Err(SessionError::NotDeclared {
                capability: "session",
            });
        }
        let Some(conversation) = conversation else {
            return Err(SessionError::Invalid {
                message: String::from(
                    "ctx.session.cancel 只能在消息事件里调用（它取消的是当前会话的等待）",
                ),
            });
        };
        let Some(queue) = queue else {
            return Err(SessionError::Unavailable {
                reason: unavailable(UNASSEMBLED),
            });
        };
        queue
            .cancel(&conversation)
            .map_err(|message| SessionError::Failed { message })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionSlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for SessionSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionSlot")
            .field("declared", &self.is_declared())
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 没装配时的错误正文（以 `HBOT_SESSION_UNAVAILABLE` 开头）。
#[must_use]
pub fn unavailable(detail: &str) -> String {
    SessionError::Unavailable {
        reason: detail.to_owned(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hbot_core::{SessionError, WaitHit, WaitId, WaitSpec};

    use super::{SessionSlot, WaitQueue, unavailable};

    /// 记下端口收到的注册；也可以预置「已有一条 pending」。
    #[derive(Default)]
    struct Recording {
        specs: Mutex<Vec<WaitSpec>>,
        owners: Mutex<Vec<(String, String)>>,
        pending: Mutex<Option<String>>,
        failure: Mutex<Option<String>>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn count(&self) -> usize {
            self.specs.lock().unwrap().len()
        }
    }

    impl WaitQueue for Recording {
        fn register(
            &self,
            spec: &WaitSpec,
            conversation: &str,
            owner: &str,
        ) -> Result<String, String> {
            if let Some(message) = self.failure.lock().unwrap().clone() {
                return Err(message);
            }
            if let Some(key) = self.pending.lock().unwrap().clone() {
                return Err(SessionError::Pending { key }.to_string());
            }
            self.specs.lock().unwrap().push(spec.clone());
            self.owners
                .lock()
                .unwrap()
                .push((conversation.to_owned(), owner.to_owned()));
            Ok("wait-1".to_owned())
        }

        fn current(&self, _conversation: &str) -> Result<Option<WaitHit>, String> {
            Ok(None)
        }

        fn cancel(&self, _conversation: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn spec() -> WaitSpec {
        WaitSpec::new("order-no", "请把订单号发给我")
    }

    /// 槽位出生就是「没声明、没端口、没身份」，三件事分别对应三个码。
    #[test]
    fn a_fresh_slot_reports_not_declared_before_anything_else() {
        let slot = SessionSlot::new();
        assert!(!slot.is_declared());
        assert!(!slot.is_assembled());
        let error = slot.prompt(&spec()).unwrap_err();
        assert_eq!(error.code(), "HBOT_SESSION_NOT_DECLARED");
    }

    /// 声明了但没装配端口：必须是 `UNAVAILABLE`，不是 `NOT_DECLARED`。
    ///
    /// 两个码的补救动作不同（改清单 vs 去装配），混用会让用户在错误的文件里找问题。
    #[test]
    fn a_declared_but_unassembled_slot_reports_unavailable() {
        let slot = SessionSlot::new();
        slot.set_declared(true);
        slot.set_current(Some("local:default".to_owned()), Some("u1".to_owned()));
        let error = slot.prompt(&spec()).unwrap_err();
        assert_eq!(error.code(), "HBOT_SESSION_UNAVAILABLE");
        assert!(error.to_string().contains("等待"));
    }

    /// 没有身份（定时器 / 作业 / 恢复事件）时报无效，而不是挂到别人的会话上。
    #[test]
    fn a_slot_without_identity_refuses_to_prompt() {
        let slot = SessionSlot::new();
        slot.set_declared(true);
        let queue = Recording::new();
        slot.set_queue(queue.clone());
        slot.set_current(None, None);
        let error = slot.prompt(&spec()).unwrap_err();
        assert_eq!(error.code(), "HBOT_SESSION_INVALID");
        assert!(
            error.to_string().contains("消息事件"),
            "文案要说清只有消息事件能开等待，实际：{error}"
        );
        assert_eq!(queue.count(), 0, "被拒的注册不许打到端口");
    }

    /// 参数闸在身份 / 装配之前：坏参数不该变成「宿主没装配」。
    #[test]
    fn a_bad_spec_is_rejected_before_the_assembly_check() {
        let slot = SessionSlot::new();
        let error = slot.prompt(&WaitSpec::new("", "")).unwrap_err();
        assert_eq!(error.code(), "HBOT_SESSION_INVALID");
        assert!(error.to_string().contains("等待键"), "{error}");
    }

    /// 正常路径：参数与身份都齐、端口也在 → 端口拿到那条规范与两个身份。
    #[test]
    fn a_valid_prompt_reaches_the_port_with_the_current_identity() {
        let slot = SessionSlot::new();
        slot.set_declared(true);
        let queue = Recording::new();
        slot.set_queue(queue.clone());
        slot.set_current(Some("local:default".to_owned()), Some("u1".to_owned()));
        assert_eq!(slot.prompt(&spec()).expect("prompt"), "wait-1");
        assert_eq!(queue.count(), 1);
        let owners = queue.owners.lock().unwrap().clone();
        assert_eq!(owners, vec![("local:default".to_owned(), "u1".to_owned())]);
    }

    /// 端口说「同会话已有一条 pending」时，码要能被上层原样看到。
    #[test]
    fn a_pending_port_error_keeps_its_code_and_the_blocking_key() {
        let slot = SessionSlot::new();
        slot.set_declared(true);
        let queue = Recording::new();
        *queue.pending.lock().unwrap() = Some("order-no".to_owned());
        slot.set_queue(queue.clone());
        slot.set_current(Some("local:default".to_owned()), Some("u1".to_owned()));
        let error = slot.prompt(&spec()).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("HBOT_SESSION_PENDING"),
            "挡路的业务判据不能被包成 FAILED，实际：{text}"
        );
        assert!(text.contains("order-no"), "要点名挡路的那条，实际：{text}");
    }

    /// `set_current` 每次整体覆盖：不写身份的下一事件不许继承上一条消息的身份。
    #[test]
    fn clearing_the_identity_does_not_inherit_the_previous_message() {
        let slot = SessionSlot::new();
        slot.set_declared(true);
        let queue = Recording::new();
        slot.set_queue(queue.clone());
        slot.set_current(Some("local:default".to_owned()), Some("u1".to_owned()));
        slot.set_current(None, None);
        let error = slot.prompt(&spec()).unwrap_err();
        assert_eq!(error.code(), "HBOT_SESSION_INVALID");
        assert_eq!(queue.count(), 0);
    }

    /// `current` 与 `cancel` 也要走同一条声明闸。
    #[test]
    fn current_and_cancel_are_gated_the_same_way() {
        let slot = SessionSlot::new();
        assert_eq!(
            slot.current().unwrap_err().code(),
            "HBOT_SESSION_NOT_DECLARED"
        );
        assert_eq!(
            slot.cancel().unwrap_err().code(),
            "HBOT_SESSION_NOT_DECLARED"
        );
    }

    /// 没装配时的提示要指到「会话等待后端」这件事（用户才知道去装配而不是改代码）。
    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        let message = unavailable("宿主还没有可用的会话等待后端");
        assert!(message.starts_with("HBOT_SESSION_UNAVAILABLE"));
    }

    /// `WaitHit` 的 id 只用来对账；这里钉住它是 `WaitId` 而不是裸字符串。
    #[test]
    fn a_hit_carries_a_typed_wait_id() {
        let hit = WaitHit {
            id: "00000000-0000-0000-0000-000000000000"
                .parse::<WaitId>()
                .expect("nil uuid"),
            key: "order-no".to_owned(),
            prompt: "请把订单号发给我".to_owned(),
            owner: "u1".to_owned(),
        };
        assert_eq!(hit.key, "order-no");
    }
}
