//! [`Event`]：核心 → UI 的推送。
//!
//! 线格式：`"type"` 是点分小写（`agent.delta`、`bot.status`），字段一律
//! camelCase（`sessionId`、`callStack`）。点分名无法由 `rename_all` 推导，
//! 所以在每个变体上用 `#[serde(rename = "...")]` 显式写出。
//!
//! 投递语义见 `docs/02-architecture.md` §9.2：每连接有界队列（1024），
//! `agent.delta` 可合并，`approval.request` / `bot.status` 优先；UI 断线重连后
//! 先拉快照再订阅增量。

use hbot_core::{ApprovalId, BotId, BotStatus, EditId, SearchId, SessionId, Timestamp, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    DebugScope, Diagnostic, EditFile, FileChangeKind, LogLevel, RiskLevel, SearchMatch, StackFrame,
};

/// 核心推给 UI 的事件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// 助手流式文本增量。
    #[serde(rename = "agent.delta")]
    AgentDelta {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 本段增量文本。
        text: String,
    },

    /// 一步执行完成（工具调用批次或最终回复）。
    #[serde(rename = "agent.step")]
    AgentStep {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 从 1 开始的步序。
        step: u32,
    },

    /// 一次工具调用的状态变化。
    #[serde(rename = "agent.tool")]
    AgentTool {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 工具调用快照。
        call: ToolCall,
    },

    /// 需要用户审批（阻塞等待，超时按拒绝处理）。
    #[serde(rename = "approval.request")]
    ApprovalRequest {
        /// 审批请求 ID，回应时带回 `AgentApprove.approvalId`。
        approval_id: ApprovalId,
        /// 工具名。
        tool: String,
        /// 已校验的调用参数（用于展示"完整动作"）。
        #[ts(type = "unknown")]
        args: Value,
        /// 风险等级（UI 用颜色/文案区分）。
        risk: RiskLevel,
    },

    /// bot 运行状态变化。
    #[serde(rename = "bot.status")]
    BotStatus {
        /// Bot ID。
        bot_id: BotId,
        /// 新状态。
        status: BotStatus,
        /// 进入该状态的原因（如断线、编译错误）；正常状态为 `None`。
        #[serde(default)]
        error: Option<String>,
    },

    /// 运行日志一行（`logs` 表的流式投影）。
    #[serde(rename = "run.log")]
    RunLog {
        /// Bot ID。
        bot_id: BotId,
        /// 日志级别。
        level: LogLevel,
        /// 日志正文（已脱敏）。
        line: String,
        /// 产生时间。
        at: Timestamp,
    },

    /// 项目目录变化（文件监听，已去抖）。
    #[serde(rename = "project.changed")]
    ProjectChanged {
        /// 变化文件的项目内相对路径。
        path: String,
        /// 变化类型。
        kind: FileChangeKind,
    },

    /// 搜索结果增量（流式，可能多次）。
    #[serde(rename = "file.search")]
    FileSearch {
        /// 搜索 ID（用于 `SearchCancel`）。
        search_id: SearchId,
        /// 本次匹配。
        matches: Vec<SearchMatch>,
    },

    /// 诊断结果（编译错误 / 语言服务）。
    #[serde(rename = "diagnostics")]
    Diagnostics {
        /// 文件路径。
        path: String,
        /// 该文件的诊断项。
        items: Vec<Diagnostic>,
    },

    /// 待审查的 Agent 写入（Diff）；用户逐块接受或整体拒绝。
    #[serde(rename = "agent.diff")]
    AgentDiff {
        /// 编辑 ID（用于 `EditApply` / `EditReject`）。
        edit_id: EditId,
        /// 涉及的文件与块。
        files: Vec<EditFile>,
    },

    /// 调试命中断点，worker 已阻塞等待。
    #[serde(rename = "debug.paused")]
    DebugPaused {
        /// Agent 会话 ID。
        session_id: SessionId,
        /// 命中断点的文件。
        file: String,
        /// 命中行号（1-based）。
        line: u32,
        /// 作用域与局部变量（不可序列化的值显示为 `<unserializable>`）。
        scopes: Vec<DebugScope>,
        /// 逻辑调用栈（外层在前）。
        call_stack: Vec<StackFrame>,
    },

    /// 调试继续执行。
    #[serde(rename = "debug.resumed")]
    DebugResumed {
        /// Agent 会话 ID。
        session_id: SessionId,
    },
}
