//! 行模型、行级 DTO 与写入参数。
//!
//! 字段与 `migrations/*.sql` 一一对应，命名用 snake_case；ID 用 `hbot_core::ids` 的强类型
//! （落库为 UUID 字符串），时间统一是 **Unix 毫秒（i64）**，由调用方注入（`clock.now_ms()`）
//! ——存储层不取系统时间，也不生成 ID。
//!
//! ## 行级 DTO 与领域实体的分层
//!
//! [`OutboundRow`] / [`InboundRow`] / [`LogRow`] 是**行级 DTO**：字段就是列形状
//! （`String` / `i64` / `u32`），不认识领域规则。落到领域实体（`hbot_core::domain`）的
//! `row_to_domain` 映射由使用方做（`docs/03-directory.md` §1.10）：store 不认识 `Value`
//! 语义，core 不认识 SQL。

use crate::ids::{BotId, ConversationId, InboundId, OutboundId, ProjectId};

/// `projects` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
    pub created_at: i64,
}

/// `bots` 行。
///
/// 不含 `status` / `updated_at`：这两列是 0002 新增的运行状态，读法见
/// [`crate::Store::get_bot_status`]。加字段会破坏已有调用方的结构体字面量，所以状态单独出
/// [`BotStatusRow`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bot {
    pub id: BotId,
    pub project_id: ProjectId,
    pub name: String,
    pub platform: String,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: i64,
}

/// `bots` 的运行状态视图（`status` + `updated_at`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotStatusRow {
    pub id: BotId,
    /// 取值见 [`BOT_STATUSES`]。
    pub status: String,
    /// 状态最后变更时间（Unix 毫秒）；0 = 从未变更。
    pub updated_at: i64,
}

/// `conversations` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub bot_id: BotId,
    pub external_id: String,
    pub kind: String,
    pub created_at: i64,
}

/// `inbound` 的插入参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInbound {
    pub id: InboundId,
    pub bot_id: BotId,
    pub external_id: String,
    pub conversation_id: ConversationId,
    pub payload_json: String,
    pub received_at: i64,
}

impl NewInbound {
    /// 幂等键 `(bot_id, external_id)` 的便捷视图，便于日志与测试断言。
    pub fn dedupe_key(&self) -> (&BotId, &str) {
        (&self.bot_id, self.external_id.as_str())
    }
}

/// `outbound` 的插入参数。初始态固定为 [`OUTBOUND_STATE_PENDING`]，attempts 从 0 开始。
///
/// 发送目标与回复引用不放这里（会破坏已有调用方的结构体字面量），由
/// [`crate::Store::enqueue_outbound_with_target`] 与 [`OutboundTarget`] 单独传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutbound {
    pub id: OutboundId,
    pub bot_id: BotId,
    pub conversation_id: ConversationId,
    pub text: String,
    pub created_at: i64,
}

/// 出站路由信息：发到哪里、引用哪条消息。
///
/// * `target_json`：平台相关的发送目标（群号/用户 id/话题等），由适配器解释；`"{}"` = 未知。
/// * `reply_to`：被动回复引用的平台消息 ID；`None` = 主动消息。
///
/// 与 [`NewOutbound`] 分开，是为了不改变已有调用方的结构体字面量（`hbot-bot` 已经按
/// 5 字段构造）。不需要路由信息时用 [`OutboundTarget::unknown`] / `Default`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundTarget {
    pub target_json: String,
    pub reply_to: Option<String>,
}

impl OutboundTarget {
    /// 明确给出目标与回复引用。
    pub fn new(target_json: impl Into<String>, reply_to: Option<String>) -> Self {
        Self {
            target_json: target_json.into(),
            reply_to,
        }
    }

    /// 未知目标、非被动回复：落库为 `'{}'` + `NULL`。
    pub fn unknown() -> Self {
        Self {
            target_json: "{}".to_owned(),
            reply_to: None,
        }
    }
}

impl Default for OutboundTarget {
    fn default() -> Self {
        Self::unknown()
    }
}

/// `outbound` 行（行级 DTO，映射到领域实体由使用方做）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRow {
    pub id: OutboundId,
    pub bot_id: BotId,
    pub conversation_id: ConversationId,
    /// 平台相关发送目标 JSON；`"{}"` = 未知。
    pub target_json: String,
    pub text: String,
    /// 被动回复引用的平台消息 ID；`None` = 主动消息。
    pub reply_to: Option<String>,
    /// 已尝试次数（含首次）。
    pub attempts: u32,
    pub created_at: i64,
}

