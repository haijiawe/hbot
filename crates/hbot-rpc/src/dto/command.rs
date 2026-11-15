//! [`Command`]：UI → 核心的唯一入口。
//!
//! 线格式：`"type"` 是变体名的 snake_case（`app_info`、`file_write`），
//! 字段一律 camelCase（`sessionId`、`baseHash`、`acceptHunks`）。
//!
//! 每个命令的 `data` 形状见 [`super`] 的"命令 → data"表；`Command` 与
//! [`super::Event`] 都不在这两个文件之外定义（唯一真源）。

use hbot_core::{ApprovalDecision, ApprovalId, BotId, ConversationId, EditId, SearchId, SessionId};
use serde::{Deserialize, Serialize};

use super::StepKind;

/// UI 发往核心的一条命令。
///
/// 唯一入口是 `dispatch(command) -> Response`（桌面走 Tauri 命令，后台走
/// JSON-RPC 2.0 的 `dispatch` 方法）；不存在第二条通道。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// 应用信息：版本、平台、数据目录（启动时第一条命令）。
    AppInfo,

    /// 打开一个项目目录（校验 `hbot.json`、建 DB、起文件监听）。
    ProjectOpen {
        /// 项目根目录绝对路径。
        path: String,
    },

    /// 向 Agent 会话发送一条用户消息。
    AgentSend {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 用户输入。
        text: String,
    },

    /// 取消当前 turn（贯穿 provider / 工具 / 命令）。
    AgentCancel {
        /// Agent 会话 ID。
        session_id: SessionId,
    },

    /// 回应一次审批请求。
    AgentApprove {
        /// 审批请求 ID（来自 `approval.request` 事件）。
        approval_id: ApprovalId,
        /// 批准或拒绝。
        decision: ApprovalDecision,
        /// 是否登记"以后都按这个完整动作放行"（规则键含规范化参数与沙箱档）。
        always: bool,
    },

    /// 读文件。
    FileRead {
        /// 相对项目根（或绝对）的路径。
        path: String,
    },

    /// 写文件（原子写；`baseHash` 用于检测冲突）。
    FileWrite {
        /// 文件路径。
        path: String,
        /// 新内容（UTF-8 文本）。
        content: String,
        /// 打开时读到的内容哈希（来自 `FileRead` 的 `data.hash`）；不匹配时拒绝覆盖。
        #[serde(default)]
        base_hash: Option<String>,
    },

    /// 列目录（一层）。
    FileList {
        /// 目录路径。
        path: String,
    },

    /// 新建文件。
    FileCreate {
        /// 文件路径。
        path: String,
        /// 初始内容。
        content: String,
    },

    /// 重命名 / 移动文件。
    FileRename {
        /// 原路径。
        from: String,
        /// 新路径。
        to: String,
    },

    /// 删除文件。
    FileDelete {
        /// 文件路径。
        path: String,
        /// 是否移到回收站（否则永久删除）。
        to_trash: bool,
    },

    /// 应用一次 Agent 编辑中的若干块。
    EditApply {
        /// 编辑 ID（来自 `agent.diff` 事件）。
        edit_id: EditId,
        /// 接受的块序号（对应 `DiffHunk.index`）；空表示全部拒绝。
        accept_hunks: Vec<u32>,
    },

    /// 拒绝整次 Agent 编辑。
    EditReject {
        /// 编辑 ID。
        edit_id: EditId,
    },

    /// 开始全局搜索（结果通过 `file.search` 事件流式返回）。
    SearchStart {
        /// 查询串。
        query: String,
        /// 是否按正则解释。
        regex: bool,
        /// glob 过滤（空表示不过滤）。
        globs: Vec<String>,
    },

    /// 取消一次正在进行的搜索。
    SearchCancel {
        /// 搜索 ID（来自 `file.search` 事件）。
        search_id: SearchId,
    },

    /// 取诊断（编译错误 / 语言服务）。
    Diagnostics {
        /// 只要该文件的诊断；`None` 表示整个项目。
        #[serde(default)]
        path: Option<String>,
    },

    /// 启动 bot（起 supervisor、建连接、恢复 outbound）。
    BotStart {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 停止 bot（停止读取、取消、flush）。
    BotStop {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 向指定会话注入一条模拟入站消息（自测用，走与真实消息相同的链路）。
    BotInject {
        /// Bot ID。
        bot_id: BotId,
        /// 目标会话。
        conversation_id: ConversationId,
        /// 模拟的消息文本。
        text: String,
    },

    /// 设置断点（调试构建）。
    DebugSetBreakpoints {
        /// 源文件路径。
        file: String,
        /// 行号（1-based）。
        lines: Vec<u32>,
    },

    /// 继续执行（步进后的恢复）。
    DebugResume {
        /// Agent 会话 ID。
        session_id: SessionId,
    },

    /// 单步执行。
    DebugStep {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 步进方向。
        kind: StepKind,
    },

    /// 取最近 N 条运行日志。
    LogsTail {
        /// Bot ID。
        bot_id: BotId,
        /// 条数上限。
        limit: usize,
    },
}

impl Command {
    /// 线格式的 `"type"` 值（`app_info`、`file_write`…）。
    ///
    /// 供日志、错误信息与 `dispatch` 分支使用。必须与 serde 的 `tag` 一致：
    /// `crates/hbot-rpc/tests/commands.rs` 里有一条测试逐条比对序列化结果，
    /// 改名忘了同步这里就会红。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AppInfo => "app_info",
            Self::ProjectOpen { .. } => "project_open",
            Self::AgentSend { .. } => "agent_send",
            Self::AgentCancel { .. } => "agent_cancel",
            Self::AgentApprove { .. } => "agent_approve",
            Self::FileRead { .. } => "file_read",
            Self::FileWrite { .. } => "file_write",
            Self::FileList { .. } => "file_list",
            Self::FileCreate { .. } => "file_create",
            Self::FileRename { .. } => "file_rename",
            Self::FileDelete { .. } => "file_delete",
            Self::EditApply { .. } => "edit_apply",
            Self::EditReject { .. } => "edit_reject",
            Self::SearchStart { .. } => "search_start",
            Self::SearchCancel { .. } => "search_cancel",
            Self::Diagnostics { .. } => "diagnostics",
            Self::BotStart { .. } => "bot_start",
            Self::BotStop { .. } => "bot_stop",
            Self::BotInject { .. } => "bot_inject",
            Self::DebugSetBreakpoints { .. } => "debug_set_breakpoints",
            Self::DebugResume { .. } => "debug_resume",
            Self::DebugStep { .. } => "debug_step",
            Self::LogsTail { .. } => "logs_tail",
        }
    }
}
