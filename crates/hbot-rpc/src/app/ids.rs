//! [`IdGenerator`] 的默认实现：UUID v4。
//!
//! 放在应用层而不是 `hbot-core`：`hbot-core` 只有端口 trait，不产生随机数
//! （`crates/hbot-core/src/ports.rs`）。组合根把这里的一个实例塞进 `AppState`，
//! 测试换成确定性实现。
//!
//! 为什么每种 ID 都手写一行而不是 `impl<T> From<Uuid>`：类型安全靠的是**每个
//! 方法返回具体 newtype**，泛型收窄一次，`BotId` 塞进 `ProjectId` 就会重新变成可能。

use hbot_core::{
    ApprovalId, BotId, Clock, ConversationId, EditId, IdGenerator, InboundId, JobId, MessageId,
    OutboundId, ProjectId, SearchId, SessionId, Timestamp, WaitId,
};

/// UUID v4 生成器。
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidIds;

impl IdGenerator for UuidIds {
    fn project_id(&self) -> ProjectId {
        ProjectId::from_uuid(uuid::Uuid::new_v4())
    }

    fn bot_id(&self) -> BotId {
        BotId::from_uuid(uuid::Uuid::new_v4())
    }

    fn conversation_id(&self) -> ConversationId {
        ConversationId::from_uuid(uuid::Uuid::new_v4())
    }

    fn session_id(&self) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::new_v4())
    }

    fn message_id(&self) -> MessageId {
        MessageId::from_uuid(uuid::Uuid::new_v4())
    }

    fn inbound_id(&self) -> InboundId {
        InboundId::from_uuid(uuid::Uuid::new_v4())
    }

    fn outbound_id(&self) -> OutboundId {
        OutboundId::from_uuid(uuid::Uuid::new_v4())
    }

    fn approval_id(&self) -> ApprovalId {
        ApprovalId::from_uuid(uuid::Uuid::new_v4())
    }

    fn edit_id(&self) -> EditId {
        EditId::from_uuid(uuid::Uuid::new_v4())
    }

    fn search_id(&self) -> SearchId {
        SearchId::from_uuid(uuid::Uuid::new_v4())
    }

    fn job_id(&self) -> JobId {
        JobId::from_uuid(uuid::Uuid::new_v4())
    }

    fn wait_id(&self) -> WaitId {
        WaitId::from_uuid(uuid::Uuid::new_v4())
    }
}

/// 可固定的时钟：从给定时刻开始，每次 `advance` 前进。
///
/// 生产用 `hbot_adapters::SystemClock`；这个是给命令测试用的——否则
/// "超时"和"顺序"类断言只能靠 sleep。
#[derive(Debug)]
pub struct ManualClock {
    now: std::sync::Mutex<Timestamp>,
}

impl ManualClock {
    /// 从 `now` 开始。
    pub fn new(now: Timestamp) -> Self {
        Self {
            now: std::sync::Mutex::new(now),
        }
    }

    /// 前进 `ms` 毫秒。
    pub fn advance(&self, ms: i64) {
        let mut guard = self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Timestamp::from_unix_millis(guard.unix_millis() + ms);
    }

    /// 拨到指定时刻。
    pub fn set(&self, now: Timestamp) {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_unique_per_kind() {
        let ids = UuidIds;
        assert_ne!(ids.bot_id(), ids.bot_id());
        assert_ne!(ids.project_id(), ids.project_id());
    }

    #[test]
    fn manual_clock_advances() {
        let clock = ManualClock::new(Timestamp::from_unix_millis(1_000));
        assert_eq!(clock.now(), Timestamp::from_unix_millis(1_000));
        clock.advance(250);
        assert_eq!(clock.now(), Timestamp::from_unix_millis(1_250));
        clock.set(Timestamp::from_unix_millis(9_000));
        assert_eq!(clock.now(), Timestamp::from_unix_millis(9_000));
    }
}
