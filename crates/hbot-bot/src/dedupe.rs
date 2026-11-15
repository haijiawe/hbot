//! 入站去重与落库。
//!
//! 主链路的第一段（`AGENTS.md` §4.1）：
//!
//! ```text
//! adapter → normalize → INSERT inbound (UNIQUE bot_id, external_id) → 入队
//! ```
//!
//! 这里的顺序是硬约束：
//!
//! 1. **先** `ensure_conversation`（写 `inbound` 之前 FK 目标必须存在）；
//! 2. **再** `insert_inbound_if_new`：`None` = 重复投递，直接 ACK（不执行、不入队）；
//!    `Some(id)` = 首次，返回可入队的 [`InboundMessage`]；
//! 3. 执行成功后写 `processed_at`（[`Inbox::mark_processed`]）。
//!
//! 幂等键是 `inbound.id`（生成代码里的 `ctx.messageId`）。重复投递之所以安全，是因为
//! 唯一约束在**落库这一层**，而不是内存里的去重表：进程重启后仍然成立。

use std::sync::Arc;

use hbot_core::{
    BotId, Clock, ConversationId, HbotError, IdGenerator, InboundId, InboundMessage, MessageId,
};
use hbot_store::{Conversation, NewInbound, Store};

use crate::event::{ConversationRef, PlatformEvent};
use crate::ids::conversation_id_for;
use crate::store_ops::{is_conversation_conflict, run_store};

/// 一次入站判定结果。
#[derive(Clone, Debug)]
pub enum Admission {
    /// 首次见到这条事件；已落库，可以入队执行。
    New(Box<InboundMessage>),
    /// 重复投递（`(bot_id, external_id)` 已存在）：ACK 即可，不执行。
    Duplicate {
        /// 重复事件的平台 ID，便于日志与 UI 对账。
        external_id: String,
    },
}

/// 入站去重 + 落库 + 会话补建。
///
/// 所有方法都会把同步的 store 调用丢进 `spawn_blocking`（见 [`crate::store_ops`]）。
pub struct Inbox {
    store: Arc<Store>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl Inbox {
    /// 由组合根注入的依赖构造。
    pub fn new(store: Arc<Store>, ids: Arc<dyn IdGenerator>, clock: Arc<dyn Clock>) -> Self {
        Self { store, ids, clock }
    }

    /// 确保 `conversations` 行存在，返回本地会话 ID。
    ///
    /// 会话 ID 由 `(bot_id, external_id)` 确定性派生（[`conversation_id_for`]），因此
    /// “插入或已存在”是幂等的：跨进程重启也不会换 ID，更不会在写 `inbound` 时踩外键。
    pub async fn ensure_conversation(
        &self,
        bot_id: &BotId,
        conversation: &ConversationRef,
    ) -> Result<ConversationId, HbotError> {
        let conversation_id = conversation_id_for(bot_id, &conversation.external_id);
        let row = Conversation {
            id: conversation_id,
            bot_id: *bot_id,
            external_id: conversation.external_id.clone(),
            kind: conversation.kind_str().to_owned(),
            created_at: self.clock.now().unix_millis(),
        };
        let store = Arc::clone(&self.store);
        match run_store("insert_conversation", move || {
            store.insert_conversation(&row)
        })
        .await
        {
            Ok(()) => Ok(conversation_id),
            Err(error) if is_conversation_conflict(&error) => Ok(conversation_id),
            Err(error) => Err(error),
        }
    }

    /// 判定并落库一条入站事件。
    pub async fn admit(
        &self,
        bot_id: &BotId,
        event: &PlatformEvent,
    ) -> Result<Admission, HbotError> {
        let conversation_id = self
            .ensure_conversation(bot_id, &event.conversation)
            .await?;
        let received_at = event.received_at.unwrap_or_else(|| self.clock.now());
        let row = NewInbound {
            id: self.ids.inbound_id(),
            bot_id: *bot_id,
            external_id: event.external_id.clone(),
            conversation_id,
            payload_json: encode_payload(&event.payload)?,
            received_at: received_at.unix_millis(),
        };
        let inbound_id = row.id;
        let store = Arc::clone(&self.store);
        let inserted = run_store("insert_inbound_if_new", move || {
            store.insert_inbound_if_new(row)
        })
        .await?;

        match inserted {
            None => Ok(Admission::Duplicate {
                external_id: event.external_id.clone(),
            }),
            Some(id) => {
                // `insert_inbound_if_new` 回显行主键；与分配值不一致说明存储层违约，
                // 记录后仍按回显值走（以库为准），不静默吞。
                if id.as_uuid() != inbound_id.as_uuid() {
                    tracing::warn!(
                        expected = %inbound_id,
                        actual = %id,
                        "inbound id returned by store differs from allocated id"
                    );
                }
                Ok(Admission::New(Box::new(InboundMessage {
                    id: MessageId::from_uuid(*id.as_uuid()),
                    bot_id: *bot_id,
                    conversation_id,
                    external_id: event.external_id.clone(),
                    sender_id: event.sender_id.clone(),
                    payload: event.payload.clone(),
                    received_at,
                    processed_at: None,
                })))
            }
        }
    }

    /// 标记入站行已处理（写 `processed_at`）。
    ///
    /// 返回 `false` 表示没有命中行（例如 inbound 行已被清理）；调用方应记录但不当作
    /// handler 失败——消息确实处理过了。
    pub async fn mark_processed(&self, message_id: &MessageId) -> Result<bool, HbotError> {
        let inbound_id = InboundId::from_uuid(*message_id.as_uuid());
        let at = self.clock.now().unix_millis();
        let store = Arc::clone(&self.store);
        run_store("mark_inbound_processed", move || {
            store.mark_inbound_processed(&inbound_id, at)
        })
        .await
    }
}

/// 载荷序列化失败属于“适配器给了不可序列化的值”，按内部错误上报，不吞。
fn encode_payload(payload: &serde_json::Value) -> Result<String, HbotError> {
    serde_json::to_string(payload)
        .map_err(|error| HbotError::internal(format!("encode inbound payload failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_encoding_round_trips() {
        let payload = serde_json::json!({ "text": "你好", "n": 1 });
        let encoded = encode_payload(&payload).expect("encodes");
        let decoded: serde_json::Value = serde_json::from_str(&encoded).expect("decodes");
        assert_eq!(decoded, payload);
    }
}
