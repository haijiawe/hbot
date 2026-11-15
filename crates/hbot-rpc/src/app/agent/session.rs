//! Agent 会话：内存里的历史、取消令牌与忙闲状态。
//!
//! ## 一个会话一把锁
//!
//! `history` / `cancel` / `busy` 放在**同一把锁**后面：`agent_send` 的
//! "检查忙闲 → 追加用户消息 → 拿令牌"必须是原子的，否则两次并发 `agent_send`
//! 会同时通过检查，把两条用户消息交给两个 `run_turn`（`AGENTS.md`：每会话串行）。
//!
//! ## 取消不留后遗症
//!
//! 一轮结束时**换一个新的取消令牌**（不是复位旧标志）：旧令牌已经被 `cancel()`
//! 永久置位，若下一轮继续用它，`run_turn` 一进循环就退出——会话看起来"卡死"。
//!
//! ## 历史里放什么
//!
//! `run_turn` 每轮自己拼 system prompt，所以历史**不含** system 消息；结束时写回
//! 的是本轮完整对话（用户 → 助手 → 工具结果 → … → 最终助手），下一轮原样接上。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hbot_agent::{CancellationToken, ChatMessage};
use hbot_core::{HbotError, SessionId};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 会话内部状态。
struct SessionInner {
    history: Vec<ChatMessage>,
    cancel: CancellationToken,
    busy: bool,
}

/// 一个 Agent 会话（进程内，不落库：M1 的重启恢复另做）。
pub struct SessionState {
    inner: Mutex<SessionInner>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// 新建会话。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SessionInner {
                history: Vec::new(),
                cancel: CancellationToken::new(),
                busy: false,
            }),
        }
    }

    /// 开一轮：追加用户消息，返回本轮的取消令牌与历史快照。
    ///
    /// 上一个回合还没结束时返回 `-32002`——**不排队**：排队会让 UI 里两条消息的
    /// 顺序与执行顺序不一致，调起来比报错难得多。
    pub fn begin_turn(
        &self,
        text: &str,
    ) -> Result<(CancellationToken, Vec<ChatMessage>), HbotError> {
        let mut inner = lock(&self.inner);
        if inner.busy {
            return Err(HbotError::invalid("该会话上一轮还没结束：先取消或等它完成"));
        }
        inner.history.push(ChatMessage::user(text));
        inner.busy = true;
        Ok((inner.cancel.clone(), inner.history.clone()))
    }

    /// 结束一轮：写回历史（`None` 表示保持原样），换新令牌，复位忙标志。
    pub fn end_turn(&self, history: Option<Vec<ChatMessage>>) {
        let mut inner = lock(&self.inner);
        if let Some(history) = history {
            inner.history = history;
        }
        inner.cancel = CancellationToken::new();
        inner.busy = false;
    }

    /// 是否有一轮正在跑。
    pub fn is_busy(&self) -> bool {
        lock(&self.inner).busy
    }

    /// 当前（或下一次）回合的取消令牌。
    pub fn cancel_token(&self) -> CancellationToken {
        lock(&self.inner).cancel.clone()
    }

    /// 取消当前回合；返回是否确实有回合在跑。
    ///
    /// 即使没有在跑也把令牌置位是**有害的**（下一轮一启动就退出），所以只在
    /// 忙的时候取消。
    pub fn cancel_turn(&self) -> bool {
        let inner = lock(&self.inner);
        if !inner.busy {
            return false;
        }
        inner.cancel.cancel();
        true
    }

    /// 历史条数（诊断/测试用）。
    pub fn history_len(&self) -> usize {
        lock(&self.inner).history.len()
    }

    /// 历史快照（诊断/测试用）。
    pub fn history(&self) -> Vec<ChatMessage> {
        lock(&self.inner).history.clone()
    }
}

/// 一轮的收尾守卫：无论正常返回、报错还是任务被 abort，都会把会话复位。
///
/// 没有它，任何一条提前 `return` 的分支都会让 `busy` 永久为真——UI 上表现为
/// "这个会话再也发不出消息了"。
pub struct TurnGuard<'a> {
    session: &'a SessionState,
    finished: bool,
}

