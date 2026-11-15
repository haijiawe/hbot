//! 工具网关：**固定顺序**的唯一执行入口。
//!
//! 顺序（不可换、不可跳过）：
//!
//! ```text
//! 参数校验 → 工具存在 → 沙箱档 → 路径 realpath → 审批 → 执行 → 审计
//! ```
//!
//! 每条调用都留下 [`GatewayStep`] 轨迹，测试断言"拒绝发生在哪一步"，
//! 而不是靠日志猜测。三条硬约束：
//!
//! - **只读档下写工具连审批都不弹**：沙箱档在审批之前判定，直接被拒；
//! - **路径 realpath 由 workspace 做**，网关只判结果并把归一化路径写回参数，
//!   保证审批键比较的是同一个规范路径；
//! - **审计永远写**：成功、失败、拒绝、取消都记一条 [`AuditEvent`]，
//!   不存在"先执行再想审计"的窗口。

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use hbot_core::{ApprovalDecision, HbotError, SandboxMode, SessionId};
use hbot_workspace::Workspace;
use serde_json::Value;

use crate::approvals::{ApprovalKey, ApprovalPolicy, ApprovalRequest, ApprovalStore, Approver};
use crate::cancel::CancellationToken;
use crate::provider::{ToolCallRequest, ToolSpec};
use crate::tools::{
    Risk, Tool, ToolCtx, ToolOutput, ToolRegistry, clamp_output, normalize_relative, validate_args,
};

/// 网关的固定步骤。
///
/// 变体顺序就是执行顺序；[`GatewayStep::ALL`] 是唯一真源，
/// 新增步骤必须插在正确位置并同步 [`GatewayStep::ALL`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GatewayStep {
    /// 参数校验（必填 / 类型）。
    ValidateArgs,
    /// 工具存在。
    ToolExists,
    /// 沙箱档。
    SandboxMode,
    /// 路径 realpath（在 workspace 内完成）。
    PathRealpath,
    /// 审批。
    Approval,
    /// 执行。
    Execute,
    /// 审计。
    Audit,
}

impl GatewayStep {
    /// 全部步骤，按执行顺序。
    pub const ALL: [Self; 7] = [
        Self::ValidateArgs,
        Self::ToolExists,
        Self::SandboxMode,
        Self::PathRealpath,
        Self::Approval,
        Self::Execute,
        Self::Audit,
    ];

    /// 稳定的字符串形式（日志/审计/UI）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ValidateArgs => "validate_args",
            Self::ToolExists => "tool_exists",
            Self::SandboxMode => "sandbox_mode",
            Self::PathRealpath => "path_realpath",
            Self::Approval => "approval",
            Self::Execute => "execute",
            Self::Audit => "audit",
        }
    }
}

impl std::fmt::Display for GatewayStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 沙箱档强弱（越宽越大）。
pub fn sandbox_rank(mode: SandboxMode) -> u8 {
    match mode {
        SandboxMode::ReadOnly => 0,
        SandboxMode::WorkspaceWrite => 1,
        SandboxMode::Danger => 2,
    }
}

/// 审计结果分类。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditOutcome {
    /// 执行成功。
    Succeeded,
    /// 执行失败（工具返回失败或内部错误）。
    Failed {
        /// 失败原因（已脱敏的错误文本）。
        message: String,
    },
    /// 被策略/用户/沙箱档拒绝。
    Denied {
        /// 拒绝原因。
        message: String,
    },
    /// 被取消。
    Cancelled,
}