/// `inbound` 行（行级 DTO，映射到领域实体由使用方做）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundRow {
    pub id: InboundId,
    pub bot_id: BotId,
    pub external_id: String,
    pub conversation_id: ConversationId,
    /// 发送者平台 ID；空串 = 平台未提供。
    pub sender_id: String,
    pub payload_json: String,
    pub received_at: i64,
}

/// 启动补偿用的一行未处理入站消息：`inbound` 行 + 它的会话目标 + 游标。
///
/// 为什么把会话字段一起带出来：补偿要在启动时把每个 `MessageCtx` 的
/// `ConversationRef` 还原完整（外部 ID + 类型），逐行再查一次 `conversations` 就是
/// N+1 次 db 往返；一条 JOIN 就够。
///
/// 为什么要 [`rowid`](Self::rowid)：分批取用 `(received_at, rowid)` 做 keyset 游标。
/// 只按 `received_at` 分页时，同一毫秒的多条消息会缺一个可比较的位次，容易漏行或
/// 反复取到同一批；`inbound.id` 是 TEXT 主键，但表不是 `WITHOUT ROWID`，隐式 `rowid`
/// 就是插入顺序，单调且唯一。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundReplayRow {
    pub message: InboundRow,
    /// SQLite 隐式 `rowid`（插入顺序）。
    pub rowid: i64,
    /// 会话的外部 ID（平台侧群号 / 用户号）。
    pub conversation_external_id: String,
    /// 会话类型（`private` / `group` / `channel` / `unknown`）。
    pub conversation_kind: String,
}

/// `logs` 行（行级 DTO）。`id` 是 SQLite AUTOINCREMENT，天然编码写入先后。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub id: i64,
    /// `None` = Agent 日志（不属于任何 bot）。
    pub bot_id: Option<BotId>,
    pub level: String,
    pub line: String,
    pub at: i64,
    /// 日志来源：`bot` / `agent`（0003 起）。
    pub source: String,
}

/// `logs` 的插入参数（Bot 日志，`source` 固定写 `'bot'`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLog {
    pub bot_id: BotId,
    pub level: String,
    pub line: String,
    pub at: i64,
}

/// `logs` 的 Agent 日志插入参数：没有 bot 归属，落库时 `bot_id = NULL`、`source = 'agent'`。
///
/// 单独一个类型而不是给 [`NewLog`] 加 `source`/`Option<BotId>`：`hbot-bot` 与
/// `hbot-rpc` 的 sandbox handler 都直接 struct literal 构造 `NewLog`，改字段形状会
/// 破坏那些不在本改动边界内的 crate。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentLog {
    pub level: String,
    pub line: String,
    pub at: i64,
}

/// 出站消息状态：刚入队，等待发送。
pub const OUTBOUND_STATE_PENDING: &str = "pending";
/// 出站消息状态：已由平台确认发送。
pub const OUTBOUND_STATE_SENT: &str = "sent";
/// 出站消息状态：超过重试上限，转为死信（保留行，不再被 `pending` 扫描捡起）。
pub const OUTBOUND_STATE_FAILED: &str = "failed";

/// `bots.status`：已停止（新装/手动停）。0002 的列默认值。
pub const BOT_STATUS_STOPPED: &str = "stopped";
/// `bots.status`：运行中。
pub const BOT_STATUS_RUNNING: &str = "running";
/// `bots.status`：降级运行（部分能力不可用，仍在重试/重连）。
pub const BOT_STATUS_DEGRADED: &str = "degraded";
/// `bots.status`：出错（需要人工介入）。
pub const BOT_STATUS_ERROR: &str = "error";

/// 全部合法的 `bots.status` 取值；校验与错误信息都用这一份清单。
pub const BOT_STATUSES: &[&str] = &[
    BOT_STATUS_STOPPED,
    BOT_STATUS_RUNNING,
    BOT_STATUS_DEGRADED,
    BOT_STATUS_ERROR,
];

/// 校验 `bots.status` 取值。
///
/// 非法值必须由调用方上报（`StoreError::Invalid`），不允许存储层挑一个近似值落库。
pub fn is_valid_bot_status(status: &str) -> bool {
    BOT_STATUSES.contains(&status)
}
