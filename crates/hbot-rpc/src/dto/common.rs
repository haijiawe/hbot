//! 命令与事件共用的载荷类型、结果类型，以及 `Response` / `ApiError`。
//!
//! 这些类型的字段全部 camelCase（`rename_all = "camelCase"`），枚举值全部
//! snake_case，与 [`super::Command`] / [`super::Event`] 的线上风格一致。
//!
//! 三类东西：
//!
//! - **事件载荷**：[`SearchMatch`]、[`Diagnostic`]、[`EditFile`]、[`DiffHunk`]、
//!   [`DebugScope`]、[`StackFrame`]、[`RiskLevel`]、[`LogLevel`]、[`FileChangeKind`]；
//! - **命令结果**：[`AppInfo`]、[`ProjectOpenResult`]、[`ProjectSummary`]、
//!   [`FileContent`]、[`DirEntry`]、[`RunLog`]，作为 [`Response::with_data`] 的
//!   `data`（形状对照见 [`super`] 的表格）；
//! - **传输包装**：[`Response`]、[`ApiError`]。

use std::collections::BTreeMap;

use hbot_core::{BotId, BotStatus, ConversationId, HbotError, JobId, Timestamp, WaitId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 调试步进方向（`DebugStep.kind`），语义见 `docs/02-architecture.md` §6.5。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// 同深度下一条语句（step over）。
    Over,
    /// 进入被调用函数（step into）。
    Into,
    /// 回到上一层（step out）。
    Out,
}

/// 审批风险等级（`approval.request.risk`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// 只读、项目内、可逆。
    Low,
    /// 写文件、外部只读访问。
    Medium,
    /// 执行命令、项目外写入、访问密钥。
    High,
}

/// 日志级别（`run.log.level`，对应 `tracing` 的五档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 追踪（默认不展示）。
    Trace,
    /// 调试。
    Debug,
    /// 信息。
    Info,
    /// 警告。
    Warn,
    /// 错误。
    Error,
}

/// 文件变化类型（`project.changed.kind`，由文件监听去抖后分类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// 新建。
    Created,
    /// 内容或元数据变化。
    Updated,
    /// 删除。
    Deleted,
    /// 重命名 / 移动（旧路径与新路径各发一次）。
    Renamed,
}

/// 一条 bot 摘要（`bot_list` 的 `data` 元素）。
///
/// 字段只取库里真的有的列：`bots` 表提供 `id` / `name` / `platform`，
/// `bot_status` 表提供 `status` / `updated_at`（从未变更为 0）。`status` 是
/// **本进程的活跃态优先**：supervisor 在跑就是 `running`，占位中就是 `starting`；
/// 否则回落库里最后一次落盘的状态——崩溃残留的 `running` 行不会让 UI 以为它活着。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct BotSummary {
    /// Bot ID。
    pub bot_id: BotId,
    /// 展示名（`bots.name`）。
    pub name: String,
    /// 平台（`bots.platform`，如 `onebot11`）。
    pub platform: String,
    /// 当前状态。
    pub status: BotStatus,
    /// 状态最后一次变更的时间（毫秒时间戳）；从未变更为 0。
    pub updated_at: Timestamp,
}

/// 某个 bot 已登记的会话（`bot_conversations` 的 `data` 元素）。
///
/// `kind` 是**开放字符串**（`private` / `group` / `channel`），不是枚举：平台将来多一种
/// 会话形态时不应该逼着这条命令先改 DTO 才能返回。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    /// 会话 ID（本地唯一，`bot_inject` 用它）。
    pub conversation_id: ConversationId,
    /// 平台侧的会话标识（如 `local:default`、群 `group_openid`、QQ 号）。
    pub external_id: String,
    /// 会话形态：`private` / `group` / `channel`。
    pub kind: String,
}

