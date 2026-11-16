//! hbot-agent：唯一的 Agent 循环、工具网关、审批与 provider 抽象。
//!
//! 边界（见 `docs/03-directory.md`）：本 crate 只做 Agent 决策与工具编排，
//! 不做 Bot 生命周期、不定义 UI 契约、不直接访问平台协议。
//!
//! 模块：
//!
//! - [`r#loop`]：**唯一的** Agent 循环（`AGENTS.md` §0.3），带步数 / token / 时长预算；
//! - [`gateway`]：工具网关，固定闸门顺序（参数 → 存在 → 沙箱 → realpath → 审批 → 执行 → 审计）；
//! - [`approvals`]：审批规则键 `(tool, 规范化参数, sandbox_mode)` 与策略存储；
//! - [`edits`]：待审编辑（Agent 写入产出 Diff，由用户逐块接受/拒绝）；
//! - [`tools`]：工具注册表与具体工具（`read_file` / `write_file` / `edit_file` / `list_dir` / `search` + 两个壳）；
//! - [`provider`]：LLM provider 抽象与两个 SSE 客户端（OpenAI 兼容 [`provider::OpenAiProvider`]、
//!   Anthropic Messages [`provider::AnthropicProvider`]）+ 真实 HTTP 传输（[`provider::reqwest`]）；
//!   两者产出**同形**的 [`provider::ToolCallRequest`]，`loop`/`gateway` 不区分 provider；
//! - [`prompt`]：system prompt 与"外部内容是不可信数据"区块；
//! - [`cancel`]：贯穿 provider / 工具 / 搜索的取消令牌。
//!
//! 不变量：
//!
//! - 取消后不再发起新的 provider 请求；
//! - 预算超限返回明确错误，不静默续跑；
//! - 审批只对**完整动作**有效，禁止按类别整类放行。

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod approvals;
pub mod cancel;
pub mod edits;
pub mod gateway;
pub mod r#loop;
pub mod prompt;
pub mod provider;
pub mod tools;

#[cfg(test)]
mod test_support;

pub use approvals::{
    ApprovalKey, ApprovalPolicy, ApprovalReply, ApprovalRequest, ApprovalStore, Approver,
    DenyAllApprover,
};
pub use cancel::{CancellationToken, cancelled_error};
pub use edits::{EditHunk, EditProposer, ProposedEdit, apply_hunks, diff_hunks};
pub use gateway::{
    AuditEvent, AuditOutcome, AuditSink, GatewayError, GatewayOutcome, GatewayStep, NullAudit,
    ToolGateway, sandbox_rank,
};
pub use hbot_core::SandboxMode;
pub use r#loop::{
    DEFAULT_MAX_DURATION, DEFAULT_MAX_STEPS, NullTurnSink, TurnConfig, TurnInput, TurnOutcome,
    TurnSink, run_turn,
};
pub use prompt::{PromptContext, UNTRUSTED_TAG, escape_untrusted, system_prompt, untrusted_block};
pub use provider::{
    AnthropicProvider, ChatMessage, Completion, CompletionRequest, FinishReason, LlmProvider,
    NullSink, OpenAiProvider, Provider, ProviderError, ReqwestTransport, StreamSink,
    ToolCallRequest, ToolSpec, Usage,
};
pub use tools::{
    DEFAULT_TOOL_TIMEOUT, MAX_TOOL_OUTPUT_BYTES, Risk, Tool, ToolCtx, ToolOutput, ToolRegistry,
    clamp_output, normalize_relative, validate_args,
};
