//! Agent 装配层：把 [`hbot_agent`] 的单循环接到 RPC 上。
//!
//! ## 这一层只做三件事
//!
//! 1. **配置**：读环境变量 / `agent.json`（[`config`]），缺配置就报 `-32002`；
//! 2. **会话**：内存里的历史、取消令牌、忙闲（[`session`]）；
//! 3. **接线**：每个 `agent_send` 起一个 tokio 任务跑 [`hbot_agent::run_turn`]，
//!    把流式增量、工具调用、步进翻成 [`Event`]，并把工具调用交给
//!    [`ToolGateway`]（**唯一**授权路径，不在本层再判一次）。
//!
//! ## 为什么审批不在这一层判
//!
//! `docs/00-assessment.md` P0-7 与 `AGENTS.md` §0.6 都要求"审批判据只有一份"。
//! 网关负责：沙箱档 → 路径 realpath → 审批规则 → 执行 → 审计；本层只提供
//! [`Approver`] 的传输实现（[`approvals::SessionApprover`]）与
//! [`EditProposer`] 的登记实现（[`AgentManager::propose`]）。
//!
//! ## 写入为什么不落盘
//!
//! Agent 的 `write_file` / `edit_file` 在网关里已经去掉"直接写"的通道（装配了
//! [`EditProposer`] 时），本层把提案登记进 [`EditRegistry`] 并推 `agent.diff`；
//! 真正落盘只有在 `edit_apply` 时、带**提案时的基线哈希**发生一次（`-32003` 即
//! "文件已被别人改过"）。

pub mod approvals;
pub mod config;
pub mod edits;
pub mod log;
pub mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hbot_agent::provider::{AnthropicProvider, LlmProvider};
use hbot_agent::{
    ApprovalStore, AuditEvent, AuditSink, ChatMessage, EditProposer, MAX_TOOL_OUTPUT_BYTES,
    OpenAiProvider, PromptContext, ProposedEdit, Provider, ReqwestTransport, SandboxMode,
    ToolCallRequest, ToolGateway, ToolOutput, ToolRegistry, TurnConfig, TurnInput, TurnSink,
    run_turn, system_prompt,
};
use hbot_core::{Clock, EditId, HbotError, IdGenerator, SessionId, ToolCall, ToolCallStatus};
use hbot_workspace::Workspace;

use crate::app::EventBus;
use crate::dto::{DiffHunk, EditFile, Event};

pub use approvals::ApprovalInbox;
pub use config::{CONFIG_FILE, MISSING_CONFIG_MESSAGE, ProviderConfig, SandboxConfig};
pub use edits::{EditRegistry, PendingEdit};
pub use log::{NullRunLogSink, RunLogSink, StoreRunLogSink};
pub use session::{SessionRegistry, SessionState};

/// 项目规则文件（拼进 system prompt 的项目内约定）。
pub const PROJECT_RULES_FILE: &str = "AGENTS.md";

/// 项目规则注入上限（字符）：避免把整本手册塞进每一轮请求。
pub const MAX_PROJECT_RULES_CHARS: usize = 32 * 1024;

/// 工具结果进 `agent.tool` 事件时的正文上限。
const MAX_TOOL_RESULT_CHARS: usize = 2000;

/// 建 provider 的工厂。生产装配用 [`default_provider_factory`]，测试注入假 provider。
pub type ProviderFactory =
    dyn Fn(&ProviderConfig) -> Result<Box<dyn Provider>, HbotError> + Send + Sync;

/// 默认工厂：真实 `reqwest` 传输 + 按 [`ProviderConfig::provider`] 选的 SSE 客户端。
///
/// **不跨 provider 重试**：选了哪条就走哪条，失败如实上报（`AGENTS.md` §0.8 —— 悄悄
/// 换一个上游会让"已发送"变成谎话，也让排障时看到的是另一个端点的错误）。
pub fn default_provider_factory(config: &ProviderConfig) -> Result<Box<dyn Provider>, HbotError> {
    let transport = ReqwestTransport::new().map_err(HbotError::from)?;
    match config.provider() {
        LlmProvider::OpenAi => Ok(Box::new(OpenAiProvider::new(
            config.base_url(),
            config.api_key(),
            transport,
        ))),
        LlmProvider::Anthropic => Ok(Box::new(AnthropicProvider::new(
            config.base_url(),
            config.api_key(),
            transport,
        ))),
    }
}

