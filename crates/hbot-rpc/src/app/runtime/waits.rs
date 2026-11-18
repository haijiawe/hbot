//! 会话等待的宿主实现：把 `ctx.session.prompt` 落进 `bot_waits` 表。
//!
//! `hbot-code` 的 [`hbot_code::session::WaitQueue`] 只声明「把一次合法的注册交出去」
//! 这个动作；真正写库的是这里。为什么不在沙箱里攒等待表：进程崩了攒在内存里的等待
//! 就没了 —— 那正是旧项目 `wait_input` 的失败形状（`docs/00-assessment.md` P0-3），
//! 也是 `C2.session` 存在的理由。
//!
//! ## 三道判据
//!
//! 沙箱侧已经过了参数闸与声明闸，这里只做落库前还剩的三件事：
//!
//! 1. **容量闸**：这个 bot 未结等待到 [`MAX_WAITS_PER_BOT`] 就报 `HBOT_SESSION_FAILED`
//!    （有上界才叫背压）；
//! 2. **会话行**：`bot_waits.conversation_id` 是外键语义的本地 ID，必须从
//!    `(bot_id, external_id)` 查出来。查不到说明这个会话还没收到过消息 —— 但那是
//!    **不可能**的：`prompt` 只在消息事件里调用，而消息一定先落库。所以查不到是
//!    内部错误，明确报出来而不是拿一个假 ID 顶上；
//! 3. **落库**：`(bot_id, conversation_id)` 的部分唯一索引兜住「同会话一个未结等待」，
//!    撞上时把 `StoreError::WaitPending` 转成 `SessionError::Pending`（带挡路那条的键）。
//!
//! ## 为什么这个端口是阻塞的
//!
//! 底层 `Store` 是同步 API（`rusqlite`）。`hbot-code` 的 `session_api` 已经把端口调用
//! 放进 `tokio::task::spawn_blocking`，所以这里只管同步写完并返回。

use std::path::Path;
use std::sync::Arc;

use hbot_code::session::WaitQueue;
use hbot_core::{BotId, Clock, IdGenerator, MAX_WAITS_PER_BOT, SessionError, WaitHit, WaitSpec};
use hbot_store::{NewWait, Store, StoreError};

use crate::app::store_failed;

/// 会话等待的 store 实现（一个 bot 一个）。
pub struct StoreWaitQueue {
    bot_id: BotId,
    store: Arc<Store>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl StoreWaitQueue {
    /// 装配一个等待端口。
    #[must_use]
    pub fn new(
        bot_id: BotId,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bot_id,
            store,
            ids,
            clock,
        }
    }

    /// 按 `external_id` 找本 bot 的会话行；找不到返回 `None`。
    fn conversation_of(
        &self,
        external_id: &str,
    ) -> Result<Option<hbot_store::Conversation>, String> {
        self.store
            .get_conversation_by_external(&self.bot_id, external_id)
            .map_err(|error| store_failed(error).to_string())
    }
}

impl WaitQueue for StoreWaitQueue {
    fn register(&self, spec: &WaitSpec, conversation: &str, owner: &str) -> Result<String, String> {
        let now = self.clock.now();
        let now_ms = now.unix_millis();
        let timeout = i64::try_from(spec.effective_timeout_ms()).unwrap_or(i64::MAX);
        let deadline = now_ms.saturating_add(timeout);

        // 容量闸：先数一遍未结等待。判据是 `MAX_WAITS_PER_BOT`，不是「看看会不会太小」——
        // 没有上界时，一个写错循环的 handler 可以把库撑到几万行。
        let active = self
            .store
            .count_active_waits_for_bot(&self.bot_id)
            .map_err(|error| store_failed(error).to_string())?;
        if active >= u32::try_from(MAX_WAITS_PER_BOT).unwrap_or(u32::MAX) {
            return Err(SessionError::Failed {
                message: format!(
                    "这个 bot 已有 {active} 条未结等待（上限 {MAX_WAITS_PER_BOT}）：等它们被回答或超时再开新的"
                ),
            }
            .to_string());
        }

        let Some(row) = self.conversation_of(conversation)? else {
            // 不可能：`prompt` 只在消息事件里调用，而消息先落库才会有那个事件。
            // 报内部错误而不是编一个会话 ID —— 编出来的等待永远唤不醒。
            return Err(SessionError::Failed {
                message: format!(
                    "找不到会话 `{conversation}` 的行：等待必须挂在一个已存在的会话上"
                ),
            }
            .to_string());
        };

        let wait = NewWait {
            id: self.ids.wait_id(),
            conversation_id: row.id,
            conversation_external_id: row.external_id.clone(),
            owner: owner.to_owned(),
            key: spec.key.clone(),
            prompt: spec.prompt.clone(),
            scope: spec.scope.as_str().to_owned(),
            deadline,
            created_at: now_ms,
        };
        match self.store.register_wait(&self.bot_id, &wait) {
            Ok(id) => Ok(id.to_string()),
            Err(StoreError::WaitPending { key }) => {
                // 业务判据，不是存储故障：`HBOT_SESSION_PENDING` 让用户知道该先回哪个。
                Err(SessionError::Pending { key }.to_string())
            }
            Err(error) => Err(store_failed(error).to_string()),
        }
    }

    fn current(&self, conversation: &str) -> Result<Option<WaitHit>, String> {
        let Some(row) = self.conversation_of(conversation)? else {
            return Ok(None);
        };
        let found = self
            .store
            .current_wait(&self.bot_id, &row.id)
            .map_err(|error| store_failed(error).to_string())?;
        Ok(found.map(|row| WaitHit {
            id: row.id,
            key: row.key,
            prompt: row.prompt,
            owner: row.owner,
        }))
    }

    fn cancel(&self, conversation: &str) -> Result<bool, String> {
        let Some(row) = self.conversation_of(conversation)? else {
            return Ok(false);
        };
        let now = self.clock.now().unix_millis();
        self.store
            .cancel_wait(&self.bot_id, &row.id, now)
            .map_err(|error| store_failed(error).to_string())
    }
}

impl std::fmt::Debug for StoreWaitQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreWaitQueue")
            .field("bot_id", &self.bot_id)
            .finish_non_exhaustive()
    }
}

/// 清单里是否声明了 `capabilities.state.session`（宿主侧读它决定装不装等待端口）。
///
/// 与 `declared_jobs` 同一口径：清单是「要不要用」的唯一真源，宿主只回答「能不能跑」。
#[must_use]
pub fn declared_session(root: &Path) -> bool {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return false;
    };
    manifest.capabilities.session()
}