/// 某个 bot 的设备会话快照（`device_status` / `device_grant` 的 `data`，也是
/// `device.status` 事件的载荷）。
///
/// **字段用开放字符串而不是 Rust 枚举**，与 [`ConversationSummary::kind`] 同一个理由：
/// 设备能力是产品面最容易继续长的一处（将来加 `audio` 之类），不该为它先改 DTO。
/// 合法值见 `hbot_core::DeviceCapability::as_str` 与 `DeviceSessionState::as_str`。
///
/// `state` 为 `none` 表示"从来没有过会话"，此时 `sessionId` / `grantedAt` / `deadline`
/// 都是 `null` —— 这一条是给 UI 画状态条用的，别拿它当"过期的会话"。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    /// Bot ID。
    pub bot_id: BotId,
    /// 会话 ID；无会话为 `null`。
    pub session_id: Option<String>,
    /// `none` / `requested` / `active` / `expired` / `revoked` / `denied`。
    pub state: String,
    /// 这个会话**生效的**子能力（清单∩设置∩会话，取交集）。
    pub sub: Vec<String>,
    /// 这个会话允许的应用（映像名，小写）。
    pub allowed_apps: Vec<String>,
    /// 会话开始时间（毫秒）；无会话为 `null`。
    pub granted_at: Option<Timestamp>,
    /// 到期时间（毫秒）；无会话为 `null`。
    pub deadline: Option<Timestamp>,
    /// 剩余毫秒；无会话或已结束时为 `null`（UI 不该自己拿 deadline 减本机时钟）。
    pub remaining_ms: Option<i64>,
}

/// 宿主级设备设置（`device_settings_get` / `device_settings_set` 的 `data`，
/// 也是 `device.settings` 事件的载荷）。
///
/// 这是**这台机器**的设置，不属于某个 bot；某个 bot 能做什么由会话
/// （[`DeviceStatus`]）与清单 `permissions.device` 再收窄。
///
/// `capabilities` 与 `allowedApps` 的空集语义**故意相反**，别"统一"：
/// - `capabilities` 空 = **全拒**（能力是开一类就整类可用，空集必须最严）；
/// - `allowedApps` 空 = **不设上限**（应用是在用户已点头那批里再收窄，
///   若空集也当全拒，用户就得在两处各抄一份名单，最后只会把名单写宽）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSettings {
    /// 总开关。关掉之后，Agent 与 bot 代码的所有设备调用都拒（`HBOT_DEVICE_NOT_ENABLED`）。
    pub enabled: bool,
    /// 允许的子能力（`screen` / `ocr` / `input` / `window` / `clipboard`）；空 = 全拒。
    pub capabilities: Vec<String>,
    /// 宿主级应用上限（映像名，小写）；空 = 不设上限。
    pub allowed_apps: Vec<String>,
    /// `device_grant` 不传 `minutes` 时的默认时长。
    pub default_minutes: u32,
    /// 一次会话最长多少分钟，硬上限 30。
    pub max_minutes: u32,
    /// 一次会话最多多少次动作（Agent 与 bot 代码共用一个计数）。
    pub max_actions_per_session: u32,
}

/// 一条搜索匹配（`file.search.matches`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    /// 项目内相对路径。
    pub path: String,
    /// 行号（1-based）。
    pub line: u32,
    /// 列号（1-based；单位由 `hbot-workspace` 统一后写进实现文档，UI 只做展示）。
    pub column: u32,
    /// 命中行文本（已截断，长度由搜索实现控制）。
    pub text: String,
}

/// 一条诊断（`diagnostics.items`，也是 `Diagnostics` 命令的 data 元素）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// 文件路径。
    pub path: String,
    /// 起始行（1-based）。
    pub line: u32,
    /// 起始列（1-based）。
    pub column: u32,
    /// 严重级别。
    pub severity: DiagnosticSeverity,
    /// 可展示的消息。
    pub message: String,
    /// 错误码（如 `TS2304`）；没有时为 `None`。
    #[serde(default)]
    pub code: Option<String>,
}

/// 诊断严重级别（与 LSP 对齐）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// 错误。
    Error,
    /// 警告。
    Warning,
    /// 信息。
    Info,
    /// 提示（弱化展示）。
    Hint,
}

/// 一个待审查文件的 Diff（`agent.diff.files`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct EditFile {
    /// 文件路径。
    pub path: String,
    /// Diff 块；用户可逐块接受。
    pub hunks: Vec<DiffHunk>,
}

/// 一个 Diff 块。
///
/// `index` 用于 [`super::Command::EditApply`] 的 `acceptHunks`；行号都是 1-based，
/// 与 `docs/02-architecture.md` §5.3 的"逐块接受/拒绝"对应。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    /// 块序号（在一次编辑内唯一，从 0 开始）。
    pub index: u32,
    /// 原文件起始行。
    pub old_start: u32,
    /// 原文件行数。
    pub old_lines: u32,
    /// 新文件起始行。
    pub new_start: u32,
    /// 新文件行数。
    pub new_lines: u32,
    /// 统一 diff 文本行（含 `+` / `-` / 空格前缀）。
    pub lines: Vec<String>,
}