/// 配置来源（测试可固定为某个配置或缺配置）。
///
/// `Fixed` / `Fail` 只有测试装配会构造——生产永远走 `Resolve`。保留在同一个枚举里
/// （而不是 `cfg(test)` 分支）是为了让 `resolve_config` 只有一条实现路径。
#[derive(Clone)]
#[allow(dead_code)]
enum ConfigSource {
    /// 环境变量 → `<data_dir>/agent.json`。
    Resolve,
    /// 固定配置（测试用：不碰进程环境）。
    Fixed(ProviderConfig),
    /// 固定失败（测试用：验证 `-32002` 直通到命令层）。
    Fail(HbotError),
}

/// Agent 装配层：会话表、待审编辑、审批通道、provider 工厂。
pub struct AgentManager {
    data_dir: PathBuf,
    events: Arc<EventBus>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    sessions: SessionRegistry,
    edits: EditRegistry,
    approvals: Arc<ApprovalInbox>,
    approval_store: Arc<ApprovalStore>,
    config_source: ConfigSource,
    provider_factory: Box<ProviderFactory>,
    /// Agent 运行日志的落点（`logs` 表）。由装配方显式注入：Agent 会话没有 bot
    /// 归属，生产装配用 [`StoreRunLogSink`]（`bot_id = NULL`、`source = 'agent'`）。
    run_log: Arc<dyn RunLogSink>,
}

impl AgentManager {
    /// 生产装配。`run_log` 由 `AppState::open` 注入。
    pub fn new(
        data_dir: PathBuf,
        events: Arc<EventBus>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        run_log: Arc<dyn RunLogSink>,
    ) -> Self {
        Self {
            data_dir,
            events,
            ids,
            clock,
            sessions: SessionRegistry::new(),
            edits: EditRegistry::new(),
            approvals: Arc::new(ApprovalInbox::new()),
            approval_store: Arc::new(ApprovalStore::new()),
            config_source: ConfigSource::Resolve,
            provider_factory: Box::new(default_provider_factory),
            run_log,
        }
    }

    /// 换一个 `RunLog` 落点（测试/诊断用；生产装配由 [`Self::new`] 注入）。
    pub fn with_run_log_sink(mut self, sink: Arc<dyn RunLogSink>) -> Self {
        self.run_log = sink;
        self
    }

    /// 当前 `RunLog` 落点（测试与诊断用）。
    pub fn run_log_sink(&self) -> &Arc<dyn RunLogSink> {
        &self.run_log
    }

    /// 测试装配：固定配置 + 假 provider + 记录审批的通道。
    #[cfg(test)]
    pub fn for_test(
        data_dir: PathBuf,
        events: Arc<EventBus>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        config: Result<ProviderConfig, HbotError>,
        provider_factory: Box<ProviderFactory>,
    ) -> Self {
        let config_source = match config {
            Ok(config) => ConfigSource::Fixed(config),
            Err(error) => ConfigSource::Fail(error),
        };
        Self {
            config_source,
            provider_factory,
            ..Self::new(data_dir, events, ids, clock, Arc::new(NullRunLogSink))
        }
    }

    /// 事件出口（测试订阅用）。
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// 解析本轮 provider 配置（同步读文件：调用方在 async 上下文里走
    /// [`crate::app::AppState::blocking`]）。
    pub fn resolve_config(&self) -> Result<ProviderConfig, HbotError> {
        match &self.config_source {
            ConfigSource::Resolve => ProviderConfig::resolve(&self.data_dir),
            ConfigSource::Fixed(config) => Ok(config.clone()),
            ConfigSource::Fail(error) => Err(error.clone()),
        }
    }

