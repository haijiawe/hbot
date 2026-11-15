//! 端口 trait：领域层 / 应用层依赖这些抽象，具体实现由 `hbot-adapters`
//! 与组合根（`hbot-cli` / `hbot-desktop`）注入。
//!
//! 约定：
//!
//! - 全部同步；异步边界在实现方（见 `docs/02-architecture.md` §10）。
//! - 都要求 `Send + Sync`，实现通常是 `Arc<dyn ...>` 共享。
//! - 实现应当是 `'static` 的，否则不能放进 `Arc<dyn Trait>` 长期持有。
//! - 时间与 UUID 必须经这里注入，测试才能固定值（`AGENTS.md` §5）。

use crate::domain::Timestamp;
use crate::ids::{
    ApprovalId, BotId, ConversationId, EditId, InboundId, MessageId, OutboundId, ProjectId,
    SearchId, SessionId,
};

/// 时钟端口。领域层不直接读系统时间。
pub trait Clock: Send + Sync {
    /// 当前时间（Unix 毫秒，UTC）。
    fn now(&self) -> Timestamp;
}

/// ID 生成端口。
///
/// 每个实体一个方法，避免"任意 UUID 都能塞进任意字段"；实现通常就是 `Uuid::new_v4()`。
pub trait IdGenerator: Send + Sync {
    /// 新的项目 ID。
    fn project_id(&self) -> ProjectId;

    /// 新的 bot ID。
    fn bot_id(&self) -> BotId;

    /// 新的会话 ID（平台会话映射）。
    fn conversation_id(&self) -> ConversationId;

    /// 新的 Agent 会话 ID。
    fn session_id(&self) -> SessionId;

    /// 新的消息 ID（入站/出站、幂等键）。
    fn message_id(&self) -> MessageId;

    /// 新的入站表行主键（与 `message_id` 同值不同用途，见 [`crate::ids::InboundId`]）。
    fn inbound_id(&self) -> InboundId;

    /// 新的出站表行主键（见 [`crate::ids::OutboundId`]）。
    fn outbound_id(&self) -> OutboundId;

    /// 新的审批请求 ID（见 [`crate::ids::ApprovalId`]）。
    fn approval_id(&self) -> ApprovalId;

    /// 新的编辑批次 ID（见 [`crate::ids::EditId`]）。
    fn edit_id(&self) -> EditId;

    /// 新的搜索会话 ID（见 [`crate::ids::SearchId`]）。
    fn search_id(&self) -> SearchId;
}

/// 事件出口端口（同步投递）。
///
/// 泛型参数是**事件负载类型**：`hbot-core` 不能依赖 `hbot-rpc`，所以这里不出现
/// 具体的 UI `Event`。
///
/// - 面向 UI 的事件：上层实例化为 `EventSink<hbot_rpc::Event>`；
/// - 平台适配器投递入站事件：实例化为 `EventSink<InboundMessage>`（或上层定义的
///   入站事件类型）。
///
/// 实现方负责有界排队与背压：队列满时不得静默丢弃，按
/// `docs/02-architecture.md` §9.2 / §10 向上游施加压力。
pub trait EventSink<E>: Send + Sync {
    /// 投递一个事件。实现不得阻塞等待 UI 消费。
    fn emit(&self, event: E);
}