impl<'a> TurnGuard<'a> {
    /// 接管一个已经 `begin_turn` 的会话。
    pub fn new(session: &'a SessionState) -> Self {
        Self {
            session,
            finished: false,
        }
    }

    /// 正常收尾：写回本轮历史。
    pub fn finish(mut self, history: Vec<ChatMessage>) {
        self.session.end_turn(Some(history));
        self.finished = true;
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.session.end_turn(None);
        }
    }
}

/// 进程级会话表。
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionId, Arc<SessionState>>>,
}

impl SessionRegistry {
    /// 空会话表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取会话，没有就建一个。
    pub fn get_or_create(&self, session_id: SessionId) -> Arc<SessionState> {
        let mut sessions = lock(&self.sessions);
        Arc::clone(
            sessions
                .entry(session_id)
                .or_insert_with(|| Arc::new(SessionState::new())),
        )
    }

    /// 取会话（不存在时 `None`）。
    pub fn get(&self, session_id: SessionId) -> Option<Arc<SessionState>> {
        lock(&self.sessions).get(&session_id).map(Arc::clone)
    }

    /// 会话数量。
    pub fn len(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// 是否没有会话。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 所有会话 id（换项目时逐个取消用）。
    pub fn ids(&self) -> Vec<SessionId> {
        lock(&self.sessions).keys().copied().collect()
    }

    /// 取消所有正在跑的回合，返回取消了几个。
    pub fn cancel_all(&self) -> usize {
        let sessions: Vec<Arc<SessionState>> =
            lock(&self.sessions).values().map(Arc::clone).collect();
        sessions
            .iter()
            .filter(|session| session.cancel_turn())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::nil())
    }

    #[test]
    fn second_turn_is_rejected_while_the_first_is_running() {
        let session = SessionState::new();
        let (_token, history) = session.begin_turn("你好").expect("第一轮");
        assert_eq!(history.len(), 1);
        assert!(session.is_busy());

        let error = session.begin_turn("又一句").expect_err("并发必须被拒");
        assert_eq!(error.code(), -32002);
        assert_eq!(session.history_len(), 1, "被拒的那句不该进历史");
    }

    #[test]
    fn end_turn_rearms_a_fresh_token() {
        let session = SessionState::new();
        let (token, _history) = session.begin_turn("你好").expect("第一轮");
        token.cancel();
        assert!(token.is_cancelled());

        session.end_turn(None);
        assert!(!session.is_busy());
        let (next, _history) = session.begin_turn("下一轮").expect("第二轮");
        assert!(!next.is_cancelled(), "新一轮必须拿到没被取消的令牌");
    }

    #[test]
    fn cancel_only_touches_a_running_turn() {
        let session = SessionState::new();
        assert!(!session.cancel_turn(), "空闲时取消不应置位令牌");

        let (token, _history) = session.begin_turn("你好").expect("第一轮");
        assert!(session.cancel_turn());
        assert!(token.is_cancelled());
    }

    #[test]
    fn guard_resets_busy_on_drop() {
        let session = SessionState::new();
        let (_token, _history) = session.begin_turn("你好").expect("第一轮");
        {
            let _guard = TurnGuard::new(&session);
        }
        assert!(!session.is_busy(), "守卫 drop 后必须复位");
    }

    #[test]
    fn guard_finish_writes_history() {
        let session = SessionState::new();
        let (_token, _history) = session.begin_turn("你好").expect("第一轮");
        {
            let guard = TurnGuard::new(&session);
            guard.finish(vec![
                ChatMessage::user("你好"),
                ChatMessage::assistant_text("在"),
            ]);
        }
        assert!(!session.is_busy());
        assert_eq!(session.history_len(), 2);
    }

    #[test]
    fn registry_returns_the_same_session() {
        let registry = SessionRegistry::new();
        let first = registry.get_or_create(session_id());
        let second = registry.get_or_create(session_id());
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(registry.len(), 1);
        assert!(registry.get(session_id()).is_some());
    }
}