    /// 解析沙箱档与命令白名单（`HBOT_SANDBOX` / `HBOT_COMMAND_ALLOW` / `agent.json`）。
    ///
    /// 与 [`Self::resolve_config`] 分开：模型配置缺失不该让"只读档"也解析失败，
    /// 反过来沙箱档写错也不该被说成"未配置模型"。两者都是同步读盘，调用方在
    /// `async fn` 里要走 [`crate::app::AppState::blocking`]。
    pub fn resolve_sandbox_config(&self) -> Result<SandboxConfig, HbotError> {
        SandboxConfig::resolve(&self.data_dir)
    }

    /// 取会话（没有就建）。
    pub fn session(&self, session_id: SessionId) -> Arc<SessionState> {
        self.sessions.get_or_create(session_id)
    }

    /// 会话是否正在跑一轮（测试与诊断用）。
    pub fn is_busy(&self, session_id: SessionId) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|session| session.is_busy())
    }

    /// 某会话的历史条数（测试与诊断用）。
    pub fn history_len(&self, session_id: SessionId) -> usize {
        self.sessions
            .get(session_id)
            .map_or(0, |session| session.history_len())
    }

    /// 取消某一轮。
    ///
    /// 三件事一起做，缺一不可：
    /// 1. 置位取消令牌（provider / 工具 / HTTP 都看它）；
    /// 2. 把该会话所有等待中的审批按拒绝答复（否则回合卡在审批上）；
    /// 3. 什么都不做时也返回 `Ok`（取消是幂等的，UI 不该收到错误）。
    pub fn cancel(&self, session_id: SessionId) -> bool {
        let Some(session) = self.sessions.get(session_id) else {
            return false;
        };
        let cancelled = session.cancel_turn();
        self.approvals.deny_all(session_id);
        cancelled
    }

    /// 取消所有会话正在跑的回合，并关闭它们的审批通道（换项目时调用）。
    pub fn cancel_all(&self) -> usize {
        let cancelled = self.sessions.cancel_all();
        for session_id in self.sessions.ids() {
            self.approvals.deny_all(session_id);
        }
        cancelled
    }

    /// 丢弃全部待审编辑（换项目时调用：旧项目的 Diff 不该写进新项目）。
    pub fn clear_edits(&self) {
        self.edits.clear();
    }

    /// 落一次审批答复。
    pub fn resolve_approval(
        &self,
        approval_id: hbot_core::ApprovalId,
        decision: hbot_core::ApprovalDecision,
        always: bool,
    ) -> Result<(), HbotError> {
        self.approvals.resolve(approval_id, decision, always)
    }

    /// 取一条待审编辑（不删除）。
    pub fn peek_edit(&self, edit_id: EditId) -> Result<PendingEdit, HbotError> {
        self.edits
            .get(edit_id)
            .ok_or_else(|| HbotError::not_found("edit", edit_id.to_string()))
    }

    /// 丢弃一条待审编辑。
    pub fn drop_edit(&self, edit_id: EditId) -> Result<(), HbotError> {
        self.edits
            .remove(edit_id)
            .map(|_| ())
            .ok_or_else(|| HbotError::not_found("edit", edit_id.to_string()))
    }

    /// 删除一条已经落盘的编辑（`edit_apply` 成功后调用）。
    pub fn forget_edit(&self, edit_id: EditId) {
        self.edits.remove(edit_id);
    }

    /// 待审编辑条数（测试与诊断用）。
    pub fn pending_edits(&self) -> usize {
        self.edits.len()
    }

    /// 起一轮：追加用户消息、装配网关、spawn 任务，**立刻返回**。
    ///
    /// 返回值只表示"已接受"，回合结果通过事件推给 UI。
    ///
    /// `project_rules` 是项目根目录 `AGENTS.md` 的正文（由调用方在
    /// [`AppState::blocking`] 里读好再传进来 —— 本函数在 async 路径上，
    /// 不能自己同步读盘）。读失败传 `None`，本轮就不注入项目规则。
    pub fn start_turn(
        self: &Arc<Self>,
        session_id: SessionId,
        text: String,
        config: ProviderConfig,
        sandbox: SandboxConfig,
        workspace: Arc<Workspace>,
        project_rules: Option<String>,
    ) -> Result<(), HbotError> {
        if text.trim().is_empty() {
            return Err(HbotError::invalid("agent_send: text 不能为空"));
        }

        let session = self.sessions.get_or_create(session_id);
        // 忙闲检查 + 追加用户消息是原子的；被拒时这里返回，会话状态未变。
        let (cancel, history) = session.begin_turn(&text)?;

        // 从这里开始，任何提前返回都要把会话复位（否则 busy 永久为真）。
        let provider = match (self.provider_factory)(&config) {
            Ok(provider) => provider,
            Err(error) => {
                session.end_turn(None);
                return Err(error);
            }
        };

        // 注册表带命令白名单：空清单 = 任何程序都拒（fail-closed）。清单只放具体程序，
        // 不放 `cmd`/`sh` 之类的解释器（`docs/adr/0003-command-isolation.md` §5）。
        let registry = Arc::new(ToolRegistry::default_tools_with_allowed_programs(
            sandbox.allowed_programs().iter().cloned(),
        ));
        let system_prompt = build_system_prompt(
            &registry,
            &workspace,
            sandbox.mode(),
            config.max_steps(),
            project_rules,
        );
        let gateway = ToolGateway::new(
            registry,
            Arc::clone(&workspace),
            sandbox.mode(),
            Arc::clone(&self.approval_store),
            Arc::new(TracingAudit),
        )
        .with_edit_proposer(Arc::clone(self) as Arc<dyn EditProposer>)
        .with_command_runner(Arc::new(hbot_command::PlatformRunner::system()));
        let approver = approvals::SessionApprover::new(
            Arc::clone(&self.approvals),
            Arc::clone(&self.ids),
            Arc::clone(&self.events),
            session_id,
            cancel.clone(),
        );
        let sink = EventTurnSink::new(
            Arc::clone(&self.events),
            session_id,
            Arc::clone(&self.clock),
            Arc::clone(&self.run_log),
        );
        let turn_config = TurnConfig::new(config.model()).with_max_steps(config.max_steps());
        let session_id_for_task = session_id;

        tokio::spawn(async move {
            run_turn_task(
                session,
                session_id_for_task,
                provider,
                gateway,
                approver,
                sink,
                history,
                system_prompt,
                turn_config,
                cancel,
            )
            .await;
        });

        Ok(())
    }

    /// 当前会话表的条数（诊断用）。
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl EditProposer for AgentManager {
    /// 登记待审编辑并推 `agent.diff`；**不写盘**。
    fn propose(
        &self,
        session_id: SessionId,
        call_id: &str,
        edit: ProposedEdit,
    ) -> Result<EditId, HbotError> {
        let edit_id = self.ids.edit_id();
        let path = edit.path.clone();
        let files = vec![EditFile {
            path,
            hunks: edit
                .hunks
                .iter()
                .map(|hunk| DiffHunk {
                    index: hunk.index,
                    old_start: hunk.old_start,
                    old_lines: hunk.old_lines,
                    new_start: hunk.new_start,
                    new_lines: hunk.new_lines,
                    lines: hunk.lines.clone(),
                })
                .collect(),
        }];

        // 先登记再推事件：UI 收到 `agent.diff` 后立刻 `edit_apply` 也能找到这条。
        self.edits
            .insert(edit_id, PendingEdit::from_proposal(session_id, edit));
        self.events.send(Event::AgentDiff { edit_id, files });
        tracing::debug!(
            session_id = %session_id,
            call_id,
            edit_id = %edit_id,
            "已登记待审编辑"
        );
        Ok(edit_id)
    }
}