/// 一个作用域及其局部变量（`debug.paused.scopes`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DebugScope {
    /// 作用域名（如 `locals`、函数名）。
    pub name: String,
    /// 变量名 → 值；无法序列化的值由宿主替换成字符串 `<unserializable>`。
    ///
    /// 值是任意 JSON，TS 侧只能给 `unknown`（`any` 会关掉全部检查，禁止使用）。
    #[ts(type = "Record<string, unknown>")]
    pub variables: BTreeMap<String, Value>,
}

/// 一帧逻辑调用栈（`debug.paused.callStack`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct StackFrame {
    /// 函数名（匿名函数为 `<anonymous>`）。
    pub name: String,
    /// 所在文件路径。
    pub file: String,
    /// 行号（1-based）。
    pub line: u32,
    /// 列号（1-based）。
    pub column: u32,
}

/// 一个断点：行号 + 可选条件。
///
/// 这是 [`Command::DebugSetBreakpoints`](crate::dto::Command::DebugSetBreakpoints)
/// 的元素类型。断点是**整份替换**语义：一次命令给出该文件的全部断点，不在列表里的行
/// 会被清掉；空数组表示取消该文件的全部断点。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DebugBreakpoint {
    /// 行号（1-based）。`0` 无意义，宿主会忽略。
    pub line: u32,
    /// 停下之前必须为真的条件；`null` 表示无条件断点。
    ///
    /// 是一段**同步 JS 表达式**，在 bot 的 isolate 里求值——那里才有 `ctx`、局部变量
    /// 与模块作用域，也只有那里求值才和用户在编辑器里看到的一致。两条硬规则：
    ///
    /// - 只支持同步表达式：`await` 是语法错误（会当成"不命中"并记一条 warn），
    ///   而不是"异步等待后判断"。
    /// - 求值抛错、编译失败、引用了不存在的名字，一律当作**不命中**，绝不打断 handler；
    ///   同一行只 warn 一次，免得一个坏条件刷满日志。
    ///
    /// 副作用允许（那是用户自己的代码在自己的 isolate 里跑），但求值次数受命中点数量
    /// 限制：只有真的执行到这一行才会求值。
    pub condition: Option<String>,
    /// 日志点消息模板；`null` 表示这是一个**断点**（会停下来）。
    ///
    /// 非 `null` 时这一行变成**日志点**：命中时**不暂停**，只发一条
    /// [`Event::DebugLog`](crate::dto::Event::DebugLog)，然后照常往下跑。模板里可以写
    /// `{表达式}`，由 isolate 在命中那一刻展开（例如 `"count={count}"`）——单个表达式
    /// 求值失败就把**那一段**换成 `<错误>`（同一条消息的其余部分照常输出），模板本身
    /// 编译失败（括号不配对等）则这个日志点静默（同一行只 warn 一次），两种情况都
    /// 绝不打断 handler。
    ///
    /// **`log` 与 `condition` 不能在同一条条目上组合**：带非空 `log` 的条目一律按
    /// "只打印、不停下"处理，它自己的 `condition` 会被丢弃（宿主不报错，因为 wire 上
    /// 表达"带条件的日志点"还有另一条路）。要在某一行**既打印又停下**，就给该行发
    /// **两条**条目：一条 `log: null`（可带 `condition`，命中并条件为真时停下），一条
    /// `log: "..."`（无条件打印）。两路各记各的表，互不清除。
    pub log: Option<String>,
}

/// `AppInfo` 命令的 data：应用与运行时信息。
///
/// 启动时 UI 的第一条命令；`dataDir` 用于"打开日志目录/数据目录"这类入口。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// 产品名（如 `hbot`）。
    pub name: String,
    /// 版本号（`CARGO_PKG_VERSION` + 构建号）。
    pub version: String,
    /// 目标平台（`windows` / `macos` / `linux`）。
    pub platform: String,
    /// 数据目录绝对路径（DB、日志、缓存）。
    pub data_dir: String,
}

