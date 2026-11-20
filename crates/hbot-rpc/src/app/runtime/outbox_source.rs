//! [`PendingOutboxSource`] 的 store 实现：重启后把上次没发出去的 `outbound` 行捞回来续发。
//!
//! ## 为什么必须按 bot 过滤
//!
//! `Store::list_pending_outbound` 是**全局**扫描（`WHERE state='pending' ORDER BY id`），
//! 先取窗口再在内存里按 `bot_id` 过滤会静默丢消息：别的 bot 攒了 1024 条 pending 时，
//! 本 bot 的行根本进不了窗口，于是"续发"看起来成功、实际一条没发。
//! 所以这里用的是 SQL 层就带 `bot_id` 条件的
//! [`Store::list_pending_outbound_for_bot`](hbot_store::Store::list_pending_outbound_for_bot)。
//!
//! ## 为什么还要查 conversations
//!
//! `outbound` 行只存 `conversation_id`（UUID），而适配器需要**平台侧**的会话 id
//! （`send_group_msg` 的 group_id / `send_private_msg` 的 user_id）。两者之间的映射只在
//! `conversations.external_id` 里，所以每条 pending 都要回查一次会话行。查不到不是"跳过
//! 就算了"——那说明数据被外力破坏了（外键本该拦住），必须作为真错误返回。

use std::sync::Arc;

use hbot_bot::{ConversationRef, PendingOutbound, PendingOutboxSource};
use hbot_core::{BotId, HbotError};
use hbot_store::Store;

use crate::app::runtime::conversation_kind_from_str;
use crate::app::store_failed;

/// 一次续发最多捞多少条（与 `OutboxConfig::capacity` 同量级）。
///
/// 上限存在的意义是内存：`outbound` 是持久表，`pending` 可能积累到任意大，
/// 而续发先把它们读进内存再逐条塞进有界 outbox 通道。
pub const RECOVER_LIMIT: u32 = 1024;

/// 从 `outbound` 表读未发送行的 [`PendingOutboxSource`]。
pub struct StorePendingOutbox {
    store: Arc<Store>,
    limit: u32,
}

impl StorePendingOutbox {
    /// 用默认上限（[`RECOVER_LIMIT`]）构造。
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            limit: RECOVER_LIMIT,
        }
    }

    /// 覆盖单次续发上限（0 会被当作 1：一次都不捞等于假装续发成功）。
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit.max(1);
        self
    }
}

impl PendingOutboxSource for StorePendingOutbox {
    fn pending(&self, bot_id: &BotId) -> Result<Vec<PendingOutbound>, HbotError> {
        let rows = self
            .store
            .list_pending_outbound_for_bot(bot_id, self.limit)
            .map_err(store_failed)?;

        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let conversation = self
                .store
                .get_conversation(&row.conversation_id)
                .map_err(store_failed)?
                .ok_or_else(|| {
                    HbotError::internal(format!(
                        "outbound {} 指向的会话 {} 不存在：无法还原发送目标，续发中止",
                        row.id, row.conversation_id
                    ))
                })?;
            // 先解析附件（借用 `row`），再 move 其余字段：`reply_to` 在字面量里会把它借住。
            let attachment = attachment_of(&row)?;
            pending.push(PendingOutbound {
                id: row.id,
                bot_id: row.bot_id,
                conversation: ConversationRef::new(
                    conversation.external_id,
                    conversation_kind_from_str(&conversation.kind),
                ),
                text: row.text,
                reply_to: row.reply_to,
                attachment,
                attempts: row.attempts,
            });
        }
        Ok(pending)
    }
}

/// 把 `outbound.attachment_json` 还原成领域引用。
///
/// 解析失败**不**降级成纯文本：那会把「发这个文件」静默变成「发一段空正文」，
/// 用户既收不到文件也看不到错误。这里返回错误，由 supervisor 的启动恢复流程记日志
/// 并把这一批续发整体失败暴露出来（`AGENTS.md` §7.1 坑 22：不许有「会恢复」的谎话）。
fn attachment_of(
    row: &hbot_store::OutboundRow,
) -> Result<Option<hbot_core::AttachmentRef>, HbotError> {
    let Some(raw) = row.attachment_json.as_deref() else {
        return Ok(None);
    };
    hbot_core::AttachmentRef::from_json(raw)
        .map(Some)
        .map_err(|error| HbotError::internal(error.to_string()))
}

impl std::fmt::Debug for StorePendingOutbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorePendingOutbox")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}