/// 一轮任务的实体：跑循环、翻事件、收尾。
#[allow(clippy::too_many_arguments)]
async fn run_turn_task(
    session: Arc<SessionState>,
    session_id: SessionId,
    provider: Box<dyn Provider>,
    gateway: ToolGateway,
    approver: approvals::SessionApprover,
    mut sink: EventTurnSink,
    history: Vec<ChatMessage>,
    system_prompt: String,
    config: TurnConfig,
    cancel: hbot_agent::CancellationToken,
) {
    let guard = session::TurnGuard::new(&session);
    let input = TurnInput {
        session_id,
        system_prompt,
        messages: history.clone(),
    };

    let outcome = run_turn(
        provider.as_ref(),
        &gateway,
        input,
        &config,
        &mut sink,
        &cancel,
        &approver,
    )
    .await;

    match outcome {
        Ok(outcome) => {
            // 非流式 provider 一条增量都没发：把最终文本补成一条 delta，UI 才看得见。
            if !sink.emitted_text() && !outcome.text.is_empty() {
                sink.emit_delta(&outcome.text);
            }
            guard.finish(without_system(outcome.messages));
        }
        Err(error) => {
            let cancelled = cancel.is_cancelled();
            let note = if cancelled {
                "（本轮已取消）".to_string()
            } else {
                format!("（本轮失败：{error}）")
            };
            if cancelled {
                tracing::info!(session_id = %session_id, "agent turn cancelled");
                sink.log_cancel();
            } else {
                tracing::warn!(session_id = %session_id, error = %error, "agent turn failed");
                // 失败必须留痕：`logs_tail` 里一行，脱敏见 `log::turn_failure_line`。
                sink.log_failure(&error.to_string());
            }
            // 失败/取消也要让 UI 看见一句话，不能静默结束。
            sink.emit_delta(&note);
            // 历史以"本轮输入"为底（里面有刚追加的用户消息），再补上已经流出来的
            // 半截回答 —— 不能拿 sink 的文本反手覆盖整个会话历史，那会把用户消息
            // 和之前所有轮次一起丢掉。
            let mut kept = without_system(history);
            kept.extend(sink.take_messages());
            guard.finish(kept);
        }
    }
}

