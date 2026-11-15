//! 唯一的 Agent 循环。
//!
//! 一步 = 组装上下文 → 调 provider → 若有 `tool_calls` 则逐个过
//! [`ToolGateway`] 执行 → 回填结果 → 继续；没有 `tool_calls` 即结束。
//!
//! 三条硬约束：
//!
//! - **取消贯穿**：每步开始、provider 返回后、每个工具调用前都检查取消；
//!   取消后不再发起任何新请求（provider / 工具）。
//! - **预算硬停**：步数（默认 40）、总 token、总时长任一超限立即返回明确错误，
//!   不静默续命、不"再试一步看看"。
//! - **工具失败不炸循环**：工具错误作为 `tool` 消息回填，让模型自己纠正；
//!   只有取消与预算超限才中止整个 turn。

use std::time::{Duration, Instant};

use hbot_core::{HbotError, SessionId};
use serde_json::json;

use crate::approvals::Approver;
use crate::cancel::{CancellationToken, cancelled_error};
use crate::gateway::ToolGateway;
use crate::provider::{
    ChatMessage, CompletionRequest, FinishReason, Provider, StreamSink, ToolCallRequest, Usage,
};
use crate::tools::ToolOutput;

/// 默认步数上限。
pub const DEFAULT_MAX_STEPS: u32 = 40;

/// 默认总时长上限（30 分钟）。
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(30 * 60);

/// 一轮 turn 的预算与采样参数。
#[derive(Clone, Debug)]
pub struct TurnConfig {
    /// 模型名。
    pub model: String,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 单次回复的 token 上限。
    pub max_tokens: Option<u32>,
    /// 最多几步（一次 provider 调用算一步）。
    pub max_steps: u32,
    /// 整轮累计 token 上限（`None` 不限）。
    pub max_total_tokens: Option<u64>,
    /// 整轮时长上限（`None` 不限）。
    pub max_duration: Option<Duration>,
}

impl TurnConfig {
    /// 按默认预算构造。
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            temperature: None,
            max_tokens: None,
            max_steps: DEFAULT_MAX_STEPS,
            max_total_tokens: None,
            max_duration: Some(DEFAULT_MAX_DURATION),
        }
    }

    /// 设置温度。
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// 设置单次 token 上限。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// 设置步数上限。
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// 设置累计 token 上限。
    pub fn with_max_total_tokens(mut self, max_total_tokens: u64) -> Self {
        self.max_total_tokens = Some(max_total_tokens);
        self
    }

    /// 设置时长上限。
    pub fn with_max_duration(mut self, max_duration: Duration) -> Self {
        self.max_duration = Some(max_duration);
        self
    }
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self::new("default")
    }
}

/// 一轮 turn 的输入。
#[derive(Clone, Debug)]
pub struct TurnInput {
    /// 会话 id（审计/工具上下文用）。
    pub session_id: SessionId,
    /// system prompt（应包含不可信数据区块的声明，见 [`crate::prompt`]）。
    pub system_prompt: String,
    /// 已有消息（用户输入、历史）。
    pub messages: Vec<ChatMessage>,
}

/// 一轮 turn 的结果。
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// 实际步数。
    pub steps: u32,
    /// 最终文本。
    pub text: String,
    /// 累计 usage。
    pub usage: Usage,
    /// 结束原因。
    pub finish_reason: FinishReason,
    /// 完整消息序列（含 system、工具回填），可直接落库/续聊。
    pub messages: Vec<ChatMessage>,
}

/// 循环对外的事件出口（UI 流式显示、审计回调）。
pub trait TurnSink: Send {
    /// 第 `step` 步开始（从 0 计）。
    fn on_step(&mut self, _step: u32) {}
    /// 文本增量。
    fn on_text_delta(&mut self, _delta: &str) {}
    /// provider 声明了一个工具调用。
    fn on_tool_call(&mut self, _call: &ToolCallRequest) {}
    /// 工具结果回填。
    fn on_tool_result(&mut self, _call_id: &str, _output: &ToolOutput) {}
}

/// 丢弃所有事件的 sink。
#[derive(Clone, Copy, Debug, Default)]
pub struct NullTurnSink;

impl TurnSink for NullTurnSink {}

