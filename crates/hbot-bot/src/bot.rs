//! Bot 句柄：`Supervisor` 的克隆安全外壳。
//!
//! 组合根（`hbot-cli` / `hbot-desktop` / RPC）拿到的是 [`BotHandle`]：可以克隆、可以跨
//! 任务共享，`start` / `stop` / `inject` / `send` 都委托给同一个 `Supervisor`。
//! 句柄本身不持有生命周期资源，drop 句柄不会停 bot——停止必须显式 `stop()`（关停顺序
//! 见 [`crate::supervisor`]），避免 clone 的句柄各自以为自己该负责停机。

use std::sync::Arc;

use hbot_core::{BotId, BotStatus, HbotError, OutboundId};

use crate::event::PlatformEvent;
use crate::outbox::OutboundRequest;
use crate::supervisor::{BotSnapshot, InjectOutcome, Supervisor};

/// 一个 bot 的句柄（可克隆）。
#[derive(Clone)]
pub struct BotHandle {
    supervisor: Arc<Supervisor>,
}

impl BotHandle {
    /// 由 supervisor 构造（通常走 [`Supervisor::handle`]）。
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }

    /// 这个句柄对应的 bot ID。
    pub fn bot_id(&self) -> BotId {
        self.supervisor.bot_id()
    }

    /// 当前状态。
    pub fn status(&self) -> BotStatus {
        self.supervisor.status()
    }

    /// 当前快照（状态 + 启动步骤 + 队列深度 + outbox 计数）。
    pub fn snapshot(&self) -> BotSnapshot {
        self.supervisor.snapshot()
    }

    /// 所有会话未完成消息数（UI 显示积压）。
    pub fn queue_depth(&self) -> usize {
        self.supervisor.queue_depth()
    }

    /// 启动 bot。
    pub async fn start(&self) -> Result<(), HbotError> {
        self.supervisor.start().await
    }

    /// 停止 bot：停止读取 → 排空 → 取消 worker → flush outbox。
    pub async fn stop(&self) -> Result<(), HbotError> {
        self.supervisor.stop().await
    }

    /// 注入一条归一化事件（RPC / 测试用），与适配器走同一条链路。
    pub async fn inject(&self, event: PlatformEvent) -> Result<InjectOutcome, HbotError> {
        self.supervisor.inject(event).await
    }

    /// 入队一条出站消息（先落库，再串行发送）。
    pub async fn send(&self, request: OutboundRequest) -> Result<OutboundId, HbotError> {
        self.supervisor.send(request).await
    }

    /// 取回底层 supervisor（需要直接调高级 API 时用）。
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }
}

impl Supervisor {
    /// 生成可克隆句柄（组合根持有句柄，supervisor 本身由 [`Arc`] 共享）。
    pub fn handle(self: &Arc<Self>) -> BotHandle {
        BotHandle::new(Arc::clone(self))
    }
}