/// 去掉 system 消息（每轮由 `run_turn` 重新拼），避免历史里重复累积。
fn without_system(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .filter(|message| !matches!(message, ChatMessage::System { .. }))
        .collect()
}

/// 拼本轮 system prompt。
fn build_system_prompt(
    registry: &ToolRegistry,
    workspace: &Workspace,
    sandbox: SandboxMode,
    max_steps: u32,
    project_rules: Option<String>,
) -> String {
    system_prompt(&PromptContext {
        workspace_root: workspace.root().display().to_string(),
        sandbox,
        tools: registry.names(),
        max_steps,
        max_tool_output_chars: MAX_TOOL_OUTPUT_BYTES,
        project_rules,
    })
}

/// 读项目根的 `AGENTS.md`（有就注入，超长截断）。
///
/// 这是同步 IO：调用方必须在 [`AppState::blocking`] 里调用它，
/// 不要在 async 任务里直接读。
pub fn read_project_rules(workspace: &Workspace) -> Option<String> {
    let path = workspace.root().join(PROJECT_RULES_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_PROJECT_RULES_CHARS {
        return Some(trimmed.to_string());
    }
    let mut truncated: String = trimmed.chars().take(MAX_PROJECT_RULES_CHARS).collect();
    truncated.push_str("\n…（项目规则过长已截断）");
    Some(truncated)
}

/// 把 `run_turn` 的进度翻成 UI 事件。
pub struct EventTurnSink {
    events: Arc<EventBus>,
    session_id: SessionId,
    clock: Arc<dyn Clock>,
    /// 本轮已执行的工具调用（`on_tool_call` 记，`on_tool_result` 用）。
    calls: HashMap<String, ToolCallRequest>,
    /// 每次工具调用**开始**的时刻（`on_tool_result` 用它算耗时）。用 `Instant` 而不是
    /// `clock.now()`：耗时是进程内的单调读数，不该被墙上时钟跳变污染。
    started: HashMap<String, Instant>,
    /// `RunLog` 落点（一步一行，见 [`log`]）。
    run_log: Arc<dyn RunLogSink>,
    /// 本轮流式文本（失败时写回历史用）。
    text: String,
    /// 是否发过至少一条增量。
    emitted: bool,
}

impl EventTurnSink {
    fn new(
        events: Arc<EventBus>,
        session_id: SessionId,
        clock: Arc<dyn Clock>,
        run_log: Arc<dyn RunLogSink>,
    ) -> Self {
        Self {
            events,
            session_id,
            clock,
            calls: HashMap::new(),
            started: HashMap::new(),
            run_log,
            text: String::new(),
            emitted: false,
        }
    }

    /// 是否已经推过文本增量。
    fn emitted_text(&self) -> bool {
        self.emitted
    }

    /// 直接推一条增量（补最终文本 / 失败说明）。
    fn emit_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.emitted = true;
        self.events.send(Event::AgentDelta {
            session_id: self.session_id,
            text: text.to_string(),
        });
    }

    fn emit_step(&self, step: u32) {
        self.events.send(Event::AgentStep {
            session_id: self.session_id,
            step,
        });
    }

    /// 失败/取消时写回历史的助手消息（保留已生成的部分，方便用户追问）。
    fn take_messages(&mut self) -> Vec<ChatMessage> {
        if self.text.is_empty() {
            return Vec::new();
        }
        vec![ChatMessage::assistant_text(self.text.clone())]
    }

    /// 给日志行加会话前缀：`logs` 没有 session 列，面板靠 `[session 3f2a…]` 区分会话。
    fn run_log_line(&self, line: String) -> String {
        format!("{}{line}", log::session_prefix(&self.session_id))
    }

    /// 整轮失败：写一行 `error`（`logs_tail` 里能看见"这一轮为什么没做成"）。
    fn log_failure(&self, error: &str) {
        self.run_log
            .append("error", self.run_log_line(log::turn_failure_line(error)));
    }

    /// 整轮取消：写一行 `info`（取消不是故障，别用 error 吓人）。
    fn log_cancel(&self) {
        self.run_log
            .append("info", self.run_log_line(log::turn_cancel_line()));
    }
}