/// `ProjectCreate` 命令的 data：刚建出来的那个项目的摘要。
///
/// **不要**复用它描述进程（版本 / 平台 / 数据目录是 [`AppInfo`] 的活）：这个类型
/// 说的是"哪个项目"，给 UI 的是打开入口文件所需的三件事。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    /// 项目根目录绝对路径（规范化后的，可直接用作后续文件命令的根）。
    pub path: String,
    /// 展示名（`hbot.json` 的 `name`）。
    pub name: String,
    /// 入口文件的项目内相对路径（默认 `src/main.ts`，`/` 分隔）；UI 打开它即可。
    pub entry: String,
}

/// `ProjectOpen` 命令的 data：核心规范化后的项目根路径。
///
/// **只有路径**：打开一个已有目录不要求它已经有 `hbot.json`（脚手架是
/// [`super::Command::ProjectCreate`] 的事），所以这里不回 `name` / `entry`。
/// 需要项目摘要的调用方应当用 `project_create` 的 [`ProjectSummary`]。
///
/// 字段是**绝对路径**，Windows 上可能是带 `\\?\` 前缀的 verbatim 路径；它只用来
/// 标识"哪个项目"（拼 journal / 项目表的键），不要拿它做字符串前缀比较。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOpenResult {
    /// 核心规范化后的绝对路径（Windows 上可能带 `\\?\` 前缀）。
    pub path: String,
}

/// `FileRead` / `FileWrite` / `FileCreate` 命令的 data：文件内容与内容哈希。
///
/// `hash` 是 [`super::Command::FileWrite`] 的 `baseHash` 来源：UI 读文件拿到
/// hash，写回时带上；不匹配则拒绝覆盖（`AGENTS.md` §0.10，禁止静默覆盖）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    /// 文件文本内容（UTF-8）。
    pub content: String,
    /// 内容哈希，算法与格式由 `hbot-workspace` 定义并写入文档（如 `sha256:...`）。
    pub hash: String,
}

/// `FileList` 命令的 data 元素：一层目录项。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    /// 文件 / 目录名（不含父路径）。
    pub name: String,
    /// 项目内相对路径（`/` 分隔，UI 直接用作树节点 key）。
    pub path: String,
    /// 是否目录。
    pub is_dir: bool,
}

/// `LogsTail` 命令的 data 元素；与 `run.log` 事件载荷相比多 `source`、少 `botId`
/// （事件只投影单个 bot 的实时日志）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct RunLog {
    /// 产生时间。
    pub at: Timestamp,
    /// 日志级别。
    pub level: LogLevel,
    /// 日志正文（已脱敏）。
    pub line: String,
    /// 日志来源：`bot` / `agent`（`logs_tail` 用它区分 Agent 行）。
    pub source: String,
}

/// 回放里"顺序不同"的一条回复：文本按多重集匹配上了，但相对位次变了。
///
/// 下标都是 0 起：`historicalIndex` 指历史出站里的位次，`replayedIndex` 指本次
/// 重跑结果里的位次。UI 用它把两边并排高亮。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReplyChange {
    /// 回复文本。
    pub text: String,
    /// 在历史出站里的下标。
    pub historical_index: u32,
    /// 在本次重跑结果里的下标。
    pub replayed_index: u32,
}

/// 一条历史入站事件的回放明细（`ReplayRun` 的 data 元素）。
///
/// `historical` 与 `replayed` 是**原始对照**，`same`/`extra`/`missing`/`reordered`
/// 是同一对数组的分类结果 —— UI 想简单就渲染后者，想自己算就渲染前者。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayEntry {
    /// 入站行主键。
    pub inbound_id: String,
    /// 入站行的 `external_id`（也是历史出站的 `reply_to`）。
    pub external_id: String,
    /// 会话的外部 id。
    pub conversation_external_id: String,
    /// 入站时间（Unix 毫秒，取平台给的 `timestamp`，不是本地收到的时间）。
    pub received_at: i64,
    /// 结论：`match` / `diff` / `error`（开放字符串，未知值 UI 按 `diff` 处理）。
    pub status: String,
    /// 历史出站文本（按 `created_at, rowid` 升序）。
    pub historical: Vec<String>,
    /// 本次重跑产生的回复（按 `reply(...)` 调用顺序）。
    pub replayed: Vec<String>,
    /// 逐条对比：一致。
    pub same: Vec<String>,
    /// 逐条对比：多出（重跑有、历史没有）。
    pub extra: Vec<String>,
    /// 逐条对比：缺少（历史有、重跑没有）。
    pub missing: Vec<String>,
    /// 逐条对比：顺序不同。
    pub reordered: Vec<ReplayReplyChange>,
    /// 结论为 `error` 时的原因。
    pub error: Option<String>,
}

