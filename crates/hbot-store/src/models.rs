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

use crate::ids::{BotId, ConversationId, InboundId, JobId, OutboundId, ProjectId, WaitId};

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

// ---------------------------------------------------------------------------
// 设备能力（C6）：会话与审计（`docs/09-device-capability.md` §3）
// ---------------------------------------------------------------------------

/// `device_sessions` 行（行级 DTO）。
///
/// `state` / `sub_json` / `allowed_apps_json` 都是**列里的原样字符串**：存储层不认识
/// `hbot_core::DeviceSessionState`，也不解析 JSON —— 映射由上层做，与 [`BotStatusRow`]
/// 把 `status` 当字符串存的口径一致（`AGENTS.md` §0.4：契约只有一份，不在存储层再抄一遍）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionRow {
    /// UUIDv4 字符串（TEXT 主键；调用方生成）。
    pub id: String,
    pub bot_id: BotId,
    /// 取值见 `hbot_core::DeviceSessionState::as_str()`：
    /// `requested` / `active` / `expired` / `revoked` / `denied`。
    pub state: String,
    /// 本次批准的子能力 JSON 数组；`[]` = 一个都没有。
    pub sub_json: String,
    /// 允许的应用 JSON 数组（进程映像名）；`[]` = 全拒。
    pub allowed_apps_json: String,
    /// 授权时刻（Unix 毫秒）；`None` = 还没批（`requested` 态）。
    pub granted_at: Option<i64>,
    pub deadline: Option<i64>,
    /// 结束时刻；`None` = 还没结束。
    pub ended_at: Option<i64>,
    pub created_at: i64,
}

/// `device_sessions` 的插入参数。新会话一定还没结束，所以没有 `ended_at` 字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeviceSession {
    pub id: String,
    pub bot_id: BotId,
    pub state: String,
    pub sub_json: String,
    pub allowed_apps_json: String,
    pub granted_at: Option<i64>,
    pub deadline: Option<i64>,
    /// 会话创建时刻（Unix 毫秒）—— 也是 `latest_device_session` 的第一排序键，必须由
    /// 调用方给，存储层不读时钟。
    pub created_at: i64,
}

/// `device_audit` 行（行级 DTO）。`id` 是 AUTOINCREMENT，同一毫秒内的定序靠它。
///
/// **没有内容字段**：`target` 只放进程名 / 窗口 id / 子能力名，像素、剪贴板文本、
/// OCR 原文一律不进库（`docs/09` §3.2、§4.3）。表结构层面就不给这些内容留列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuditRow {
    pub id: i64,
    pub at: i64,
    pub bot_id: BotId,
    /// 谁在请求：`bot_code` / `agent_tool`。
    pub actor: String,
    /// 动作名。
    pub action: String,
    /// 目标标识符（进程名 / 窗口 id / 子能力名），`None` = 无目标。
    pub target: Option<String>,
    /// 结果：`ok` / `denied` / `failed`。
    pub outcome: String,
}

/// `device_audit` 的插入参数（`id` 由 AUTOINCREMENT 生成）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeviceAudit {
    pub at: i64,
    pub bot_id: BotId,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub outcome: String,
}

// ---------------------------------------------------------------------------
// Bot 侧状态（C2）：KV 与历史（`hbot_core::bot_state` 的存储侧行形状）
// ---------------------------------------------------------------------------

/// 一行 KV。
///
/// `value_json` 是**原样字符串**：存储层不解析 JSON（与 [`DeviceSessionRow::sub_json`]
/// 同口径），解析与类型判断在 `hbot-rpc` 那一层做 —— 那边才有 `serde_json::Value`。
///
/// `expires_at` 是 Unix 毫秒到期时刻，`None` = 永不过期。过期判定还需要一个 `now`，
/// 所以本结构只描述列，不自己判断"现在过没过期"（见 [`crate::Store::get_kv`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvRow {
    pub key: String,
    pub value_json: String,
    pub expires_at: Option<i64>,
}

/// 待写入的一条 KV（[`crate::Store::set_kv_many`] 的批量元素）。
///
/// 与 [`KvRow`] 同形状但**语义不同**：`expires_at` 是"这次写入后到期时刻"，`None` 表示
/// 把到期时间清成永不过期（覆盖旧值时必须能把非 NULL 改回 NULL）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewKvEntry {
    pub key: String,
    pub value_json: String,
    pub expires_at: Option<i64>,
}

/// 历史里的一条消息（`inbound` 或 `outbound`）。
///
/// 两个方向被**合成一张表**返回（[`crate::Store::list_history`]）：生成代码的
/// `ctx.history(n)` 只关心"谁、什么时候、说了什么"，而它们分居两表。
///
/// `direction` 是 `"inbound"` / `"outbound"`（与
/// `hbot_core::HistoryDirection::as_str()` 同值）；`message_id` 是各自表的主键，
/// `at` 是各自的业务时间（inbound `received_at` / outbound `created_at`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// `"inbound"` | `"outbound"`。
    pub direction: String,
    /// `inbound.id` / `outbound.id`（UUID 字符串）。
    pub message_id: String,
    /// `inbound.external_id` / `outbound.reply_to.unwrap_or_default()`。
    pub external_id: String,
    /// `inbound.sender_id` / `""`（出站行没有发送者）。
    pub sender_id: String,
    /// `inbound`：`payload_json` 里 `"text"` 的字符串值（缺失/非字符串/整段非法 JSON → `""`）；
    /// `outbound`：`outbound.text`。
    pub text: String,
    /// `inbound.received_at` / `outbound.created_at`（Unix 毫秒）。
    pub at: i64,
}