impl TurnSink for EventTurnSink {
    fn on_step(&mut self, step: u32) {
        self.emit_step(step);
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.emit_delta(delta);
    }

    fn on_tool_call(&mut self, call: &ToolCallRequest) {
        // 执行前的状态先记下：执行结果由 `on_tool_result` 补。
        self.started.insert(call.id.clone(), Instant::now());
        self.calls.insert(call.id.clone(), call.clone());
    }

    fn on_tool_result(&mut self, call_id: &str, output: &ToolOutput) {
        let Some(call) = self.calls.get(call_id) else {
            tracing::warn!(call_id, "工具结果没有对应的调用记录");
            return;
        };
        // 一步一条：工具名 + 成功/失败 + 耗时（参数与输出都过脱敏，见 `log` 模块）。
        let elapsed = self
            .started
            .remove(call_id)
            .map_or(Duration::ZERO, |started| started.elapsed());
        let (level, line) = log::tool_result_line(call, output, elapsed);
        self.run_log.append(level, self.run_log_line(line));

        let now = self.clock.now();
        let content = truncate_chars(&output.content, MAX_TOOL_RESULT_CHARS);
        let result = serde_json::json!({
            "content": content,
            "isError": output.is_error,
            "truncated": output.truncated,
        });
        self.events.send(Event::AgentTool {
            session_id: self.session_id,
            call: ToolCall {
                id: call.id.clone(),
                session_id: self.session_id,
                name: call.name.clone(),
                args: call.arguments.clone(),
                result: Some(result),
                status: if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Succeeded
                },
                approval: None,
                created_at: now,
                finished_at: Some(now),
            },
        });
    }
}

/// 按字符截断（不切 UTF-8）。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// 审计落点：只写 tracing，**不含工具参数**（参数里可能有文件内容或密钥）。
pub struct TracingAudit;

