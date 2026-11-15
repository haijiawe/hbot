//! 领域实体与值类型。
//!
//! 这里**只有数据与枚举**：没有 IO、没有业务规则、没有校验函数；字段与
//! `docs/02-architecture.md` §7 的表结构一一对应（`*_json` 列在这里是结构化字段）。
//!
//! 序列化约定（与 UI 契约一致）：
//!
//! - 结构体字段用 camelCase（这些类型会出现在 `Event` 载荷里，如 `agent.tool.call`）；
//! - 枚举值用 snake_case；
//! - 时间为 [`Timestamp`]（Unix 毫秒整数），不引入 `chrono` / `time` 依赖。

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ids::{BotId, ConversationId, MessageId, ProjectId, SessionId};

/// Unix 毫秒时间戳（UTC，可为负）。
///
/// workspace 没有引入日期库，领域层也不需要时区；DB 与线上都是整数毫秒。
/// 落库为 SQLite `INTEGER`（毫秒）；线上 JSON 也是整数，不是 ISO 字符串。
/// 展示时由调用方格式化（UI 用 `Intl.DateTimeFormat`）。
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    ts_rs::TS,
)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 由 Unix 毫秒构造。
    pub const fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// 返回 Unix 毫秒。
    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// 由系统时间构造（时钟实现用）。早于 Unix 纪元时为负数。
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(i64::try_from(after.as_millis()).unwrap_or(i64::MAX)),
            Err(before) => {
                let millis = i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX);
                Self(-millis)
            }
        }
    }
}

/// 项目：一个可打开的目录（`hbot.json` + 源码），对应 `projects` 表。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// 项目 ID（`projects.id`）。
    pub id: ProjectId,
    /// 展示名（默认取目录名，用户可改）。
    pub name: String,
    /// 项目根目录的绝对路径；原样保存，不做规范化（realpath 在 workspace 层做）。
    pub path: String,
    /// 创建时间。
    pub created_at: Timestamp,
}

/// Bot：对应 `bots` 表，一个项目下可以有多个 bot。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bot {
    /// Bot ID（`bots.id`）。
    pub id: BotId,
    /// 所属项目。
    pub project_id: ProjectId,
    /// 展示名。
    pub name: String,
    /// 平台标识，如 `onebot11`、`webhook`（对应 `hbot-adapters` 的注册名）。
    pub platform: String,
    /// 平台相关配置（`bots.config_json`），不透明 JSON，由适配器解释。
    pub config: serde_json::Value,
    /// 是否允许被启动（用户开关）。
    pub enabled: bool,
    /// 运行时状态，由 supervisor 维护。
    pub status: BotStatus,
    /// 创建时间。
    pub created_at: Timestamp,
}

/// Bot 运行时状态（`bot.status` 事件与 UI 的状态灯）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum BotStatus {
    /// 已停止（未启动或被用户停止）。
    #[default]
    Stopped,
    /// 正在启动（编译 / 建连接）。
    Starting,
    /// 运行中（适配器已连接）。
    Running,
    /// 正在停止（排空队列 / 断开连接）。
    Stopping,
    /// 降级：可运行但平台断线、编译失败或连续崩溃后停止自动重启。
    Degraded,
}

/// 会话：平台上的私聊 / 群 / 频道的本地映射，对应 `conversations` 表。
///
/// `(bot_id, external_id)` 唯一；同一会话的消息必须串行处理（`AGENTS.md` §4.1）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    /// 会话 ID（`conversations.id`）。
    pub id: ConversationId,
    /// 所属 bot。
    pub bot_id: BotId,
    /// 平台侧会话标识（群号、用户 id 等），用于去重与发送定位。
    pub external_id: String,
    /// 会话类型。
    pub kind: ConversationKind,
    /// 展示名；平台没给时为 `None`，UI 用 `external_id` 兜底。
    pub title: Option<String>,
    /// 最近一条消息时间；从未有消息时为 `None`。
    pub last_message_at: Option<Timestamp>,
}

/// 会话类型。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    /// 私聊。
    Private,
    /// 群聊。
    Group,
    /// 频道 / 公告。
    Channel,
    /// 平台给了会话但类型未知。
    #[default]
    Unknown,
}

