//! 出站端口的宿主实现：把 `ctx.send` / `ctx.notify` 交回 supervisor 的回复通道。
//!
//! ## 为什么复用 `replies` 而不是自己写 `outbound` 行
//!
//! `SandboxHandler.replies` 是这条 bot **唯一**的出口：handler 的返回值、定时器的
//! `ctx.reply`、主动消息全走它，到 `Outbox::enqueue` 才落 `outbound` 表。绕过去自己插一行
//! 会立刻长出第二份「目标怎么构造」的逻辑（`reply_to` 该不该填、`target_json` 放什么、
//! `conversation_id` 怎么派生），而两份总有对不上的一天。
//!
//! `Outbox::enqueue` 里的 `conversation_id` 是按 `(bot_id, external_id)` **确定性派生**的，
//! 但 `outbound.conversation_id` 有外键，所以发送前必须保证 `conversations` 行存在
//! （见 [`StoreSendQueue::ensure_target`]）—— 主动消息的常态正是「平台从没推过这个会话」。
//!
//! ## `reply_to` 恒为 `None`
//!
//! `ctx.send` 是**主动**消息：它不绑定任何入站事件，所以不得冒用某条旧消息的 ID 当
//! 引用（`AGENTS.md` §4.1）。被动回复仍走 handler 返回值那条路。
//!
//! ## 类型为什么可能要查库
//!
//! `ConversationRef` 必须带 `ConversationKind`，而调用点可以不写 `kind`。这时候只能
//! 去 `conversations` 表查这个会话（它就是适配器落地消息时记下的答案）；查不到、或者库里的
//! 值不是三种已知类型之一，就报 `NO_TARGET` 并要求调用点补 `kind` —— **不猜**。
//! 猜成 `private` 而目标其实是群，就是把消息发给错误的收件人。

use std::path::Path;
use std::sync::Arc;

use hbot_bot::event::ConversationRef;
use hbot_bot::outbox::OutboundRequest;
use hbot_code::send::{SendFailure, SendQueue};
use hbot_core::{BotId, Clock, ConversationKind, NotifyTarget, SendRequest};
use hbot_store::Store;
use tokio::sync::mpsc;

use crate::app::store_failed;

/// 出站端口的 store 实现（一个 bot 一个）。
pub struct StoreSendQueue {
    bot_id: BotId,
    store: Arc<Store>,
    /// supervisor 的回复通道：handler 返回值与主动消息共用同一条队列，顺序不会互相插队。
    replies: mpsc::Sender<OutboundRequest>,
    /// 补建会话行时写 `created_at`（与其它能力一致：时间只从注入的时钟取）。
    clock: Arc<dyn Clock>,
}

impl StoreSendQueue {
    /// 装配一个出站端口。
    #[must_use]
    pub fn new(
        bot_id: BotId,
        store: Arc<Store>,
        replies: mpsc::Sender<OutboundRequest>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bot_id,
            store,
            replies,
            clock,
        }
    }

    /// 解析这次发送要用的会话类型，并确保目标会话行存在。
    ///
    /// 顺序是刻意的：**库里有的以库为准**（那是平台真实说过的），库里没有才用调用点给
    /// 的 `kind`；两处都没有就报 `NO_TARGET` 要求补 `kind` —— 不猜（猜成 `private` 而目标
    /// 其实是群，就是把消息发给错误的人）。
    ///
    /// 行不存在时补建：`outbound.conversation_id` 有外键，缺行会在出站那一刻撞
    /// `FOREIGN KEY constraint failed`，而那时 `ctx.send` 早就返回成功 —— 用户只会看到
    /// 「发了但对方没收到」。会话 ID 走 `hbot_bot::conversation_id_for` 的确定性派生，
    /// 与 supervisor 写 inbound 时用的是同一个函数，所以补建出来的行就是那一行。
    fn ensure_target(&self, request: &SendRequest) -> Result<ConversationKind, SendFailure> {
        let existing = self
            .store
            .get_conversation_by_external(&self.bot_id, &request.conversation)
            .map_err(|error| SendFailure::Failed {
                reason: store_failed(error).to_string(),
            })?;

        let stored = existing
            .as_ref()
            .map(|row| crate::app::runtime::conversation_kind_from_str(&row.kind))
            .filter(|kind| *kind != ConversationKind::Unknown);
        let Some(kind) = stored.or_else(|| request.effective_kind()) else {
            return Err(SendFailure::NoTarget {
                conversation: request.conversation.clone(),
            });
        };

        if existing.is_none() {
            let row = hbot_store::Conversation {
                id: hbot_bot::conversation_id_for(&self.bot_id, &request.conversation),
                bot_id: self.bot_id,
                external_id: request.conversation.clone(),
                kind: crate::app::runtime::conversation_kind_str(kind).to_owned(),
                created_at: self.clock.now().unix_millis(),
            };
            self.store
                .insert_conversation_if_absent(&row)
                .map_err(|error| SendFailure::Failed {
                    reason: store_failed(error).to_string(),
                })?;
        }
        Ok(kind)
    }
}
impl SendQueue for StoreSendQueue {
    fn send(&self, request: &SendRequest) -> Result<(), SendFailure> {
        let kind = self.ensure_target(request)?;
        let outbound = OutboundRequest {
            conversation: ConversationRef::new(request.conversation.clone(), kind),
            text: request.text.clone(),
            reply_to: None,
        };
        // 这里是同步上下文（`hbot-code` 已把端口调用放进 `spawn_blocking`），所以用
        // `blocking_send`：它在该线程上阻塞等待容量，不会把 reactor 卡住。
        self.replies
            .blocking_send(outbound)
            .map_err(|error| SendFailure::Failed {
                reason: format!("出站通道已关闭，消息没能交给 supervisor：{error}"),
            })
    }
}

impl std::fmt::Debug for StoreSendQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreSendQueue")
            .field("bot_id", &self.bot_id)
            .finish_non_exhaustive()
    }
}

/// 清单里声明的出站能力：`capabilities.send` 与 `capabilities.notify`。
///
/// 与 `declared_jobs` / `declared_schedules` 同一口径：清单是项目文件，改它除了指纹没有
/// 别的信号，所以每次装配时现读一遍，不在构造时快照（缓存会让「改了清单但 isolate 没换」
/// 变成静默失效，见 `AGENTS.md` §7 坑 59）。
#[must_use]
pub fn declared_egress(root: &Path) -> (bool, Option<NotifyTarget>) {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return (false, None);
    };
    (
        manifest.capabilities.send(),
        manifest.capabilities.notify().cloned(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn declared_egress_of_a_missing_manifest_is_nothing() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(declared_egress(&root), (false, None));
    }
}
