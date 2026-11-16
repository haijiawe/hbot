//! 被动回复（[`QqSender`]）：`msg_seq` 维护 + 主动推送明确拒绝。
//!
//! # 被动回复的协议形状（实测确认可用的路径）
//!
//! ```text
//! POST https://api.sgroup.qq.com/v2/groups/{group_openid}/messages
//! POST https://api.sgroup.qq.com/v2/users/{user_openid}/messages
//! authorization: QQBot <access_token>
//!
//! {"content":"…","msg_type":0,"msg_id":"<事件 d.id>","msg_seq":1}
//! ```
//!
//! 两条硬规则：
//!
//! 1. **被动回复必须带 `msg_id`**（就是事件的 `d.id`），不带就成了主动推送。
//! 2. 同一个 `msg_id` 下 `msg_seq` 必须**自增**。平台按
//!    `(msg_id, msg_seq)` 去重，重复的 `msg_seq` 会被判为重复消息而被丢弃 ——
//!    所以「同一句话发两次」不是「发两条」，而是第二条被平台吃掉。
//!
//! `msg_seq` 表用**有界** LRU（默认 512 条，[`crate::platforms::qq::QqConfig::msg_seq_capacity`]）：
//! 被动回复只在事件后的一小段时间内有效（**具体有效期未核实**），所以没必要为整个
//! 进程生命周期保留每条消息的计数；有界也避免长跑进程无限占内存。
//!
//! # 主动推送：不实现，且不假装成功
//!
//! 不带 `msg_id` 的主动推送有严格限额（**具体额度未核实**），而且失败形态与被动回复
//! 不同。本适配器**不实现**它：遇到主动推送路径直接返回
//! [`crate::error::AdapterError::Unsupported`]，绝不伪造一个「成功」的
//! [`SendReceipt`] —— 上层看到成功就会以为用户收到了消息。
//!
//! # 错误里不许有凭据
//!
//! 状态码映射在 [`crate::platforms::qq::api`]，那边只保留状态码与平台数字 `code`；
//! 这里拼的错误串只含路径与会话 ID。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use hbot_core::domain::ConversationKind;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::AdapterError;
use crate::normalize::NormalizedInbound;
use crate::platforms::qq::api::ApiClient;

/// 被动回复成功后的回执。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    /// 平台返回的新消息 ID（响应里的 `id`；平台没给时为 `None`）。
    pub message_id: Option<String>,
    /// 本次用的 `msg_seq`（同一个 `msg_id` 下从 1 开始自增）。
    pub msg_seq: u32,
    /// 被动回复绑定的那条入站消息 ID（事件的 `d.id`）。
    pub reply_to: String,
}

/// 带 token 的被动回复发送器。
///
/// `Clone` 是便宜的（内部 `Arc`），可以从 [`crate::platforms::qq::QqClient`] 取出来后
/// 交给别的任务长期持有。
#[derive(Clone)]
pub struct QqSender {
    api: Arc<ApiClient>,
    seq: Arc<Mutex<MsgSeqTable>>,
}

impl std::fmt::Debug for QqSender {
    /// 刻意只打类型名：内部持有 API 客户端（间接持有 access token），
    /// 任何 Debug 输出都不应该成为凭据泄露的通道。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqSender").finish_non_exhaustive()
    }
}

impl QqSender {
    /// 从 API 客户端与容量构造（一般由 [`crate::platforms::qq::QqClient::start`] 调）。
    pub(crate) fn new(api: Arc<ApiClient>, capacity: usize) -> Self {
        Self {
            api,
            seq: Arc::new(Mutex::new(MsgSeqTable::new(capacity))),
        }
    }

    /// 群被动回复：`POST /v2/groups/{group_openid}/messages`。
    pub async fn send_group_passive(
        &self,
        group_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        self.send_passive("groups", group_openid, msg_id, content)
            .await
    }

    /// 单聊被动回复：`POST /v2/users/{openid}/messages`。
    pub async fn send_c2c_passive(
        &self,
        user_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        self.send_passive("users", user_openid, msg_id, content)
            .await
    }

    /// 按归一化入站消息的会话类型自动选路径回复。
    ///
    /// - `kind == Group` → `/v2/groups/{conversation}/messages`
    /// - `kind == Private` → `/v2/users/{conversation}/messages`
    /// - `kind == Channel` / `Unknown` → [`AdapterError::Unsupported`]（频道发送路径与
    ///   凭据形态都未核实，不猜）
    /// - 不会话 ID 或没有 `reply_to` → 同样是 `Unsupported`，不是「主动推送」
    pub async fn reply(
        &self,
        message: &NormalizedInbound,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        let msg_id = message.reply_to.as_deref().ok_or_else(|| {
            AdapterError::unsupported(
                "qq passive reply requires the inbound message id (`d.id`); \
                 active push without `msg_id` is not implemented",
            )
        })?;
        if message.conversation_external_id.trim().is_empty() {
            return Err(AdapterError::unsupported(
                "qq passive reply requires a conversation openid",
            ));
        }
        match message.kind {
            ConversationKind::Group => {
                self.send_group_passive(&message.conversation_external_id, msg_id, content)
                    .await
            }
            ConversationKind::Private => {
                self.send_c2c_passive(&message.conversation_external_id, msg_id, content)
                    .await
            }
            ConversationKind::Channel | ConversationKind::Unknown => {
                Err(AdapterError::unsupported(
                    "qq passive reply is only implemented for group and c2c messages; \
                     channel messages need a different endpoint that has not been verified",
                ))
            }
        }
    }