impl AuditOutcome {
    /// 稳定的字符串形式。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed { .. } => "failed",
            Self::Denied { .. } => "denied",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 一条工具审计事件。
#[derive(Clone, Debug)]
pub struct AuditEvent {
    /// 会话。
    pub session_id: SessionId,
    /// 工具调用 id（与 provider 的 `tool_call_id` 对齐）。
    pub call_id: String,
    /// 工具名。
    pub tool: String,
    /// 审批键（只在走过审批时存在）。
    pub key: Option<ApprovalKey>,
    /// 风险类别（工具不存在时为空）。
    pub risk: Option<Risk>,
    /// 当时的沙箱档。
    pub sandbox: SandboxMode,
    /// 结果分类。
    pub outcome: AuditOutcome,
    /// 总耗时。
    pub duration_ms: u64,
}

/// 审计出口。
///
/// 故意是**不可失败**的 `record`：审计不能因为下游写失败而反过来让工具"看起来
/// 没执行"。真正的持久化由实现方自己保证（落库失败要单独报警，不能吞）。
pub trait AuditSink: Send + Sync {
    /// 记录一条事件。
    fn record(&self, event: &AuditEvent);
}

/// 丢弃审计（测试/无审计场景）。
#[derive(Clone, Copy, Debug, Default)]
pub struct NullAudit;

impl AuditSink for NullAudit {
    fn record(&self, _event: &AuditEvent) {}
}

/// 网关错误：保留"走到哪一步"的证据。
#[derive(Debug)]
pub struct GatewayError {
    /// 具体错误。
    pub error: HbotError,
    /// 已到达的步骤（最后一个是失败点；`Audit` 总会追加）。
    pub steps: Vec<GatewayStep>,
    /// 工具名。
    pub tool_name: String,
    /// 审批结论（若走到审批）。
    pub approval: Option<ApprovalDecision>,
    /// 风险类别（工具存在时）。
    pub risk: Option<Risk>,
    /// 审批键（若走到审批）。
    pub key: Option<ApprovalKey>,
}

impl GatewayError {
    /// 失败发生在哪一步（`Audit` 之前）。
    pub fn failed_at(&self) -> Option<GatewayStep> {
        self.steps
            .iter()
            .rev()
            .find(|step| **step != GatewayStep::Audit)
            .copied()
    }

    /// 是否到达过某一步。
    pub fn reached(&self, step: GatewayStep) -> bool {
        self.steps.contains(&step)
    }

    /// 供日志/错误消息使用的单行描述。
    pub fn describe(&self) -> String {
        let step = self
            .failed_at()
            .map_or_else(|| "unknown".to_string(), |step| step.as_str().to_string());
        format!("工具 {} 在 {} 步失败：{}", self.tool_name, step, self.error)
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.describe())
    }
}

impl std::error::Error for GatewayError {}

impl From<GatewayError> for HbotError {
    fn from(error: GatewayError) -> Self {
        error.error
    }
}

/// 一次成功调用的结果。
#[derive(Clone, Debug)]
pub struct GatewayOutcome {
    /// 工具输出（已按上限截断）。
    pub output: ToolOutput,
    /// 完整步骤轨迹。
    pub steps: Vec<GatewayStep>,
    /// 审批结论（只读/免审批时为空）。
    pub approval: Option<ApprovalDecision>,
    /// 总耗时。
    pub duration_ms: u64,
    /// 审批键（若走到审批）。
    pub key: Option<ApprovalKey>,
}

/// 内部轨迹，随错误一起返回。
#[derive(Default)]
struct CallTrace {
    steps: Vec<GatewayStep>,
    approval: Option<ApprovalDecision>,
    risk: Option<Risk>,
    key: Option<ApprovalKey>,
}

/// 工具网关。
pub struct ToolGateway {
    registry: Arc<ToolRegistry>,
    workspace: Arc<Workspace>,
    level: SandboxMode,
    approvals: Arc<ApprovalStore>,
    audit: Arc<dyn AuditSink>,
}