impl AuditSink for TracingAudit {
    fn record(&self, event: &AuditEvent) {
        // 刻意不打印 `event.key` 的规范化参数：那是完整动作，可能含正文。
        tracing::debug!(
            session_id = %event.session_id,
            call_id = event.call_id.as_str(),
            tool = event.tool.as_str(),
            sandbox = event.sandbox.as_str(),
            outcome = event.outcome.as_str(),
            duration_ms = event.duration_ms,
            "tool audit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::TempDir;

    /// 测试用 `RunLog` 收集器：把 Agent 写出的每一行留在内存里供断言。
    #[derive(Default)]
    struct CaptureSink {
        lines: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl RunLogSink for CaptureSink {
        fn append(&self, level: &str, line: String) {
            self.lines
                .lock()
                .expect("日志锁")
                .push((level.to_owned(), line));
        }
    }

    impl CaptureSink {
        fn lines(&self) -> Vec<(String, String)> {
            self.lines.lock().expect("日志锁").clone()
        }
    }

    fn sink_for(session_id: SessionId) -> (Arc<CaptureSink>, EventTurnSink) {
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn RunLogSink> = capture.clone();
        let turn = EventTurnSink::new(
            Arc::new(EventBus::new()),
            session_id,
            Arc::new(hbot_adapters::SystemClock),
            sink,
        );
        (capture, turn)
    }

    fn session() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::from_u128(0x44))
    }

    /// 一步一条：一次工具调用正好一行，带工具名 + 成功/失败 + 耗时。
    ///
    /// "一步一条"是任务里的硬要求：写三条（开始/结束/摘要）会把 `logs_tail` 刷爆。
    #[test]
    fn each_tool_call_writes_exactly_one_run_log_line() {
        let (capture, mut turn) = sink_for(session());

        turn.on_tool_call(&ToolCallRequest {
            id: "call-1".to_owned(),
            name: "fs_read".to_owned(),
            arguments: serde_json::json!({ "path": "src/main.ts" }),
        });
        turn.on_tool_result("call-1", &ToolOutput::text("ok"));

        turn.on_tool_call(&ToolCallRequest {
            id: "call-2".to_owned(),
            name: "fs_write".to_owned(),
            arguments: serde_json::json!({ "path": "src/b.txt" }),
        });
        turn.on_tool_result("call-2", &ToolOutput::error("disk full"));

        let lines = capture.lines();
        assert_eq!(
            lines.len(),
            2,
            "两次工具调用只该有两行（一步一条），实际 {lines:?}"
        );
        assert_eq!(lines[0].0, "info");
        assert!(lines[0].1.contains("fs_read"), "实际 {}", lines[0].1);
        assert!(lines[0].1.contains("成功"), "实际 {}", lines[0].1);
        assert_eq!(lines[1].0, "error");
        assert!(lines[1].1.contains("fs_write"), "实际 {}", lines[1].1);
        assert!(lines[1].1.contains("失败"), "实际 {}", lines[1].1);
        assert!(lines[1].1.contains("disk full"), "失败原因要留下");
        for (_, line) in &lines {
            assert!(line.contains(" ms）"), "每行都要带耗时：{line}");
        }
    }

    /// 工具参数里的密钥**不许**原样进 `logs` 表。
    ///
    /// 模型给的参数里经常夹着 `token` 字段与 `Authorization: Bearer ...`；这一层是
    /// 写库前的最后一道（`logs` 表落库后会被 `logs_tail` 原样推给 UI、也会被导出）。
    #[test]
    fn tool_arguments_are_redacted_before_the_log_line() {
        let (capture, mut turn) = sink_for(session());
        turn.on_tool_call(&ToolCallRequest {
            id: "call-1".to_owned(),
            name: "http_fetch".to_owned(),
            arguments: serde_json::json!({
                "url": "https://example.com",
                "token": "sk-live-SECRET-123",
                "headers": { "Authorization": "Bearer abcdef123456" },
                "body": "x".repeat(500),
            }),
        });
        turn.on_tool_result("call-1", &ToolOutput::text("ok"));

        let lines = capture.lines();
        assert_eq!(lines.len(), 1);
        let line = &lines[0].1;
        assert!(!line.contains("sk-live-SECRET-123"), "密钥泄漏：{line}");
        assert!(!line.contains("abcdef123456"), "Bearer 凭据泄漏：{line}");
        assert!(line.contains(log::REDACTED), "要留下脱敏标记：{line}");
        assert!(line.contains("example.com"), "普通参数要保留：{line}");
    }

    /// 整轮失败写一行 `error`；取消写一行 `info`（取消不是故障）。
    #[test]
    fn failed_and_cancelled_turns_leave_a_line() {
        let (capture, turn) = sink_for(session());
        turn.log_failure("provider 401: key=sk-live-999");
        turn.log_cancel();

        let lines = capture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "error");
        // `logs` 没有 session 列：会话 ID 必须体现在行前缀里。
        assert!(lines[0].1.starts_with("[session "), "{}", lines[0].1);
        assert!(lines[0].1.contains("agent 本轮失败："), "{}", lines[0].1);
        assert!(!lines[0].1.contains("sk-live-999"), "{}", lines[0].1);
        assert_eq!(lines[1].0, "info");
        assert!(lines[1].1.contains("已取消"));
        assert!(lines[1].1.starts_with("[session "), "{}", lines[1].1);
    }

