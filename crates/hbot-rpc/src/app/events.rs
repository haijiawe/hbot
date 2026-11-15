//! 进程内事件出口。
//!
//! `docs/02-architecture.md` §5.1：**核心内部事件总线不走 loopback socket**。旧项目
//! 为此起了一个本地 WebSocket 服务器、再把事件接回自己的客户端，代价是一条没有鉴权的
//! 本地端口和一圈序列化（`docs/00-assessment.md` P0-2）。这里只是一个
//! `tokio::sync::broadcast` 通道。
//!
//! ## 语义
//!
//! * **没有订阅者时事件被丢弃**：UI 没连上不等于要给它补历史，历史在数据库里。
//! * **慢订阅者**（`Lagged`）自己决定补什么：`ws` 层对 `bulk` 事件记一条 warn
//!   并继续，对关键事件则断开重连（`docs/02-architecture.md` §9.2）。
//! * 广播容量固定：不因为某个订阅者慢就把内存吃光。

use std::sync::Arc;

use hbot_core::EventSink;
use tokio::sync::broadcast;

use crate::dto::Event;

/// 广播容量：一条连接背后最多积压多少个事件。
///
/// 与 `ws` 的每连接队列（critical 64 / bulk 1024）不同层：这里是**进程级**积压，
/// 缓冲的是"订阅者来不及取"的部分，不是"连不上"的部分。
pub const BUS_CAPACITY: usize = 4096;

/// 事件总线：一个广播发送端 + 订阅入口。
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// 建一条总线。
    pub fn new() -> Self {
        Self::with_capacity(BUS_CAPACITY)
    }

    /// 指定容量（测试用小容量验证 `Lagged` 行为）。
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// 订阅后续事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// 当前订阅者数量（诊断用）。
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// 发一条事件，返回实际送达的订阅者数。
    ///
    /// 返回 0 表示"没人听"——这是正常状态（UI 没打开），不是错误。
    pub fn send(&self, event: Event) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink<Event> for EventBus {
    fn emit(&self, event: Event) {
        let _ = self.sender.send(event);
    }
}

/// 共享句柄别名：`AppState` 与各命令都按这个类型持有总线。
pub type SharedBus = Arc<EventBus>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::LogLevel;

    fn sample() -> Event {
        Event::RunLog {
            bot_id: hbot_core::BotId::from_uuid(uuid::Uuid::new_v4()),
            level: LogLevel::Info,
            line: "hello".to_owned(),
            at: hbot_core::Timestamp::from_unix_millis(1),
        }
    }

    #[tokio::test]
    async fn send_reaches_a_subscriber() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        assert_eq!(bus.send(sample()), 1);
        assert!(receiver.recv().await.is_ok());
    }

    #[tokio::test]
    async fn send_without_subscribers_is_not_an_error() {
        let bus = EventBus::new();
        assert_eq!(bus.send(sample()), 0);
    }

    #[tokio::test]
    async fn emit_through_the_port_compiles() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        EventSink::emit(&bus, sample());
        assert!(receiver.recv().await.is_ok());
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_instead_of_blocking_the_sender() {
        let bus = EventBus::with_capacity(2);
        let mut receiver = bus.subscribe();
        for _ in 0..5 {
            bus.send(sample());
        }
        // 广播不阻塞发送方：落后的一方拿到 Lagged。
        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