/// `ReplayRun` 的返回：一次回放的完整报告。
///
/// **回放不写库、不联网**：它另起一个全新 isolate 逐条 dispatch，回复只收集在内存里
/// （`dispatch_collecting`），所以对正在跑的 bot 与真实平台都没有副作用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    /// 回放用的项目目录（原样回显）。
    pub project: String,
    /// 回放的 bot id。
    pub bot: String,
    /// 时间下界（Unix 毫秒）；`null` 表示不限。
    pub from: Option<i64>,
    /// 要求的上限；`null` 表示全量。
    pub limit: Option<u32>,
    /// 实际回放的事件条数。
    pub events: u32,
    /// 其中一致的条数。
    pub matched_events: u32,
    /// 其中有差异的条数。
    pub differing_events: u32,
    /// 其中重跑失败的条数。
    pub failed_events: u32,
    /// 逐条回复统计：一致。
    pub same: u32,
    /// 逐条回复统计：多出。
    pub extra: u32,
    /// 逐条回复统计：缺少。
    pub missing: u32,
    /// 逐条回复统计：顺序不同。
    pub reordered: u32,
    /// 逐条明细。
    pub entries: Vec<ReplayEntry>,
}

/// `ReplaySaveFixture` 的返回：刚才那份夹具落到了哪里。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct SavedFixture {
    /// 实际写入的绝对路径（`path` 相对时按项目根解析后的结果）。
    pub path: String,
    /// 写进去的事件条数。
    pub events: u32,
}

/// 夹具里的一条事件跑出来的结果（`FixtureRun` 的 data 元素）。
///
/// 与 [`ReplayEntry`] 的区别：夹具**没有历史出站**可对照，所以这里只报"这条事件
/// 跑出了什么回复 / 有没有抛错"，不做 same/extra/missing 分类。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct FixtureEntry {
    /// 在夹具数组里的下标（0 起）。
    pub index: u32,
    /// 原始事件里的 `external_id`（缺失时为空串）。
    pub external_id: String,
    /// 原始事件里的文本（缺失或纯图片时为 `null`）。
    pub text: Option<String>,
    /// 这条事件跑出来的回复（按 `reply(...)` 调用顺序）。
    pub replies: Vec<String>,
    /// 这条事件抛错时的原因；没抛错为 `null`。
    pub error: Option<String>,
}

/// `FixtureRun` 的返回：把一份夹具从头跑一遍的结果。
///
/// **不写库、不联网**：与回放一样另起一个全新 isolate 逐条 dispatch，回复只在内存里。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct FixtureRunReport {
    /// 夹具文件的绝对路径（原样回显）。
    pub path: String,
    /// 实际跑的事件条数。
    pub events: u32,
    /// 回复总条数。
    pub replies: u32,
    /// 抛错的事件条数。
    pub errors: u32,
    /// 逐条明细。
    pub entries: Vec<FixtureEntry>,
}

/// `BotJobs` 命令的 data 元素：一条后台作业的只读快照（`bot_jobs` 一行）。
///
/// 字段全是**开放字符串**（`state` 不是枚举）：状态集合在 `hbot_core::bot_jobs` 里
/// 还会长，UI 遇到不认识的值要当「未知状态」原样显示，不能 throw。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    /// 作业行主键。
    pub id: JobId,
    /// 作业类型名（清单里 `capabilities.jobs` 的那一条的 `name`）。
    pub name: String,
    /// 状态原样字符串：`pending` / `running` / `done` / `failed` / `cancelled`。
    pub state: String,
    /// 已经跑过几次（含当前这一次）；重试策略按它判。
    pub attempts: u32,
    /// 入队当时冻结的单次预算（毫秒）。
    pub budget_ms: u64,
    /// 回复目标的平台会话 `externalId`；`null` = 只能记日志。
    pub conversation: Option<String>,
    /// 下一次可派发的时刻（Unix 毫秒）。
    pub next_at: i64,
    /// 最近一次失败正文；没失败过或已成功为 `null`。
    pub last_error: Option<String>,
    /// 入队时刻（Unix 毫秒）。
    pub created_at: i64,
}

