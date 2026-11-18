//! 端口 trait：领域层 / 应用层依赖这些抽象，具体实现由 `hbot-adapters`
//! 与组合根（`hbot-cli` / `hbot-desktop`）注入。
//!
//! 约定：
//!
//! - 全部同步；异步边界在实现方（见 `docs/02-architecture.md` §10）。
//! - 都要求 `Send + Sync`，实现通常是 `Arc<dyn ...>` 共享。
//! - 实现应当是 `'static` 的，否则不能放进 `Arc<dyn Trait>` 长期持有。
//! - 时间与 UUID 必须经这里注入，测试才能固定值（`AGENTS.md` §5）。

use crate::command::{CommandDenied, CommandOutput, CommandSpec};
use crate::domain::Timestamp;
use crate::ids::{
    ApprovalId, BotId, ConversationId, EditId, InboundId, JobId, MessageId, OutboundId, ProjectId,
    SearchId, SessionId, WaitId,
};

/// 时钟端口。领域层不直接读系统时间。
pub trait Clock: Send + Sync {
    /// 当前时间（Unix 毫秒，UTC）。
    fn now(&self) -> Timestamp;
}

/// 系统时钟：`now()` 读 [`std::time::SystemTime`]。
///
/// 为什么放在 `hbot-core` 而不是适配器层：读系统时间**不是平台适配**——没有协议、
/// 没有网络、没有平台方言，只是 `std::time` 的一层薄包装。放在这里，组合根
/// （`hbot-desktop` 等）注入时钟时不必为此依赖整个 `hbot-adapters`，否则会出现
/// `hbot-desktop -> hbot-adapters` 这种违反依赖方向的边（`cargo run -p xtask -- deps`
/// 会报错）。`hbot-adapters` 仍然 `pub use` 再导出它，历史调用点不用改。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_system_time(std::time::SystemTime::now())
    }
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

    /// 新的后台作业行 ID（见 [`crate::ids::JobId`]）。
    fn job_id(&self) -> JobId;

    /// 新的会话等待行 ID（见 [`crate::ids::WaitId`]）。
    fn wait_id(&self) -> WaitId;
}

/// 受控命令执行。
///
/// 实现必须 **fail-closed**：装不上隔离就返回 [`CommandDenied::IsolationUnavailable`]，
/// 绝不放行。具体合约（`docs/adr/0003-command-isolation.md`）：
///
/// - **只走 argv**：没有 shell 字符串、没有 `sh -c`、没有隐式通配展开；
/// - **工作目录必须留在项目根内**：规范化后按路径组件比较，逃逸即拒绝；
/// - **环境默认清空**：只有 [`CommandSpec::env`] 与 [`CommandSpec::inherit_env`]
///   显式点名的变量会传下去；
/// - **超时 / 输出超限杀整棵进程树**，不是只杀直接子进程；
/// - **程序清单空 = 什么都不许跑**，拿到名字清单当隔离用是旧项目的坑
///   （`AGENTS.md` §4.4）。
///
/// 边界与线程模型：这是**同步、阻塞**端口（和本文件其它端口一致，见 §10 的并发约定）。
/// 一次调用会拉起子进程并等到退出/超时，所以 async 调用方必须放进
/// `spawn_blocking`，不要在 tokio worker 上直接调用。
///
/// 端口放在 `hbot-core`、实现在 `hbot-command`，是为了让"换实现"（作业对象 →
/// 受限令牌 → cgroup/landlock）不动调用方；测试也可以注入一个永远不可用的探测替身。
pub trait CommandRunner: Send + Sync {
    /// 执行一条命令。
    ///
    /// 返回 `Ok` 只表示"跑完了"（退出码见 [`CommandOutput::status`]）；
    /// 隔离不可用、程序不在清单、路径逃逸、要求整份继承环境、输出超限时返回
    /// [`CommandDenied`]，**不会**"尽力跑一下再报告"。
    fn run(&self, spec: CommandSpec) -> Result<CommandOutput, CommandDenied>;
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