/// 把 provider 流式回调桥接到 [`TurnSink`]。
struct SinkBridge<'a> {
    sink: &'a mut dyn TurnSink,
}

impl StreamSink for SinkBridge<'_> {
    fn on_text_delta(&mut self, delta: &str) {
        self.sink.on_text_delta(delta);
    }

    fn on_tool_call(&mut self, call: &ToolCallRequest) {
        self.sink.on_tool_call(call);
    }

    fn on_usage(&mut self, _usage: Usage) {}
}

/// 跑一轮 turn。
pub async fn run_turn(
    provider: &dyn Provider,
    gateway: &ToolGateway,
    input: TurnInput,
    config: &TurnConfig,
    sink: &mut dyn TurnSink,
    cancel: &CancellationToken,
    approver: &dyn Approver,
) -> Result<TurnOutcome, HbotError> {
    let started = Instant::now();
    let mut messages = Vec::with_capacity(input.messages.len() + 8);
    messages.push(ChatMessage::system(input.system_prompt));
    messages.extend(input.messages);
    let tools = gateway.tool_specs();
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut steps = 0u32;

    loop {
        // 每次请求前都检查：取消后不再发起新请求。
        cancel.check()?;
        if steps >= config.max_steps {
            return Err(HbotError::invalid(format!(
                "已达步数上限 {}（max_steps），停止本轮；请缩小任务或提高上限",
                config.max_steps
            )));
        }
        if let Some(max_duration) = config.max_duration
            && started.elapsed() >= max_duration
        {
            return Err(HbotError::timeout(
                "agent turn",
                u64::try_from(max_duration.as_millis()).unwrap_or(u64::MAX),
            ));
        }
        sink.on_step(steps);
        let request = CompletionRequest {
            model: config.model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            cancel: cancel.clone(),
        };
        let completion = {
            let mut bridge = SinkBridge { sink: &mut *sink };
            provider
                .complete(request, &mut bridge)
                .await
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        cancelled_error()
                    } else {
                        error
                    }
                })?
        };
        steps += 1;

        // provider 刚返回就检查：取消后不执行任何工具、不再发下一次请求。
        cancel.check()?;
        usage.add(completion.usage);
        if let Some(max_total_tokens) = config.max_total_tokens
            && usage.total_tokens > max_total_tokens
        {
            return Err(HbotError::invalid(format!(
                "本轮累计 token {} 超过上限 {max_total_tokens}，停止；不要把剩余工作塞进下一轮",
                usage.total_tokens
            )));
        }

        if let Some(content) = &completion.content
            && !content.is_empty()
        {
            text.push_str(content);
        }
        messages.push(ChatMessage::Assistant {
            content: completion.content.clone(),
            tool_calls: completion.tool_calls.clone(),
        });

        if completion.tool_calls.is_empty() {
            return Ok(TurnOutcome {
                steps,
                text,
                usage,
                finish_reason: completion.finish_reason.clone(),
                messages,
            });
        }

        for tool_call in &completion.tool_calls {
            cancel.check()?;
            let result = gateway
                .call(input.session_id, tool_call.clone(), cancel, approver)
                .await;
            match result {
                Ok(outcome) => {
                    sink.on_tool_result(&tool_call.id, &outcome.output);
                    messages.push(ChatMessage::tool_result(
                        &tool_call.id,
                        outcome.output.content,
                    ));
                }
                Err(error) => {
                    // 取消：立即中止，不把"已取消"当成工具结果喂给模型。
                    if cancel.is_cancelled() {
                        return Err(cancelled_error());
                    }
                    let payload = json!({
                        "error": error.error.to_string(),
                        "step": error.failed_at().map(|step| step.as_str()),
                        "tool": error.tool_name,
                    });
                    let content = serde_json::to_string(&payload)
                        .unwrap_or_else(|_| format!("{{\"error\":\"{error}\"}}"));
                    let output = ToolOutput::error(content);
                    sink.on_tool_result(&tool_call.id, &output);
                    messages.push(ChatMessage::tool_result(&tool_call.id, output.content));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hbot_core::{HbotError, SandboxMode};
    use serde_json::json;

    use super::*;
    use crate::approvals::{ApprovalStore, DenyAllApprover};
    use crate::gateway::ToolGateway;
    use crate::test_support::{
        FakeProvider, RecordingApprover, RecordingAudit, RecordingTool, TestDir, session_id,
        text_completion, tool_call_completion,
    };
    use crate::tools::{Tool, ToolRegistry};

    /// 测试脚手架：真实网关（临时目录）+ 记录型审计。
    struct Harness {
        gateway: ToolGateway,
        audit: RecordingAudit,
        dir: TestDir,
    }

    impl Harness {
        fn new(level: SandboxMode, tool: Arc<RecordingTool>) -> Self {
            let dir = TestDir::new("loop");
            dir.write("a.txt", "hello");
            let mut registry = ToolRegistry::new();
            registry.register(tool as Arc<dyn Tool>);
            let audit = RecordingAudit::new();
            let gateway = ToolGateway::new(
                Arc::new(registry),
                dir.workspace(),
                level,
                Arc::new(ApprovalStore::new()),
                Arc::new(audit.clone()),
            );
            Self {
                gateway,
                audit,
                dir,
            }
        }
    }

    fn input() -> TurnInput {
        TurnInput {
            session_id: session_id(),
            system_prompt: "你是 hbot 的 Agent。".to_string(),
            messages: vec![ChatMessage::user("读 a.txt")],
        }
    }

    #[tokio::test]
    async fn tool_call_is_executed_once_and_result_is_backfilled() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let provider = FakeProvider::new(vec![
            tool_call_completion("call_1", "read_file", json!({ "path": "a.txt" })),
            text_completion("读完了"),
        ]);
        let mut sink = crate::test_support::RecordingSink::default();
        let cancel = CancellationToken::new();
        let approver = DenyAllApprover;

        let outcome = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &TurnConfig::new("fake-model"),
            &mut sink,
            &cancel,
            &approver,
        )
        .await
        .expect("第一轮应正常结束");

        // 工具只被调用一次，且拿到的是原始参数。
        assert_eq!(tool.calls().len(), 1, "工具必须只被调用一次");
        assert_eq!(tool.calls()[0]["path"], json!("a.txt"));
        // 两步：先 tool_calls 回合，再收尾回合。
        assert_eq!(outcome.steps, 2);
        assert_eq!(provider.request_count(), 2);
        assert_eq!(outcome.text, "读完了");

        // 第二次请求必须已经带上工具结果（回填）。
        let second = &provider.requests()[1].messages;
        let backfilled = second.iter().any(|message| {
            matches!(message, ChatMessage::Tool { tool_call_id, content }
                if tool_call_id == "call_1" && content == "ran read_file")
        });
        assert!(backfilled, "工具结果必须回填进第二次请求");

        // 结果消息顺序：system → user → assistant(tool_calls) → tool。
        assert!(matches!(
            outcome.messages.first(),
            Some(ChatMessage::System { .. })
        ));
        assert!(matches!(
            outcome.messages.get(2),
            Some(ChatMessage::Assistant { tool_calls, .. }) if tool_calls.len() == 1
        ));
        assert!(matches!(
            outcome.messages.get(3),
            Some(ChatMessage::Tool { tool_call_id, .. }) if tool_call_id == "call_1"
        ));
        // 审计有一条成功记录。
        assert_eq!(harness.audit.len(), 1);
        let _ = &harness.dir;
    }

    #[tokio::test]
    async fn max_steps_stops_with_explicit_error() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let step = || tool_call_completion("call_x", "read_file", json!({ "path": "a.txt" }));
        let provider = FakeProvider::new(vec![step(), step(), step(), step()]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();
        let config = TurnConfig::new("fake-model").with_max_steps(2);

        let error = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &config,
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect_err("必须超限报错而不是静默续跑");

        assert!(matches!(error, HbotError::Invalid { .. }));
        assert!(
            error.to_string().contains("步数上限"),
            "错误要说清是步数上限"
        );
        assert_eq!(tool.calls().len(), 2, "上限 2 步就只执行 2 次");
        assert_eq!(provider.request_count(), 2);
    }

    #[tokio::test]
    async fn cancellation_stops_before_next_request() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let cancel = CancellationToken::new();
        let provider = FakeProvider::new(vec![
            text_completion("第一个回合"),
            text_completion("不该出现"),
        ])
        .cancelling_after(1, cancel.clone());
        let mut sink = NullTurnSink;

        let error = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &TurnConfig::new("fake-model"),
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect_err("取消后必须中止");

        assert!(
            matches!(error, HbotError::Denied { .. }),
            "取消映射为 Denied"
        );
        assert_eq!(
            provider.request_count(),
            1,
            "取消后不得再发新的 provider 请求"
        );
    }

    #[tokio::test]
    async fn cancellation_before_any_request_keeps_provider_untouched() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let provider = FakeProvider::new(vec![text_completion("不该出现")]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &TurnConfig::new("fake-model"),
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect_err("已取消的 turn 不得开始");

        assert!(matches!(error, HbotError::Denied { .. }));
        assert_eq!(provider.request_count(), 0);
    }

    #[tokio::test]
    async fn token_budget_stops_turn() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        // text_completion 的 usage 是 10/5/15。
        let provider = FakeProvider::new(vec![text_completion("一"), text_completion("二")]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();
        let config = TurnConfig::new("fake-model").with_max_total_tokens(10);

        let error = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &config,
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect_err("token 超限必须报错");

        assert!(matches!(error, HbotError::Invalid { .. }));
        assert!(
            error.to_string().contains("token"),
            "错误要说清是 token 上限"
        );
        assert_eq!(provider.request_count(), 1, "超限后不得再请求");
    }

    #[tokio::test]
    async fn duration_budget_stops_before_request() {
        let tool = Arc::new(RecordingTool::new("read_file").with_path_arg("path"));
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let provider = FakeProvider::new(vec![text_completion("不该出现")]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();
        let config = TurnConfig::new("fake-model").with_max_duration(Duration::ZERO);

        let error = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &config,
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect_err("时长超限必须报错");

        assert!(matches!(error, HbotError::Timeout { .. }));
        assert_eq!(provider.request_count(), 0, "时长超限时不该再发请求");
    }

    #[tokio::test]
    async fn tool_failure_is_backfilled_and_turn_continues() {
        let tool = Arc::new(
            RecordingTool::new("read_file")
                .with_path_arg("path")
                .fail_with("炸了"),
        );
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let provider = FakeProvider::new(vec![
            tool_call_completion("call_1", "read_file", json!({ "path": "a.txt" })),
            text_completion("我换个办法"),
        ]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();

        let outcome = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &TurnConfig::new("fake-model"),
            &mut sink,
            &cancel,
            &DenyAllApprover,
        )
        .await
        .expect("工具失败不该炸掉整个 turn");

        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.text, "我换个办法");
        // 失败以 error 结构回填，且带上是哪一步挂的。
        let tool_message = outcome
            .messages
            .iter()
            .find_map(|message| match message {
                ChatMessage::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("必须有工具回填消息");
        assert!(tool_message.contains("\"error\""));
        assert!(tool_message.contains("\"execute\""), "要标注失败闸门");
        // 审计记录的是失败。
        assert!(matches!(
            harness.audit.events()[0].outcome,
            crate::gateway::AuditOutcome::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn write_requires_approval_and_denial_is_backfilled() {
        let tool = Arc::new(
            RecordingTool::new("write_file")
                .with_risk(crate::tools::Risk::Write)
                .with_approval(true)
                .with_path_arg("path"),
        );
        let harness = Harness::new(SandboxMode::WorkspaceWrite, Arc::clone(&tool));
        let provider = FakeProvider::new(vec![
            tool_call_completion(
                "call_1",
                "write_file",
                json!({ "path": "a.txt", "text": "x" }),
            ),
            text_completion("那我先问清楚"),
        ]);
        let mut sink = NullTurnSink;
        let cancel = CancellationToken::new();
        let approver = RecordingApprover::denying();

        let outcome = run_turn(
            &provider,
            &harness.gateway,
            input(),
            &TurnConfig::new("fake-model"),
            &mut sink,
            &cancel,
            &approver,
        )
        .await
        .expect("拒绝审批不该炸 turn");

        assert_eq!(approver.count(), 1, "写工具必须弹一次审批");
        assert!(tool.calls().is_empty(), "被拒绝的工具不得执行");
        let backfilled = outcome.messages.iter().any(|message| {
            matches!(message, ChatMessage::Tool { content, .. }
                if content.contains("\"approval\""))
        });
        assert!(backfilled, "拒绝原因要回填给模型");
    }
}
