//! 领域 ID 新类型。
//!
//! 十个 ID 内部都是 [`uuid::Uuid`]：
//!
//! - JSON 中是带连字符的字符串（`"7d4f1c9e-..."`），与 `docs/02-architecture.md` §9.1
//!   里 `session_id: String` 的线上形状一致；
//! - [`std::fmt::Display`] 输出该字符串，[`std::str::FromStr`] 负责解析（非法 UUID 报错）；
//! - 类型之间不能互换，避免把 `bot_id` 传给 `session_id` 参数。
//!
//! 消息相关的三个 ID 容易混，区别只在**用途**，值可以相同：
//!
//! - [`MessageId`]：暴露给生成代码的幂等键（`ctx.messageId`），等于入站行的 id；
//! - [`InboundId`]：`inbound` 表行主键，store / repository 层用；
//! - [`OutboundId`]：`outbound` 表行主键，重试与死信记录以它为键。
//!
//! 入站一行既可以用 [`InboundId`] 也可以用 [`MessageId`] 指代（同一个 UUID），
//! 分开是为了让"幂等键"与"行主键"在类型上各自可读，且出站 ID 不会被误当入站 ID。
//!
//! ID 的生成只经 [`crate::ports::IdGenerator`]；不要在业务代码里直接 `Uuid::new_v4()`，
//! 否则测试无法固定 ID。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 定义一个新的 ID 类型：`Uuid` 内部表示 + serde 透明 + `Display` / `FromStr`。
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
            ts_rs::TS,
        )]
        #[serde(transparent)]
        // TS 侧是 `string` 别名（`uuid-impl` 特性）：UI 不该看到 `{ "0": ... }`。
        #[ts(type = "string")]
        pub struct $name(Uuid);

        impl $name {
            /// 由已有 UUID 构造（反序列化、测试、迁移场景）。
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// 取出内部 UUID。
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

define_id!(
    /// 项目 ID；对应 `projects.id`。
    ProjectId
);

define_id!(
    /// Bot ID；对应 `bots.id`。一个项目可以有多个 bot。
    BotId
);

define_id!(
    /// 会话 ID；对应 `conversations.id`（平台会话在本地的映射）。
    ConversationId
);

define_id!(
    /// Agent 会话 ID；对应 `agent_sessions.id`（一次人机对话，不是平台会话）。
    SessionId
);

define_id!(
    /// 消息 ID；对应 `inbound.id` / `outbound.id`，也是生成代码拿到的幂等键 `ctx.messageId`。
    ///
    /// 与 [`InboundId`] / [`OutboundId`] 同值不同用途：本类型面向"消息"语义
    /// （幂等键、回复绑定 `reply_to`），后两者面向表行主键。
    MessageId
);

define_id!(
    /// 入站消息表行主键（`inbound.id`）。
    ///
    /// 与 [`MessageId`] 指向同一行、值相同（`MessageId` 是生成代码看到的幂等键），
    /// 分开是为了让 store / repository 层明确"我在读写一张表的行"。
    InboundId
);

define_id!(
    /// 出站消息表行主键（`outbound.id`）。
    ///
    /// 发送重试、退避调度与死信记录都以它为键；与 [`MessageId`] 同值不同用途，
    /// 与 [`InboundId`] **不可互换**（避免把待发送记录当已收到的消息）。
    OutboundId
);

define_id!(
    /// 审批请求 ID；`approval.request` 事件发出、`AgentApprove` 命令带回。
    ///
    /// 一次审批只对一个**完整动作**（工具 + 规范化参数 + 沙箱档）有效，
    /// 过期或已答复的 ID 必须拒绝（见 `docs/02-architecture.md` §4.2）。
    ApprovalId
);

define_id!(
    /// 编辑批次 ID；`agent.diff` 事件发出、`EditApply` / `EditReject` 带回。
    ///
    /// 一个 ID 对应一次待审的 Diff（可含多个文件、每个文件多个块），
    /// 逐块接受时块序号见 `DiffHunk.index`。
    EditId
);

define_id!(
    /// 搜索会话 ID；`file.search` 事件首次发出、`SearchCancel` 带回。
    ///
    /// 一次 `SearchStart` 对应一个 ID，所有增量结果共用它；取消后不得再发结果。
    SearchId
);

define_id!(
    /// 后台作业行 ID；`bot_jobs.id` 的主键。
    ///
    /// 一个 ID 对应一次的**入队**，而不是一次执行：重试会让同一行多跑几遍
    /// （见 `crate::bot_jobs` 的"至少一次"一节）。
    JobId
);

define_id!(
    /// 等待行 ID；`bot_waits.id` 的主键。
    ///
    /// 一个 ID 对应一次 `ctx.session.prompt()` 注册的等待，而不是一条消息：
    /// 被唤醒、超时、取消都只是这一行的状态变化（见 `crate::bot_session`）。
    WaitId
);