/// `BotWaits` 命令的 data 元素：一条会话等待的只读快照（`bot_waits` 一行）。
///
/// 与 [`JobView`] 同一口径：`state` / `scope` 是开放字符串，不认识的按原样显示。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct WaitView {
    /// 等待行主键。
    pub id: WaitId,
    /// 等待键（`onResume` 的 `ctx.waitKey`）。
    pub key: String,
    /// 注册时发给用户的提示文本（面板原样显示，**不再**重发）。
    pub prompt: String,
    /// 谁可以回答：`sender` / `conversation`。
    pub scope: String,
    /// 状态原样字符串：`pending` / `fulfilled` / `expired` / `cancelled`。
    pub state: String,
    /// 会话的平台外部 ID（回复目标）。
    pub conversation: String,
    /// 到这个时刻还没人回答就算超时（Unix 毫秒）。
    pub deadline: i64,
    /// 还能重发几次提示（0 = 到点就 expired）。
    pub reminders_left: u8,
    /// 每一轮等多久（毫秒）；重发时用它推后 `deadline`。
    pub timeout_ms: i64,
    /// 注册时刻（Unix 毫秒）。
    pub created_at: i64,
}

/// 命令的返回结果。
///
/// **这是唯一错误通道**：`dispatch(command) -> Response`，桌面与后台都不再包
/// `Result<Response, ApiError>`（否则错误有两个来源）。需要拆成传输层形状时用
/// [`Response::into_result`]。
///
/// 用 tagged 枚举而不是 `{ok: bool, data?, error?}`，避免出现"既无 data 也无
/// error"的非法状态。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// 成功；`data` 为命令结果（无结果时为 `null`，形状见 [`super`] 的表）。
    Ok {
        /// 命令返回数据。
        #[serde(default)]
        // 任意 JSON：TS 侧用 `unknown` 而非 `any`，调用方必须显式收窄。
        #[ts(type = "unknown | null")]
        data: Option<Value>,
    },

    /// 失败。
    Error {
        /// 错误详情。
        error: ApiError,
    },
}

impl Response {
    /// 成功且没有返回数据。
    pub fn ok() -> Self {
        Self::Ok { data: None }
    }

    /// 成功并携带 JSON 数据。
    pub fn with_data(data: Value) -> Self {
        Self::Ok { data: Some(data) }
    }

    /// 失败。
    pub fn error(error: ApiError) -> Self {
        Self::Error { error }
    }

    /// 拆成 `Result`：传输层（JSON-RPC 的 `result` / `error` 字段、Tauri 的
    /// `Result<T, E>`）需要这个形状。成功但无数据时为 `Ok(None)`。
    pub fn into_result(self) -> Result<Option<Value>, ApiError> {
        match self {
            Self::Ok { data } => Ok(data),
            Self::Error { error } => Err(error),
        }
    }
}

/// 错误对象：`{ code, message, data }`。
///
/// - `code` 来自 [`HbotError::code`]（`-32001..=-32006`），是 UI 判断错误类别的唯一依据；
/// - `message` 可直接展示，不得包含密钥（脱敏规则见 `docs/02-architecture.md` §11.3）；
/// - `data` 是结构化补充信息，字段因错误而异。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct ApiError {
    /// RPC 错误码（`-32001` 资源不存在 … `-32006` 内部错误）。
    pub code: i32,
    /// 可展示的错误信息。
    pub message: String,
    /// 结构化补充信息；没有时序列化为 `null`。
    #[serde(default)]
    // 形状因错误而异（如 `{ entity, id }` / `{ operation, timeoutMs }`），
    // UI 需要按 `code` 收窄，不能用 `any`。
    #[ts(type = "unknown | null")]
    pub data: Option<Value>,
}

impl From<HbotError> for ApiError {
    fn from(error: HbotError) -> Self {
        let data = match &error {
            HbotError::NotFound { entity, id } => Some(serde_json::json!({
                "entity": entity,
                "id": id,
            })),
            HbotError::Timeout {
                operation,
                timeout_ms,
            } => Some(serde_json::json!({
                "operation": operation,
                "timeoutMs": timeout_ms,
            })),
            HbotError::Invalid { .. }
            | HbotError::Conflict { .. }
            | HbotError::Denied { .. }
            | HbotError::Internal { .. } => None,
        };

        Self {
            code: error.code(),
            message: error.to_string(),
            data,
        }
    }
}
