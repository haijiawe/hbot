//! 测试替身（仅 `cfg(test)` 编译）。
//!
//! 刻意放在库里而不是各测试模块内：fake provider / 记录型审批器 / 审计器要在
//! `gateway`、`r#loop`、`tools` 三处共用，复制三份必然漂移。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{HbotError, SandboxMode, SessionId};
use hbot_workspace::Workspace;

use crate::approvals::{ApprovalReply, ApprovalRequest, Approver};
use crate::cancel::CancellationToken;
use crate::gateway::{AuditEvent, AuditSink};
use crate::provider::{
    Completion, CompletionRequest, FinishReason, Provider, StreamSink, ToolCallRequest, Usage,
};
use crate::tools::{Risk, Tool, ToolCtx, ToolOutput};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 临时工作区目录，`Drop` 时删除。
pub struct TestDir {
    root: PathBuf,
}

impl TestDir {
    /// 建一个唯一临时目录。
    pub fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hbot-agent-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        Self { root }
    }

    /// 根路径。
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// 打开工作区（失败直接 panic，测试里可以）。
    pub fn workspace(&self) -> Arc<Workspace> {
        Arc::new(Workspace::open(self.root.clone()).expect("open test workspace"))
    }

    /// 写一个文件（自动建父目录）。
    pub fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write test file");
    }

    /// 读一个文件。
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).expect("read test file")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.root) {
            tracing::warn!(root = %self.root.display(), %err, "测试目录清理失败");
        }
    }
}

/// 记录所有审批请求，并按脚本回复。
#[derive(Clone)]
pub struct RecordingApprover {
    requests: Arc<Mutex<Vec<ApprovalRequest>>>,
    reply: Arc<ApprovalReply>,
}

impl Default for RecordingApprover {
    fn default() -> Self {
        Self::approving_once()
    }
}

impl RecordingApprover {
    /// 每次都批准，但不记住。
    pub fn approving_once() -> Self {
        Self::with_reply(ApprovalReply::approve_once())
    }

    /// 批准并记住（只记住这一个完整键）。
    pub fn approving_always() -> Self {
        Self::with_reply(ApprovalReply::approve_always())
    }

    /// 一律拒绝。
    pub fn denying() -> Self {
        Self::with_reply(ApprovalReply::deny())
    }

    /// 自定义回复。
    pub fn with_reply(reply: ApprovalReply) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            reply: Arc::new(reply),
        }
    }

    /// 已收到的审批请求。
    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 已收到的审批请求数。
    pub fn count(&self) -> usize {
        self.requests.lock().map(|guard| guard.len()).unwrap_or(0)
    }
}

#[async_trait]
impl Approver for RecordingApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        if let Ok(mut guard) = self.requests.lock() {
            guard.push(request.clone());
        }
        Ok(*self.reply)
    }
}

/// 记录审计事件。
#[derive(Clone, Default)]
pub struct RecordingAudit {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl RecordingAudit {
    /// 新记录器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已记录事件。
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 事件数。
    pub fn len(&self) -> usize {
        self.events.lock().map(|guard| guard.len()).unwrap_or(0)
    }
}

impl AuditSink for RecordingAudit {
    fn record(&self, event: &AuditEvent) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event.clone());
        }
    }
}

