//! LLM provider 抽象。
//!
//! 只有一种 provider 接口，循环不关心具体厂商：
//!
//! - [`Provider::complete`] 一次调用完成一次"回合内的单步"（流式回调 + 最终结果）；
//! - [`StreamSink`] 接收流式增量，供 UI 边生成边显示；
//! - [`CompletionRequest`] 自带 [`CancellationToken`]，provider 必须在等待过程中响应取消，
//!   取消后不得再发新的网络请求。
//!
//! 具体实现见 [`openai`]（OpenAI 兼容 `/chat/completions` SSE）；SSE 解析被拆成纯函数
//! 与纯状态机，测试不碰网络。真实传输在 [`reqwest`]（同样是"只接
//! [`HttpTransport`]"，SSE 解析仍归 `openai`）。

pub mod openai;
pub mod reqwest;

pub use openai::OpenAiProvider;
pub use reqwest::ReqwestTransport;

use async_trait::async_trait;
use hbot_core::HbotError;
use serde_json::Value;

use crate::cancel::CancellationToken;

/// 对话消息。
#[derive(Clone, Debug, PartialEq)]
pub enum ChatMessage {
    /// system prompt（含不可信数据规则）。
    System {
        /// 文本内容。
        content: String,
    },
    /// 用户消息（含来自平台的消息，必须已被包进不可信区块）。
    User {
        /// 文本内容。
        content: String,
    },
    /// 助手消息；可以同时带文本与工具调用。
    Assistant {
        /// 文本内容；纯工具调用回合为 `None`。
        content: Option<String>,
        /// 本步请求的工具调用。
        tool_calls: Vec<ToolCallRequest>,
    },
    /// 工具结果回填（`tool_call_id` 必须与请求对应）。
    Tool {
        /// 对应的工具调用 ID。
        tool_call_id: String,
        /// 工具输出（错误也走这里，让模型看到并调整）。
        content: String,
    },
}

impl ChatMessage {
    /// 构造 system 消息。
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    /// 构造 user 消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    /// 构造助手文本消息。
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        }
    }

    /// 构造工具结果消息。
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

/// 模型请求的一次工具调用（参数已解析为 JSON）。
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallRequest {
    /// provider 给的调用 ID；回填工具结果时必须原样带回。
    pub id: String,
    /// 工具名（必须存在于 [`crate::tools::ToolRegistry`]）。
    pub name: String,
    /// 参数；约定是 JSON object，具体 schema 由工具声明。
    pub arguments: Value,
}

/// 暴露给模型的工具声明。
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    /// 工具名。
    pub name: String,
    /// 一句话描述。
    pub description: String,
    /// JSON Schema（`type: "object"`）。
    pub parameters: Value,
}

/// token 用量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// 输入 token。
    pub prompt_tokens: u64,
    /// 输出 token。
    pub completion_tokens: u64,
    /// 合计 token（provider 没给时等于前两者之和）。
    pub total_tokens: u64,
}

impl Usage {
    /// 累加（预算统计用；溢出时饱和而不是 panic）。
    pub fn add(&mut self, other: Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    /// 把 `total_tokens` 补成前两者之和（provider 省略 `total_tokens` 时用）。
    pub fn with_total_from_parts(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.prompt_tokens.saturating_add(self.completion_tokens);
        }
        self
    }
}

/// 结束原因。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FinishReason {
    /// 正常结束。
    #[default]
    Stop,
    /// 达到输出上限，内容被截断。
    Length,
    /// 这一步是工具调用。
    ToolCalls,
    /// 被内容过滤拦下。
    ContentFilter,
    /// 其它/未知原因（保留原始字符串）。
    Other(String),
}

impl FinishReason {
    /// 由线上字符串（OpenAI 风格的 `finish_reason`）解析。
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" | "function_call" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 一次 `complete` 的结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    /// 助手文本（纯工具调用回合可能为空）。
    pub content: Option<String>,
    /// 本步请求的工具调用。
    pub tool_calls: Vec<ToolCallRequest>,
    /// token 用量（provider 未提供时为零）。
    pub usage: Usage,
    /// 结束原因。
    pub finish_reason: FinishReason,
}

impl Completion {
    /// 本步是否有工具调用。
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// 文本内容（空为 `""`）。
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// 流式增量出口。
///
/// 只做展示与统计，不参与决策：决策以 [`Provider::complete`] 的返回值为准，
/// 这样"半路取消/断流"不会产生半个工具调用被执行的窗口。
pub trait StreamSink: Send {
    /// 一段助手文本增量。
    fn on_text_delta(&mut self, delta: &str);

    /// 一个**完整**的工具调用（增量拼好之后才回调）。
    fn on_tool_call(&mut self, call: &ToolCallRequest);