    /// 群主动推送：**不实现**。
    ///
    /// 主动推送（不带 `msg_id`）有严格的限额且失败形态不同，本适配器刻意不提供。
    /// 这里返回 `Unsupported` 而不是「看起来成功」的回执。
    pub async fn send_group_active(
        &self,
        _group_openid: &str,
        _content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        Err(AdapterError::unsupported(
            "qq active push (no `msg_id`) is not implemented; only passive replies are supported",
        ))
    }

    /// 单聊主动推送：**不实现**（原因同 [`Self::send_group_active`]）。
    pub async fn send_c2c_active(
        &self,
        _user_openid: &str,
        _content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        Err(AdapterError::unsupported(
            "qq active push (no `msg_id`) is not implemented; only passive replies are supported",
        ))
    }

    /// 真正发请求：拼路径、取 `msg_seq`、解析回执。
    async fn send_passive(
        &self,
        scope: &str,
        openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        if openid.trim().is_empty() {
            return Err(AdapterError::protocol(
                "qq passive reply requires an openid",
            ));
        }
        if msg_id.trim().is_empty() {
            return Err(AdapterError::protocol(
                "qq passive reply requires the inbound message id (`d.id`)",
            ));
        }
        if content.trim().is_empty() {
            return Err(AdapterError::protocol(
                "qq passive reply content must not be empty",
            ));
        }
        let msg_seq = self.seq.lock().await.next_seq(msg_id);
        let body = serde_json::json!({
            "content": content,
            "msg_type": 0,
            "msg_id": msg_id,
            "msg_seq": msg_seq,
        });
        let path = format!("/v2/{scope}/{openid}/messages");
        let response = self.api.post_json(&path, &body).await?;
        Ok(SendReceipt {
            message_id: response
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            msg_seq,
            reply_to: msg_id.to_owned(),
        })
    }
}

/// `msg_id → 已用到的 msg_seq` 的有界 LRU。
///
/// 用 `HashMap` + `VecDeque` 手写（容量几百，线性移动的代价可以忽略），
/// 不额外引入 LRU 依赖。
struct MsgSeqTable {
    capacity: usize,
    used: HashMap<String, u32>,
    order: VecDeque<String>,
}

impl MsgSeqTable {
    fn new(capacity: usize) -> Self {
        Self {
            // 0 会让表永远为空、msg_seq 恒为 1（平台判重复），所以兜到 1。
            capacity: capacity.max(1),
            used: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// 取下一条 `msg_seq`（同一个 `msg_id` 从 1 开始自增）。
    fn next_seq(&mut self, msg_id: &str) -> u32 {
        if let Some(seq) = self.used.get_mut(msg_id) {
            let next = seq.saturating_add(1);
            *seq = next;
            self.touch(msg_id);
            return next;
        }
        self.used.insert(msg_id.to_owned(), 1);
        self.order.push_back(msg_id.to_owned());
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.used.remove(&evicted);
            }
        }
        1
    }

    /// 把一个 key 移到队尾（LRU 语义）。
    fn touch(&mut self, msg_id: &str) {
        if let Some(index) = self.order.iter().position(|key| key == msg_id) {
            self.order.remove(index);
        }
        self.order.push_back(msg_id.to_owned());
    }

    /// 当前表大小（测试用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.used.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_seq_starts_at_one_and_increments_per_message() {
        let mut table = MsgSeqTable::new(8);
        assert_eq!(table.next_seq("MSG-A"), 1);
        assert_eq!(table.next_seq("MSG-A"), 2);
        assert_eq!(table.next_seq("MSG-A"), 3);
        // 不同 msg_id 各自从 1 开始。
        assert_eq!(table.next_seq("MSG-B"), 1);
    }

    #[test]
    fn msg_seq_table_stays_bounded_and_evicts_the_oldest() {
        let mut table = MsgSeqTable::new(2);
        assert_eq!(table.next_seq("MSG-1"), 1);
        assert_eq!(table.next_seq("MSG-2"), 1);
        // 表满（2 条），再插一条会挤掉最旧的 MSG-1。
        assert_eq!(table.next_seq("MSG-3"), 1);
        assert_eq!(table.len(), 2);
        // MSG-1 被挤掉后重新从 1 开始（这是有意的：超过容量的事件早已无法被动回复）。
        assert_eq!(table.next_seq("MSG-1"), 1);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn msg_seq_table_touch_keeps_recent_entries_alive() {
        let mut table = MsgSeqTable::new(2);
        table.next_seq("MSG-1");
        table.next_seq("MSG-2");
        // 碰一下 MSG-1，让它变成「最近使用」，于是被挤掉的是 MSG-2。
        assert_eq!(table.next_seq("MSG-1"), 2);
        table.next_seq("MSG-3");
        // MSG-1 被 touch 过，所以它是最新的、活了下来；被挤掉的是 MSG-2。
        assert_eq!(table.next_seq("MSG-1"), 3, "MSG-1 should have survived");
        assert_eq!(table.next_seq("MSG-2"), 1, "MSG-2 should have been evicted");
    }

    #[test]
    fn zero_capacity_is_clamped_instead_of_always_returning_one() {
        let mut table = MsgSeqTable::new(0);
        assert_eq!(table.next_seq("MSG-A"), 1);
        assert_eq!(table.next_seq("MSG-A"), 2);
    }
}