/// 可配置的记录型工具。
pub struct RecordingTool {
    name: String,
    risk: Risk,
    needs_approval: bool,
    path_arg: Option<&'static str>,
    may_create: bool,
    failure: Option<String>,
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordingTool {
    /// 只读记录型工具。
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            risk: Risk::Read,
            needs_approval: false,
            path_arg: None,
            may_create: false,
            failure: None,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 设置风险类别。
    pub fn with_risk(mut self, risk: Risk) -> Self {
        self.risk = risk;
        self
    }

    /// 设置是否要审批。
    pub fn with_approval(mut self, needs: bool) -> Self {
        self.needs_approval = needs;
        self
    }

    /// 设置路径参数名。
    pub fn with_path_arg(mut self, arg: &'static str) -> Self {
        self.path_arg = Some(arg);
        self
    }

    /// 允许路径不存在（新建）。
    pub fn creating_paths(mut self) -> Self {
        self.may_create = true;
        self
    }

    /// 执行时返回错误。
    pub fn fail_with(mut self, message: &str) -> Self {
        self.failure = Some(message.to_string());
        self
    }

    /// 收到过的参数。
    pub fn calls(&self) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "记录型测试工具"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["path"]
        })
    }

    fn risk(&self) -> Risk {
        self.risk
    }

    fn path_arg(&self) -> Option<&'static str> {
        self.path_arg
    }

    fn may_create_path(&self) -> bool {
        self.may_create
    }

    fn requires_approval(&self, _sandbox: SandboxMode, _args: &serde_json::Value) -> bool {
        self.needs_approval
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    async fn run(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, HbotError> {
        if let Ok(mut guard) = self.calls.lock() {
            guard.push(args.clone());
        }
        if let Some(message) = &self.failure {
            return Err(HbotError::internal(message.clone()));
        }
        Ok(ToolOutput::text(format!("ran {}", self.name)))
    }
}

/// 按脚本返回 `Completion` 的假 provider。
pub struct FakeProvider {
    script: Mutex<VecDeque<Completion>>,
    requests: Mutex<Vec<CompletionRequest>>,
    canceller: Mutex<Option<(usize, CancellationToken)>>,
}

impl FakeProvider {
    /// 用一串回合脚本构造。
    pub fn new(script: Vec<Completion>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            canceller: Mutex::new(None),
        }
    }

    /// 在第 `after_calls` 次请求返回后触发取消（用来测"取消后不再请求"）。
    pub fn cancelling_after(self, after_calls: usize, cancel: CancellationToken) -> Self {
        if let Ok(mut guard) = self.canceller.lock() {
            *guard = Some((after_calls, cancel));
        }
        self
    }

    /// 已收到的请求。
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 已收到的请求数。
    pub fn request_count(&self) -> usize {
        self.requests.lock().map(|guard| guard.len()).unwrap_or(0)
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn complete(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<Completion, HbotError> {
        let call_index = {
            let mut guard = self
                .requests
                .lock()
                .map_err(|_| HbotError::internal("测试锁中毒"))?;
            guard.push(req);
            guard.len()
        };
        let completion = {
            let mut guard = self
                .script
                .lock()
                .map_err(|_| HbotError::internal("测试锁中毒"))?;
            guard.pop_front().unwrap_or_else(|| Completion {
                content: Some("脚本已耗尽".to_string()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        };
        if let Some((after_calls, cancel)) =
            self.canceller.lock().ok().and_then(|guard| guard.clone())
            && call_index >= after_calls
        {
            cancel.cancel();
        }
        for delta in completion.text().split_inclusive('\n') {
            sink.on_text_delta(delta);
        }
        sink.on_usage(completion.usage);
        Ok(completion)
    }
}

/// 收集流式回调的 sink。
#[derive(Default)]
pub struct RecordingSink {
    /// 文本增量。
    pub deltas: Vec<String>,
    /// 工具调用。
    pub tool_calls: Vec<ToolCallRequest>,
    /// usage。
    pub usage: Vec<Usage>,
    /// 循环步号。
    pub steps: Vec<u32>,
    /// 工具结果回填。
    pub tool_results: Vec<String>,
}

impl StreamSink for RecordingSink {
    fn on_text_delta(&mut self, delta: &str) {
        self.deltas.push(delta.to_string());
    }

    fn on_tool_call(&mut self, call: &ToolCallRequest) {
        self.tool_calls.push(call.clone());
    }

    fn on_usage(&mut self, usage: Usage) {
        self.usage.push(usage);
    }
}

impl crate::r#loop::TurnSink for RecordingSink {
    fn on_step(&mut self, step: u32) {
        self.steps.push(step);
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.deltas.push(delta.to_string());
    }

    fn on_tool_call(&mut self, call: &ToolCallRequest) {
        self.tool_calls.push(call.clone());
    }

    fn on_tool_result(&mut self, call_id: &str, output: &ToolOutput) {
        self.tool_results
            .push(format!("{call_id}:{}", output.content));
    }
}

/// 便捷构造：带一个工具调用的回合。
pub fn tool_call_completion(id: &str, name: &str, arguments: serde_json::Value) -> Completion {
    Completion {
        content: None,
        tool_calls: vec![ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
        finish_reason: FinishReason::ToolCalls,
    }
}

/// 便捷构造：纯文本结束回合。
pub fn text_completion(text: &str) -> Completion {
    Completion {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
        finish_reason: FinishReason::Stop,
    }
}

/// 便捷构造：会话 id（全零 UUID，测试里不关心具体值）。
pub fn session_id() -> SessionId {
    "00000000-0000-0000-0000-000000000000"
        .parse()
        .expect("nil session id")
}