    /// 没有对应调用的结果（乱序/重复回调）不写日志，也不能 panic。
    #[test]
    fn orphan_tool_results_are_ignored() {
        let (capture, mut turn) = sink_for(session());
        turn.on_tool_result("unknown", &ToolOutput::text("ok"));
        assert!(capture.lines().is_empty());
    }

    /// 构造时注入的落点就是 `start_turn` 读的那个字段。
    #[test]
    fn run_log_sink_is_injected_by_constructor() {
        let capture: Arc<dyn RunLogSink> = Arc::new(CaptureSink::default());
        let manager = AgentManager::new(
            std::env::temp_dir(),
            Arc::new(EventBus::new()),
            Arc::new(crate::app::ids::UuidIds),
            Arc::new(hbot_adapters::SystemClock),
            Arc::clone(&capture),
        );
        assert!(Arc::ptr_eq(manager.run_log_sink(), &capture));
    }

    fn project_with_rules(label: &str, rules: &str) -> (TempDir, Workspace) {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("创建测试项目目录");
        std::fs::write(project.join(PROJECT_RULES_FILE), rules).expect("写 AGENTS.md");
        let workspace = Workspace::open(project).expect("打开测试项目");
        (dir, workspace)
    }

    #[test]
    fn project_rules_are_injected_into_the_system_prompt() {
        let (_dir, workspace) = project_with_rules("rules", "项目规则：只改 src/。");

        let rules = read_project_rules(&workspace);
        assert_eq!(rules.as_deref(), Some("项目规则：只改 src/。"));

        let registry = ToolRegistry::default_tools();
        let prompt = build_system_prompt(
            &registry,
            &workspace,
            SandboxMode::WorkspaceWrite,
            12,
            rules,
        );
        assert!(
            prompt.contains("项目规则：只改 src/"),
            "系统提示里应带上项目规则"
        );
        assert!(
            prompt.contains(&workspace.root().display().to_string()),
            "系统提示里应带上项目根目录"
        );
        assert!(prompt.contains("write_file"), "系统提示里应列出可用工具");
    }

    #[test]
    fn without_project_rules_nothing_is_injected() {
        let dir = TempDir::new("no-rules");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("创建测试项目目录");
        let workspace = Workspace::open(project).expect("打开测试项目");

        assert!(read_project_rules(&workspace).is_none());
        let registry = ToolRegistry::default_tools();
        let prompt =
            build_system_prompt(&registry, &workspace, SandboxMode::WorkspaceWrite, 12, None);
        assert!(!prompt.is_empty());
    }

    #[test]
    fn overlong_project_rules_are_truncated() {
        let rules: String = "规".repeat(MAX_PROJECT_RULES_CHARS + 100);
        let (_dir, workspace) = project_with_rules("long-rules", &rules);

        let read = read_project_rules(&workspace).expect("应当读到项目规则");
        assert!(read.ends_with("…（项目规则过长已截断）"));
        assert!(
            read.chars().count() <= MAX_PROJECT_RULES_CHARS + 32,
            "截断后长度应回到上限附近，实际 {}",
            read.chars().count()
        );
    }

    #[test]
    fn empty_project_rules_are_ignored() {
        let (_dir, workspace) = project_with_rules("empty-rules", "   \n\t\n");
        assert!(read_project_rules(&workspace).is_none());
    }
}