/// 入站消息：已落库、待处理（`inbound` 表）。
///
/// 不变量：`(bot_id, external_id)` 唯一 → 重复投递只处理一次；
/// 处理完成才写 `processed_at`（`docs/02-architecture.md` §4.1）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboundMessage {
    /// 消息 ID（`inbound.id`），也是生成代码拿到的幂等键。
    pub id: MessageId,
    /// 所属 bot。
    pub bot_id: BotId,
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 平台事件 ID（`inbound.external_id`），去重键的一半。
    pub external_id: String,
    /// 发送者平台 ID。
    pub sender_id: String,
    /// 归一化后的事件载荷（不透明 JSON，交给生成代码解释）。
    pub payload: serde_json::Value,
    /// 收到时间。
    pub received_at: Timestamp,
    /// 处理完成时间；`None` 表示未处理（崩溃后应重放）。
    pub processed_at: Option<Timestamp>,
}

/// 出站消息：待发送 / 已发送的消息记录（`outbound` 表）。
///
/// 与入站对称：先落库，再由发送器按会话串行发送、失败退避重试，
/// 超过上限进死信（`docs/02-architecture.md` §12）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundMessage {
    /// 消息 ID（`outbound.id`）。
    pub id: MessageId,
    /// 所属 bot。
    pub bot_id: BotId,
    /// 目标会话。
    pub conversation_id: ConversationId,
    /// 待发送载荷（不透明 JSON，由适配器翻译成平台协议）。
    pub payload: serde_json::Value,
    /// 当前发送状态。
    pub status: OutboundState,
    /// 已尝试次数（含首次）。
    pub attempts: u32,
    /// 下一次重试时间；`None` 表示不需要重试（已发送或已进死信）。
    pub next_attempt_at: Option<Timestamp>,
    /// 创建时间。
    pub created_at: Timestamp,
}

/// 出站消息状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum OutboundState {
    /// 待发送（已落库，尚未交给适配器）。
    #[default]
    Pending,
    /// 发送中（已交给适配器，结果未知；崩溃后按"可能已发送"重放）。
    Sending,
    /// 已发送成功。
    Sent,
    /// 单次发送失败，等待退避重试。
    Failed,
    /// 死信：重试超限或不可重试错误，需要人工处理。
    Dead,
}

/// 沙箱档位（`docs/02-architecture.md` §11.2）。
///
/// 默认 [`SandboxMode::WorkspaceWrite`]；档位之间没有"继承"关系，
/// 命令隔离建不出来时直接拒绝，不回退名字白名单。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// 只读：项目内可读，写入 / 命令 / 网络一律拒绝。
    ReadOnly,
    /// 项目内写入 + 受限命令隔离（**默认**）。
    #[default]
    WorkspaceWrite,
    /// 危险档：命令需显式确认；仅用户主动切换。
    Danger,
}

impl SandboxMode {
    /// 稳定字符串（用于 DB、审计与审批规则键 `(tool, 参数, sandbox_mode)`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::Danger => "danger",
        }
    }
}

/// 用户对一次审批的决定。
///
/// `always` 不在这里：是否"以后都放行"由命令参数单独携带，且规则键必须包含
/// 完整动作（`AGENTS.md` §0.7），记录在 [`ToolCall`] 的 `approval` 字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 批准本次动作。
    Approve,
    /// 拒绝本次动作。
    Deny,
}

/// 一次工具调用（`tool_calls` 表；也作为 `agent.tool` 事件的载荷）。
///
/// 每步落库，崩溃后显示"中断"，不自动重放有副作用的工具
/// （`docs/02-architecture.md` §5.1）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// 工具调用 ID（provider 的 call id 或本地生成的稳定 ID）。
    pub id: String,
    /// 所属 Agent 会话。
    pub session_id: SessionId,
    /// 工具名（与 `hbot-agent/src/tools/` 注册表一致）。
    pub name: String,
    /// 调用参数（已按工具 schema 校验过的 JSON）。
    #[ts(type = "unknown")]
    pub args: serde_json::Value,
    /// 执行结果；未完成 / 无返回时为 `None`。
    #[ts(type = "unknown | null")]
    pub result: Option<serde_json::Value>,
    /// 执行状态。
    pub status: ToolCallStatus,
    /// 审批结果；不需要审批的工具为 `None`。
    pub approval: Option<ApprovalDecision>,
    /// 开始时间。
    pub created_at: Timestamp,
    /// 结束时间；进行中为 `None`。
    pub finished_at: Option<Timestamp>,
}

/// 工具调用状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// 已登记，尚未执行（等 LLM 批次调度）。
    #[default]
    Pending,
    /// 等待用户审批（`approval.request` 已发出）。
    AwaitingApproval,
    /// 执行中。
    Running,
    /// 执行成功。
    Succeeded,
    /// 执行失败（含超时、非零退出码）。
    Failed,
    /// 被取消（用户取消 turn 或超时中断）。
    Cancelled,
}
