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

use hbot_core::{BotId, BotStatus, ConversationId, HbotError, Timestamp};
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