impl ToolGateway {
    /// 新建网关。
    pub fn new(
        registry: Arc<ToolRegistry>,
        workspace: Arc<Workspace>,
        level: SandboxMode,
        approvals: Arc<ApprovalStore>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            registry,
            workspace,
            level,
            approvals,
            audit,
        }
    }

    /// 当前沙箱档。
    pub fn level(&self) -> SandboxMode {
        self.level
    }

    /// 注册表。
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// 审批规则库。
    pub fn approvals(&self) -> &Arc<ApprovalStore> {
        &self.approvals
    }

    /// 工具声明（provider 用）。
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.registry.specs()
    }

    /// 执行一次工具调用。
    pub async fn call(
        &self,
        session_id: SessionId,
        call: ToolCallRequest,
        cancel: &CancellationToken,
        approver: &dyn Approver,
    ) -> Result<GatewayOutcome, GatewayError> {
        let started = Instant::now();
        let mut trace = CallTrace::default();
        let result = self
            .dispatch(session_id, &call, cancel, approver, &mut trace)
            .await;
        trace.steps.push(GatewayStep::Audit);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = match &result {
            Ok(output) if output.is_error => AuditOutcome::Failed {
                message: "工具返回失败结果".to_string(),
            },
            Ok(_) => AuditOutcome::Succeeded,
            Err(HbotError::Denied { .. }) if cancel.is_cancelled() => AuditOutcome::Cancelled,
            Err(HbotError::Denied { message }) => AuditOutcome::Denied {
                message: message.clone(),
            },
            Err(other) => AuditOutcome::Failed {
                message: other.to_string(),
            },
        };
        self.audit.record(&AuditEvent {
            session_id,
            call_id: call.id.clone(),
            tool: call.name.clone(),
            key: trace.key.clone(),
            risk: trace.risk,
            sandbox: self.level,
            outcome,
            duration_ms,
        });
        match result {
            Ok(output) => {
                tracing::debug!(
                    tool = %call.name,
                    duration_ms,
                    "工具执行成功"
                );
                Ok(GatewayOutcome {
                    output,
                    steps: trace.steps,
                    approval: trace.approval,
                    duration_ms,
                    key: trace.key,
                })
            }
            Err(error) => {
                let failed = GatewayError {
                    error,
                    steps: trace.steps,
                    tool_name: call.name,
                    approval: trace.approval,
                    risk: trace.risk,
                    key: trace.key,
                };
                tracing::warn!(error = %failed.describe(), "工具调用被拒绝或失败");
                Err(failed)
            }
        }
    }

    async fn dispatch(
        &self,
        session_id: SessionId,
        call: &ToolCallRequest,
        cancel: &CancellationToken,
        approver: &dyn Approver,
        trace: &mut CallTrace,
    ) -> Result<ToolOutput, HbotError> {
        // 1) 参数：先做结构判定（参数不是对象连工具都查不了），
        //    工具存在时再按它的 schema 校验必填/类型。
        trace.steps.push(GatewayStep::ValidateArgs);
        if !call.arguments.is_object() {
            return Err(HbotError::invalid(format!(
                "工具 `{}` 的参数必须是 JSON 对象",
                call.name
            )));
        }
        let looked_up = self.registry.get(&call.name);
        if let Some(tool) = &looked_up {
            validate_args(&call.name, &tool.schema(), &call.arguments)?;
        }

        // 2) 工具存在。
        trace.steps.push(GatewayStep::ToolExists);
        let tool = looked_up.ok_or_else(|| HbotError::not_found("tool", call.name.clone()))?;
        trace.risk = Some(tool.risk());

        // 3) 沙箱档：低于工具要求直接拒，**不进审批**。
        trace.steps.push(GatewayStep::SandboxMode);
        let required = tool.min_sandbox();
        if sandbox_rank(self.level) < sandbox_rank(required) {
            return Err(HbotError::denied(format!(
                "当前沙箱档 {} 不允许工具 {}（至少需要 {}）",
                self.level.as_str(),
                tool.name(),
                required.as_str()
            )));
        }

        // 4) 路径 realpath：真正解析在 workspace 里，网关只判结果并把
        //    规范化后的相对路径写回参数，审批键因此稳定。
        let mut args = call.arguments.clone();
        if let Some(arg) = tool.path_arg()
            && let Some(raw) = args.get(arg).and_then(Value::as_str)
        {
            trace.steps.push(GatewayStep::PathRealpath);
            let canonical = self.canonicalize_path(tool.as_ref(), raw)?;
            if let Some(map) = args.as_object_mut() {
                map.insert(arg.to_string(), Value::String(canonical));
            }
        }

        // 5) 审批：免审批的工具（只读）整步跳过——不是"批准后跳过"。
        if tool.requires_approval(self.level, &args) {
            trace.steps.push(GatewayStep::Approval);
            let key = ApprovalKey::new(tool.name(), &args, self.level);
            trace.key = Some(key.clone());
            match self.approvals.policy_for(&key) {
                ApprovalPolicy::Deny(_) => {
                    trace.approval = Some(ApprovalDecision::Deny);
                    return Err(HbotError::denied(format!(
                        "该动作已被拒绝（需手动解除）：{}",
                        key.describe()
                    )));
                }
                ApprovalPolicy::AlwaysAllowThisExactKey(_) => {
                    trace.approval = Some(ApprovalDecision::Approve);
                }
                ApprovalPolicy::Ask => {
                    let request = ApprovalRequest {
                        key: key.clone(),
                        tool: tool.name().to_string(),
                        args: args.clone(),
                        risk: tool.risk(),
                        sandbox: self.level,
                    };
                    let reply = approver.approve(&request).await?;
                    trace.approval = Some(reply.decision);
                    if !reply.is_approved() {
                        return Err(HbotError::denied(format!(
                            "用户拒绝执行：{}",
                            key.describe()
                        )));
                    }
                    if reply.remember {
                        // 只记住这一个完整键：换路径 / 换内容 / 换沙箱档都不算。
                        self.approvals.allow_exactly(key.clone());
                    }
                }
            }
            // 审批等待期间可能被取消：取消后不得再执行。
            cancel.check()?;
        }

        // 6) 执行。
        trace.steps.push(GatewayStep::Execute);
        cancel.check()?;
        let ctx = ToolCtx {
            workspace: Arc::clone(&self.workspace),
            sandbox: self.level,
            cancel,
            session_id,
            call_id: &call.id,
        };
        let timeout = tool.timeout();
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let output = match tokio::time::timeout(timeout, tool.run(args, &ctx)).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(HbotError::timeout(
                    format!("工具 {}", tool.name()),
                    timeout_ms,
                ));
            }
        };
        let (content, truncated) = clamp_output(output.content);
        Ok(ToolOutput {
            content,
            truncated,
            is_error: output.is_error,
        })
    }

    fn canonicalize_path(&self, tool: &dyn Tool, raw: &str) -> Result<String, HbotError> {
        let rel = Path::new(raw);
        let resolved = if tool.may_create_path() {
            self.workspace
                .resolve_new(rel)
                .map_err(|error| match error {
                    HbotError::NotFound { .. } => {
                        HbotError::invalid(format!("路径 `{raw}` 的父目录不存在或不可写"))
                    }
                    other => other,
                })
        } else {
            self.workspace.resolve_existing(rel)
        }?;
        let relative = self.workspace.relative_path(&resolved)?;
        Ok(normalize_relative(&relative))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use hbot_core::{ApprovalDecision, HbotError, SandboxMode};
    use serde_json::{Value, json};

    use super::*;
    use crate::approvals::{ApprovalKey, ApprovalReply, ApprovalRequest};
    use crate::test_support::{
        RecordingApprover, RecordingAudit, RecordingTool, TestDir, session_id,
    };
    use crate::tools::{ListDirTool, ReadFileTool, Risk, WriteFileTool};

    fn call(name: &str, arguments: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: format!("call_{name}"),
            name: name.to_string(),
            arguments,
        }
    }

    struct Fixture {
        gateway: ToolGateway,
        audit: RecordingAudit,
        dir: TestDir,
    }

    impl Fixture {
        fn new(level: SandboxMode, tools: &[Arc<dyn Tool>]) -> Self {
            let dir = TestDir::new("gateway");
            dir.write("a.txt", "old");
            dir.write("sub/a.txt", "nested");
            dir.write("sub/b.txt", "other");
            let workspace = dir.workspace();
            let mut registry = ToolRegistry::new();
            for tool in tools {
                registry.register(Arc::clone(tool));
            }
            let audit = RecordingAudit::new();
            let gateway = ToolGateway::new(
                Arc::new(registry),
                workspace,
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

    #[test]
    fn steps_are_declared_in_fixed_order() {
        let expected = [
            GatewayStep::ValidateArgs,
            GatewayStep::ToolExists,
            GatewayStep::SandboxMode,
            GatewayStep::PathRealpath,
            GatewayStep::Approval,
            GatewayStep::Execute,
            GatewayStep::Audit,
        ];
        assert_eq!(GatewayStep::ALL, expected);
        // 轨迹必须是声明顺序的子序列：不允许倒着走或跳着回来。
        let trace = [
            GatewayStep::ValidateArgs,
            GatewayStep::ToolExists,
            GatewayStep::SandboxMode,
            GatewayStep::Execute,
            GatewayStep::Audit,
        ];
        let mut cursor = 0;
        for step in trace {
            let position = GatewayStep::ALL[cursor..]
                .iter()
                .position(|candidate| *candidate == step)
                .map(|offset| cursor + offset)
                .expect("步骤必须按声明顺序出现");
            cursor = position + 1;
        }
    }

    #[tokio::test]
    async fn read_only_rejects_write_before_approval() {
        let fixture = Fixture::new(
            SandboxMode::ReadOnly,
            &[Arc::new(WriteFileTool), Arc::new(ReadFileTool)],
        );
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("write_file", json!({ "path": "a.txt", "content": "new" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("只读档必须拒绝写入");

        assert!(matches!(error.error, HbotError::Denied { .. }));
        // 走到沙箱档就停：没有 realpath、没有审批、没有执行。
        assert_eq!(
            error.steps,
            vec![
                GatewayStep::ValidateArgs,
                GatewayStep::ToolExists,
                GatewayStep::SandboxMode,
                GatewayStep::Audit,
            ]
        );
        assert!(!error.reached(GatewayStep::Approval));
        assert_eq!(error.failed_at(), Some(GatewayStep::SandboxMode));
        assert_eq!(approver.count(), 0, "被沙箱档拒绝时不得弹审批");
        assert_eq!(fixture.dir.read("a.txt"), "old", "不得写入");
        let events = fixture.audit.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].outcome, AuditOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn read_tool_skips_approval_entirely() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(ReadFileTool)]);
        let approver = RecordingApprover::denying();
        let cancel = CancellationToken::new();
        let outcome = fixture
            .gateway
            .call(
                session_id(),
                call("read_file", json!({ "path": "a.txt" })),
                &cancel,
                &approver,
            )
            .await
            .expect("只读工具应放行");
        assert_eq!(
            outcome.steps,
            vec![
                GatewayStep::ValidateArgs,
                GatewayStep::ToolExists,
                GatewayStep::SandboxMode,
                GatewayStep::PathRealpath,
                GatewayStep::Execute,
                GatewayStep::Audit,
            ]
        );
        assert!(outcome.output.content.contains("old"));
        assert_eq!(approver.count(), 0);
    }

    #[tokio::test]
    async fn write_tool_asks_then_executes() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(WriteFileTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let outcome = fixture
            .gateway
            .call(
                session_id(),
                call("write_file", json!({ "path": "a.txt", "content": "new" })),
                &cancel,
                &approver,
            )
            .await
            .expect("应放行");
        assert_eq!(approver.count(), 1);
        assert_eq!(outcome.approval, Some(ApprovalDecision::Approve));
        assert_eq!(
            outcome.steps,
            vec![
                GatewayStep::ValidateArgs,
                GatewayStep::ToolExists,
                GatewayStep::SandboxMode,
                GatewayStep::PathRealpath,
                GatewayStep::Approval,
                GatewayStep::Execute,
                GatewayStep::Audit,
            ]
        );
        assert_eq!(fixture.dir.read("a.txt"), "new");
        let events = fixture.audit.events();
        assert_eq!(events.len(), 1);
        assert!(events[0].key.is_some(), "审计要带上完整审批键");
        assert_eq!(events[0].outcome, AuditOutcome::Succeeded);
    }

    #[tokio::test]
    async fn policy_deny_does_not_prompt() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(WriteFileTool)]);
        let key = ApprovalKey::new(
            "write_file",
            &json!({ "path": "a.txt", "content": "new" }),
            SandboxMode::WorkspaceWrite,
        );
        fixture.gateway.approvals().deny_exactly(key);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("write_file", json!({ "path": "a.txt", "content": "new" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("策略拒绝");
        assert!(matches!(error.error, HbotError::Denied { .. }));
        assert_eq!(error.failed_at(), Some(GatewayStep::Approval));
        assert_eq!(approver.count(), 0, "规则已拒绝，不该再问用户");
        assert_eq!(fixture.dir.read("a.txt"), "old");
    }

    #[tokio::test]
    async fn remembered_approval_matches_only_the_same_normalized_action() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(WriteFileTool)]);
        let approver = RecordingApprover::approving_always();
        let cancel = CancellationToken::new();

        // 第一次：批准并记住（路径写成 ./sub/a.txt）。
        fixture
            .gateway
            .call(
                session_id(),
                call(
                    "write_file",
                    json!({ "path": "./sub/a.txt", "content": "one" }),
                ),
                &cancel,
                &approver,
            )
            .await
            .expect("第一次应放行");
        assert_eq!(approver.count(), 1);

        // 第二次：同一动作、路径写法不同 → 归一化后同键，不再问。
        fixture
            .gateway
            .call(
                session_id(),
                call(
                    "write_file",
                    json!({ "path": "sub/a.txt", "content": "one" }),
                ),
                &cancel,
                &approver,
            )
            .await
            .expect("同键应直接放行");
        assert_eq!(
            approver.count(),
            1,
            "同工具不同路径写法归一化后应共享同一个键"
        );

        // 第三次：同工具、不同路径 → 必须重新审批。
        fixture
            .gateway
            .call(
                session_id(),
                call(
                    "write_file",
                    json!({ "path": "sub/b.txt", "content": "one" }),
                ),
                &cancel,
                &approver,
            )
            .await
            .expect("新路径应再问一次并放行");
        assert_eq!(approver.count(), 2, "不同路径不得共享已批准");

        // 第四次：同路径但内容不同 → 键不同，必须再问。
        fixture
            .gateway
            .call(
                session_id(),
                call(
                    "write_file",
                    json!({ "path": "sub/a.txt", "content": "two" }),
                ),
                &cancel,
                &approver,
            )
            .await
            .expect("不同内容应再问一次并放行");
        assert_eq!(approver.count(), 3, "不同内容不得共享已批准");
    }

    #[tokio::test]
    async fn unknown_tool_stops_at_lookup() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(ReadFileTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("rm_rf", json!({ "path": "a.txt" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("未知工具");
        assert!(matches!(error.error, HbotError::NotFound { .. }));
        assert_eq!(
            error.steps,
            vec![
                GatewayStep::ValidateArgs,
                GatewayStep::ToolExists,
                GatewayStep::Audit,
            ]
        );
        assert_eq!(approver.count(), 0);
    }

    #[tokio::test]
    async fn invalid_args_stop_before_lookup() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(ReadFileTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("read_file", json!({ "path": 42 })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("参数类型错误");
        assert!(matches!(error.error, HbotError::Invalid { .. }));
        assert_eq!(
            error.steps,
            vec![GatewayStep::ValidateArgs, GatewayStep::Audit]
        );
    }

    #[tokio::test]
    async fn path_escape_is_rejected_by_workspace() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(ReadFileTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("read_file", json!({ "path": "../../etc/passwd" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("路径逃逸");
        assert!(
            !error.reached(GatewayStep::Execute),
            "逃逸必须在 realpath 阶段被拒"
        );
        assert_eq!(error.failed_at(), Some(GatewayStep::PathRealpath));
    }

    #[tokio::test]
    async fn tool_error_is_reported_with_trace_and_audit() {
        let tool = Arc::new(
            RecordingTool::new("boom")
                .with_path_arg("path")
                .fail_with("炸了"),
        );
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[tool]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("boom", json!({ "path": "a.txt" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("工具内部错误");
        assert!(matches!(error.error, HbotError::Internal { .. }));
        assert_eq!(error.failed_at(), Some(GatewayStep::Execute));
        let events = fixture.audit.events();
        assert!(matches!(events[0].outcome, AuditOutcome::Failed { .. }));
    }

    #[tokio::test]
    async fn cancellation_during_approval_prevents_execution() {
        /// 用户还在确认时被取消。
        struct CancellingApprover(CancellationToken);

        #[async_trait]
        impl Approver for CancellingApprover {
            async fn approve(
                &self,
                _request: &ApprovalRequest,
            ) -> Result<ApprovalReply, HbotError> {
                self.0.cancel();
                Ok(ApprovalReply::approve_once())
            }
        }

        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(WriteFileTool)]);
        let cancel = CancellationToken::new();
        let approver = CancellingApprover(cancel.clone());
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("write_file", json!({ "path": "a.txt", "content": "new" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("取消后不得执行");
        assert_eq!(error.failed_at(), Some(GatewayStep::Approval));
        assert_eq!(fixture.dir.read("a.txt"), "old");
        assert_eq!(fixture.audit.events()[0].outcome, AuditOutcome::Cancelled);
    }

    #[tokio::test]
    async fn creating_tool_resolves_new_path_and_canonicalizes_it() {
        let tool = Arc::new(
            RecordingTool::new("write_new")
                .with_risk(Risk::Write)
                .with_approval(true)
                .with_path_arg("path")
                .creating_paths(),
        );
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[tool]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let outcome = fixture
            .gateway
            .call(
                session_id(),
                call("write_new", json!({ "path": "./sub/new.txt", "text": "x" })),
                &cancel,
                &approver,
            )
            .await
            .expect("父目录存在时允许新建");
        assert!(outcome.steps.contains(&GatewayStep::PathRealpath));

        // 审批键必须是规范化后的完整动作：路径归一化、带 sandbox_mode。
        let requests = approver.requests();
        assert_eq!(requests.len(), 1);
        let key = &requests[0].key;
        assert!(
            key.canonical_args().contains("sub/new.txt"),
            "审批键里的路径必须是规范化后的相对路径：{}",
            key.canonical_args()
        );
        assert!(!key.canonical_args().contains("./sub"), "不得保留 ./ 前缀");
        assert_eq!(key.sandbox(), SandboxMode::WorkspaceWrite.as_str());
        assert_eq!(key.tool(), "write_new");
    }

    #[tokio::test]
    async fn list_dir_works_without_path_argument() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(ListDirTool)]);
        let approver = RecordingApprover::denying();
        let cancel = CancellationToken::new();
        let outcome = fixture
            .gateway
            .call(
                session_id(),
                call("list_dir", json!({})),
                &cancel,
                &approver,
            )
            .await
            .expect("列目录");
        assert!(outcome.output.content.contains("a.txt"));
        assert!(!outcome.steps.contains(&GatewayStep::PathRealpath));
    }
}
