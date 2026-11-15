//! OneBot 11 适配器：把 [`hbot_adapters::onebot::OneBotClient`] 接到 supervisor 的
//! [`BotAdapter`](hbot_bot::BotAdapter) 契约上。
//!
//! 职责边界（`docs/03-directory.md` §2）：
//!
//! * `hbot-adapters` 只做协议翻译与传输（JSON → `NormalizedInbound`）；
//! * `hbot-bot` 只做队列、去重、outbox；
//! * **本文件**把两者接起来，并决定"这条消息回哪个会话""发送失败怎么报"。
//!
//! ## 当前状态（M0，**未接线**）
//!
//! 两个方法都返回明确的错误，不伪造成功：
//!
//! * `subscribe`：`OneBotClient::next_event` 需要 `&mut self`，而 `BotAdapter::subscribe`
//!   只给 `&self`。所有权问题需要一次显式设计决定（把客户端拆成
//!   reader/writer 两半，或用 `Arc<Mutex<>>`），见 `docs/spikes/S4-onebot.md`；
//! * `send`：`hbot-adapters` 目前**没有 TLS 后端**，`wss://` 会失败；`ws://` 的
//!   `send_group_msg` 通道待接。
//!
//! 宁可让 `bot_start` 失败并显示原因，也不要在"发送成功"之后什么都没发出去
//! （`AGENTS.md` §0.8）。

use async_trait::async_trait;
use hbot_adapters::platforms::onebot::{OneBotClient, OneBotConfig};
use hbot_bot::{AdapterSubscription, BotAdapter, OutboundEnvelope};
use hbot_core::HbotError;

/// 一个 OneBot 11 bot 的适配器实例。
pub struct OneBotAdapter {
    /// 已建立的客户端（后台线程持有连接与重连状态）。
    client: OneBotClient,
}

impl OneBotAdapter {
    /// 按配置建立连接（不阻塞：客户端在后台重连）。
    pub fn connect(config: OneBotConfig) -> Result<Self, HbotError> {
        let client = OneBotClient::start(config).map_err(HbotError::from)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl BotAdapter for OneBotAdapter {
    fn id(&self) -> &'static str {
        "onebot11"
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        let _ = &self.client;
        Err(HbotError::internal(
            "onebot 订阅尚未接线：OneBotClient::next_event 需要可变所有权，见 docs/spikes/S4-onebot.md",
        ))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        let _ = envelope;
        Err(HbotError::denied(
            "onebot 发送尚未接线：wss:// 无 TLS 后端；ws:// 待接 send_group_msg",
        ))
    }
}