    /// token 用量。
    fn on_usage(&mut self, usage: Usage);
}

/// 丢弃所有增量的 sink（无 UI 场景 / 测试）。
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl StreamSink for NullSink {
    fn on_text_delta(&mut self, _delta: &str) {}

    fn on_tool_call(&mut self, _call: &ToolCallRequest) {}

    fn on_usage(&mut self, _usage: Usage) {}
}

/// 一次完整请求。
#[derive(Clone, Debug)]
pub struct CompletionRequest {
    /// 模型名（provider 决定如何解释）。
    pub model: String,
    /// 完整对话（第一条通常是 system）。
    pub messages: Vec<ChatMessage>,
    /// 可用工具声明；空表示不允许工具调用。
    pub tools: Vec<ToolSpec>,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 单步输出上限。
    pub max_tokens: Option<u32>,
    /// 取消令牌：provider 必须响应，且取消后不再发新请求。
    pub cancel: CancellationToken,
}

/// LLM provider。
#[async_trait]
pub trait Provider: Send + Sync {
    /// 执行一次补全；增量走 `sink`，最终结果作为返回值。
    ///
    /// 实现约定：
    ///
    /// - 取消（`req.cancel`）必须尽快返回错误，不得继续等待或继续发请求；
    /// - 只有拼装**完整**的工具调用才会通过 `sink.on_tool_call` 回调，也会出现在返回值里；
    /// - 网络/协议错误映射为 [`ProviderError`]（保留是否可重试），对外只暴露 [`HbotError`]。
    async fn complete(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<Completion, HbotError>;
}

/// provider 错误：领域错误 + 是否可重试。
///
/// 为什么不用 `HbotError` 直接表达：`HbotError` 的六个变体是 RPC 契约
/// （`-32001..-32006`），"可重试"是传输层信息，塞进领域错误会污染契约。
/// 对外的 trait 方法仍然只返回 `HbotError`（调用方需要重试信息时用
/// [`ProviderError::from_error`] 在网络层内部处理）。
#[derive(Debug)]
pub struct ProviderError {
    /// 领域错误。
    pub error: HbotError,
    /// 是否值得重试（429 / 5xx / 超时）。
    pub retryable: bool,
}

impl ProviderError {
    /// 不可重试错误（4xx、协议错误）。
    pub fn fatal(error: HbotError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }

    /// 可重试错误（限流、服务端错误、超时）。
    pub fn retryable(error: HbotError) -> Self {
        Self {
            error,
            retryable: true,
        }
    }

    /// 按 HTTP 状态码映射：
    ///
    /// - 4xx（408/429 除外）→ [`HbotError::Invalid`]，不可重试；
    /// - 408 / 429 / 5xx → 可重试（`Internal`），由上层退避；
    /// - 其它 → 不可重试。
    pub fn from_status(status: u16, body: &str) -> Self {
        let body = truncate_for_message(body, 300);
        match status {
            400 | 401 | 403 | 404 | 409 | 422 | 402 => Self::fatal(HbotError::invalid(format!(
                "provider 返回 HTTP {status}: {body}"
            ))),
            408 | 429 => Self::retryable(HbotError::internal(format!(
                "provider 暂时不可用（HTTP {status}）：{body}"
            ))),
            500..=599 => Self::retryable(HbotError::internal(format!(
                "provider 服务端错误（HTTP {status}）：{body}"
            ))),
            _ => Self::fatal(HbotError::internal(format!(
                "provider 返回未预期的 HTTP {status}: {body}"
            ))),
        }
    }

    /// 超时（可重试）。
    pub fn timeout(operation: &str, timeout_ms: u64) -> Self {
        Self::retryable(HbotError::timeout(operation, timeout_ms))
    }

    /// 取消（不可重试）。
    pub fn cancelled() -> Self {
        Self::fatal(crate::cancel::cancelled_error())
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（可重试={}）", self.error, self.retryable)
    }
}

impl std::error::Error for ProviderError {}

impl From<ProviderError> for HbotError {
    fn from(value: ProviderError) -> Self {
        value.error
    }
}

/// 错误信息里截断响应体，避免把整页 HTML 塞进日志。
fn truncate_for_message(body: &str, max: usize) -> String {
    let text = body.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_adds_saturating() {
        let mut total = Usage::default();
        total.add(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        });
        total.add(Usage {
            prompt_tokens: u64::MAX,
            completion_tokens: 0,
            total_tokens: 0,
        });
        assert_eq!(total.prompt_tokens, u64::MAX);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(total.total_tokens, 15);
    }

    #[test]
    fn status_mapping_marks_retryable() {
        assert!(!ProviderError::from_status(400, "bad").retryable);
        assert!(!ProviderError::from_status(401, "no key").retryable);
        assert!(ProviderError::from_status(429, "slow down").retryable);
        assert!(ProviderError::from_status(503, "down").retryable);
        assert!(matches!(
            ProviderError::from_status(400, "bad").error,
            HbotError::Invalid { .. }
        ));
    }

    #[test]
    fn finish_reason_parses_wire_values() {
        assert_eq!(FinishReason::from_wire("stop"), FinishReason::Stop);
        assert_eq!(FinishReason::from_wire("length"), FinishReason::Length);
        assert_eq!(
            FinishReason::from_wire("tool_calls"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_wire("weird"),
            FinishReason::Other("weird".to_string())
        );
    }

    #[test]
    fn body_truncation_is_char_safe() {
        let error = ProviderError::from_status(500, &"错".repeat(1000));
        assert!(matches!(error.error, HbotError::Internal { .. }));
        assert!(error.retryable, "5xx 必须标记为可重试");
        let message = error.error.to_string();
        assert!(message.contains('…'), "超长响应体必须被截断");
        // 按字节截断会把中文切成半个字符，出现替换符。
        assert!(!message.contains('\u{fffd}'), "截断必须按字符边界");
    }
}