/// 后台作业行（[`crate::Store::get_job`] / [`crate::Store::list_jobs_for_bot`] 的返回形状）。
///
/// 与表 `bot_jobs` 同列，但**不**解析载荷与状态：`payload_json` 是原样字符串，
/// `state` 也是原样字符串（拼写真源在 `hbot_core::bot_jobs::JobState::as_str()`，
/// 存储层不重复定义一份）。解析与状态判断在装配层做。
///
/// `attempts` 是"已经跑过几次"（含当前这一次）；`next_at` 是下一次可派发的时刻
/// （Unix 毫秒，调用方注入）；`budget_ms` 是入队当时冻结的单次预算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// 作业行主键。
    pub id: JobId,
    /// 作业类型名。
    pub name: String,
    /// 不透明载荷的 JSON 文本（原样入队、原样回填）。
    pub payload_json: String,
    /// 幂等键（`(bot_id, idempotency_key)` 唯一）。
    pub idempotency_key: String,
    /// 状态原样字符串（`pending` / `running` / `done` / `failed` / `cancelled`）。
    pub state: String,
    /// 已经跑过几次（含当前这一次）。
    pub attempts: u32,
    /// 入队当时冻结的单次预算（毫秒）。
    pub budget_ms: u64,
    /// 回复目标的平台会话 `external_id`；`None` = 只能记日志。
    pub conversation: Option<String>,
    /// 下一次可派发的时刻（Unix 毫秒）。
    pub next_at: i64,
    /// 最近一次失败正文（没失败过或已成功为 `None`）。
    pub last_error: Option<String>,
    /// 入队时刻（Unix 毫秒）。
    pub created_at: i64,
}

/// 待入队的一条后台作业（[`crate::Store::enqueue_job`] 的入参）。
///
/// `attempts` 不在这里：新行恒为 0。`updated_at` 也不在这里：入队那一刻
/// 它等于 `created_at`，由存储层写，调用方不必重复传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// 作业行主键（由 `IdGenerator::job_id()` 生成）。
    pub id: JobId,
    /// 作业类型名。
    pub name: String,
    /// 不透明载荷的 JSON 文本。
    pub payload_json: String,
    /// 幂等键。
    pub idempotency_key: String,
    /// 入队当时冻结的单次预算（毫秒）。
    pub budget_ms: u64,
    /// 回复目标的平台会话 `external_id`。
    pub conversation: Option<String>,
    /// 首次可派发的时刻（Unix 毫秒）。
    pub next_at: i64,
    /// 入队时刻（Unix 毫秒）。
    pub created_at: i64,
}

/// 等待行（`bot_waits` 一行；[`crate::Store::current_wait`] 等的返回形状）。
///
/// 与表同列，**不**解析 `scope` / `state`：两者是原样字符串（拼写真源在
/// `hbot_core::bot_session` 的 `WaitScope::as_str()` / `WaitState::as_str()`，
/// 存储层不重复定义一份）。解析与状态判断在装配层做。
///
/// `deadline` 是「到这个时刻还没人回答就算超时」（Unix 毫秒，调用方注入）；
/// `fulfilled_message_id` 只有被唤醒的行才非空 —— 它把「谁回答了哪个等待」写进
/// 库里，排障时不必翻日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitRow {
    /// 等待行主键。
    pub id: WaitId,
    /// 等待键（`onResume` 的 `ctx.waitKey`）。
    pub key: String,
    /// 注册时发给用户的提示文本。
    pub prompt: String,
    /// 开等待的人（`sender_id`）；`scope = sender` 时只有他能回答。
    pub owner: String,
    /// 谁可以回答：`sender` / `conversation`。
    pub scope: String,
    /// 状态原样字符串：`pending` / `fulfilled` / `expired` / `cancelled`。
    pub state: String,
    /// 会话的平台外部 ID（回复目标，也是 `onResume` 里 `ctx.conversationExternalId`）。
    pub conversation: String,
    /// 到这个时刻还没人回答就算超时（Unix 毫秒）。
    pub deadline: i64,
    /// 唤醒它的那条消息 id；未唤醒为 `None`。
    pub fulfilled_message_id: Option<String>,
    /// 注册时刻（Unix 毫秒）。
    pub created_at: i64,
    /// 还能重发几次提示（0 = 到点就 expired）。
    pub reminders_left: u8,
    /// 每一轮等多久（Unix 毫秒）；重发时用它推后 `deadline`。
    pub timeout_ms: i64,
}

/// 待注册的一条等待（[`crate::Store::register_wait`] 的入参）。
///
/// `state` / `updated_at` 不在这里：新行恒为 `pending`、`updated_at = created_at`，
/// 由存储层写（与 [`NewJob`] 同一纪律）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWait {
    /// 等待行主键（由 `IdGenerator::wait_id()` 生成）。
    pub id: WaitId,
    /// 会话行主键。
    pub conversation_id: ConversationId,
    /// 会话的平台外部 ID。
    pub conversation_external_id: String,
    /// 开等待的人。
    pub owner: String,
    /// 等待键。
    pub key: String,
    /// 提示文本。
    pub prompt: String,
    /// 谁可以回答：`sender` / `conversation`。
    pub scope: String,
    /// 超时时刻（Unix 毫秒）。
    pub deadline: i64,
    /// 注册时刻（Unix 毫秒）。
    pub created_at: i64,
    /// 超时后还能重发几次提示（0 = 不重发，缺省语义）。
    pub reminders_left: u8,
    /// 每一轮等多久（Unix 毫秒）；重发时用它推后 `deadline`。
    pub timeout_ms: i64,
}
