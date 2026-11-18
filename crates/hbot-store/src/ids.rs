//! 强类型 ID：**唯一真源在 `hbot-core`**（`AGENTS.md` §0.4「只有一份契约」）。
//!
//! 存储层不自己定义 ID 类型。`crates/hbot-core/src/ids.rs` 里每个 ID 内部是
//! [`uuid::Uuid`]，落库时存成 TEXT（带连字符的 UUID 字符串，SQLite 列类型 TEXT）。
//! 本模块只是一层重导出，好处是：
//!
//! * 内部代码继续写 `crate::ids::BotId`，与 `hbot-core` 是**同一个类型**，跨 crate
//!   不需要任何转换胶水；
//! * 调用方可以 `use hbot_store::BotId`，不必同时依赖 `hbot-core`。
//!
//! 注意两件事：
//!
//! * ID 的生成本层不做——由边界经 `hbot_core::ports::IdGenerator` 注入（测试可固定）；
//! * `hbot_core::ids::MessageId` **不在**这里重导出：它是"生成代码看到的幂等键"
//!   （`ctx.messageId`），与 [`InboundId`] 同值但概念不同，存储层的行主键只用
//!   [`InboundId`] / [`OutboundId`]。

pub use hbot_core::ids::{BotId, ConversationId, InboundId, JobId, OutboundId, ProjectId};
