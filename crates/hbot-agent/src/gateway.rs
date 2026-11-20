//! 工具网关：**固定顺序**的唯一执行入口。
//!
//! 顺序（不可换、不可跳过）：
//!
//! ```text
//! 能力声明 → 参数校验 → 工具存在 → 沙箱档 → 路径 realpath → 审批 → 执行 → 审计
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

use hbot_core::{
    ApprovalDecision, CapabilityGate, CommandRunner, DeviceBackend, HbotError, SandboxMode,
    SessionId,
};
use hbot_workspace::Workspace;
use serde_json::Value;

use crate::approvals::{ApprovalKey, ApprovalPolicy, ApprovalRequest, ApprovalStore, Approver};
use crate::cancel::CancellationToken;
use crate::edits::EditProposer;
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
    /// 能力声明（项目清单有没有声明这个工具要的设备能力）。
    Capability,
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
    pub const ALL: [Self; 8] = [
        Self::Capability,
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
            Self::Capability => "capability",
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
///
/// `error` **装箱**：`Result<GatewayOutcome, GatewayError>` 的 `Err` 侧要压在
/// 128 字节以内，否则新版本 clippy 的 `result_large_err` 会红（同
/// `hbot_code::EventFailure` 的取舍，见 `crates/hbot-code/src/dispatch.rs`）。
/// 字段语义与访问方式都不变：`error.error.code()` 靠 `Box` 的 `Deref` 照旧可用。
#[derive(Debug)]
pub struct GatewayError {
    /// 具体错误。
    pub error: Box<HbotError>,
    /// 已到达的步骤（最后一个是失败点；`Audit` 总会追加）。
    pub steps: Vec<GatewayStep>,
    /// 工具名。
    pub tool_name: String,
    /// 审批结论（若走到审批）。
    pub approval: Option<ApprovalDecision>,
    /// 风险类别（工具存在时）。
    pub risk: Option<Risk>,
    /// 审批键（若走到审批）。
    pub key: Option<Box<ApprovalKey>>,
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
        *error.error
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
    edits: Option<Arc<dyn EditProposer>>,
    command: Option<Arc<dyn CommandRunner>>,
    capabilities: Option<Arc<dyn CapabilityGate>>,
    device: Option<Arc<dyn DeviceBackend>>,
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
            edits: None,
            command: None,
            capabilities: None,
            device: None,
        }
    }

    /// 装配受控命令执行端口：`run_command` 从"一律拒绝"变成按沙箱档执行。
    ///
    /// 不装配时工具**仍然存在**（模型看得见），但调用会以"没有装配命令执行器"
    /// 拒绝——不是"平台不支持"：前者该由组合根补，后者换机器才有用。
    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command = Some(runner);
        self
    }

    /// 装配待审编辑端口：写类工具从此**不直接落盘**，改为登记待审编辑
    /// （由用户 `edit_apply` 落盘，见 [`crate::edits`]）。
    ///
    /// 只在真正的 Agent 回合里装配；网关自测与非 Agent 场景不装配，
    /// 保持"工具直接写盘"的老行为。
    #[must_use]
    pub fn with_edit_proposer(mut self, proposer: Arc<dyn EditProposer>) -> Self {
        self.edits = Some(proposer);
        self
    }

    /// 装配能力声明端口：网关第一步（`Capability`）用它判断"项目清单有没有
    /// 声明这个工具要的设备能力"。
    ///
    /// 不装配时的行为是**按需拒绝**，不是全拒：不需要能力的工具（今天除
    /// `read_screen` 以外的全部）照常执行；需要能力的工具会以"没有装配能力
    /// 声明端口"拒绝。这样旧调用方（网关自测、非 Agent 场景）不会因为多了
    /// 一步就全体失灵，而真正要能力的那条路仍然 fail-closed。
    #[must_use]
    pub fn with_capability_gate(mut self, gate: Arc<dyn CapabilityGate>) -> Self {
        self.capabilities = Some(gate);
        self
    }

    /// 装配设备执行端口：`read_screen` 从"没有装配设备后端"变成真能读屏。
    ///
    /// 只给执行，不给判据：能不能读由 `Capability` 步与设备闸决定。
    #[must_use]
    pub fn with_device(mut self, backend: Arc<dyn DeviceBackend>) -> Self {
        self.device = Some(backend);
        self
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
                    error: Box::new(error),
                    steps: trace.steps,
                    tool_name: call.name,
                    approval: trace.approval,
                    risk: trace.risk,
                    key: trace.key.map(Box::new),
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
        // 1) 能力声明：工具要求的设备能力必须在项目清单里声明过。
        //
        //    放在最前面（早于参数校验）：这一步不看参数，只判"这份代码有没有
        //    资格要这类能力"。用工具名查一次即可；查不到工具时**放过**，
        //    让下一步 `ToolExists` 报那个更准确的错。
        //
        //    与 `PathRealpath` / `Approval` 同一约定：**只在适用时才进轨迹**。
        //    不要求能力的工具（今天除 `read_screen` 以外的全部）不会凭空多出
        //    一步，既有测试与审计读法都不用改。
        if let Some(tool) = self.registry.get(&call.name) {
            let required = tool.required_capabilities();
            if !required.is_empty() {
                trace.steps.push(GatewayStep::Capability);
                let gate = self.capabilities.as_ref().ok_or_else(|| {
                    HbotError::denied(format!(
                        "工具 `{}` 需要设备能力，但没有装配能力声明端口",
                        call.name
                    ))
                })?;
                for capability in required {
                    gate.authorize(*capability).map_err(|error| {
                        HbotError::denied(format!(
                            "项目没有声明能力 `{}`：{}",
                            capability.as_str(),
                            error
                        ))
                    })?;
                }
            }
        }

        // 2) 参数：先做结构判定（参数不是对象连工具都查不了），
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

        // 3) 工具存在。
        trace.steps.push(GatewayStep::ToolExists);
        let tool = looked_up.ok_or_else(|| HbotError::not_found("tool", call.name.clone()))?;
        trace.risk = Some(tool.risk());

        // 4) 沙箱档：低于工具要求直接拒，**不进审批**。
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

        // 5) 路径 realpath：真正解析在 workspace 里，网关只判结果并把
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

        // 6) 审批：免审批的工具（只读）整步跳过——不是"批准后跳过"。
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

        // 7) 执行。
        trace.steps.push(GatewayStep::Execute);
        cancel.check()?;
        let ctx = ToolCtx {
            workspace: Arc::clone(&self.workspace),
            sandbox: self.level,
            cancel,
            session_id,
            call_id: &call.id,
            edits: self.edits.as_deref(),
            command: self.command.clone(),
            device: self.device.clone(),
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
            // 写入/建目录类工具允许**父目录尚不存在**（`write_file` 会按需建，
            // `create_dir` 自己建）：用宽松解析。守卫强度不变——`..`、绝对路径、
            // `.git`/`.hbot`、符号链接逃逸都在这里被拒，早于任何目录创建。
            self.workspace.resolve_write_target(rel)
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
    use crate::tools::{CreateDirTool, ListDirTool, ReadFileTool, Risk, WriteFileTool};

    /// 防的是：`GatewayError` 悄悄又长大，让**新版本** clippy 的 `result_large_err`
    /// 在 CI 红、而本地（clippy 版本更旧）全绿 —— 这类红只会在 push 之后出现。
    /// 阈值与 clippy 的默认 `large_error_threshold` 一致（128 字节）。
    #[test]
    fn gateway_error_stays_within_the_large_err_budget() {
        let size = std::mem::size_of::<GatewayError>();
        assert!(
            size < 128,
            "GatewayError 已长到 {size} 字节，超过 clippy result_large_err 的 128 字节预算；\
             请把大字段装箱（照 `error: Box<HbotError>` 的先例）"
        );
    }

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
            GatewayStep::Capability,
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

        assert!(matches!(*error.error, HbotError::Denied { .. }));
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
        assert!(matches!(*error.error, HbotError::Denied { .. }));
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
        assert!(matches!(*error.error, HbotError::NotFound { .. }));
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
        assert!(matches!(*error.error, HbotError::Invalid { .. }));
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
        assert!(matches!(*error.error, HbotError::Internal { .. }));
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

    /// 假命令执行器：只数调用次数，返回固定成功输出。
    ///
    /// 网关层的判据是"装配了就真的被调用 / 没装配就诚实拒绝"，
    /// 命令本身怎么跑由 `tools::shell` 的测试与 `hbot-command` 负责。
    #[derive(Default)]
    struct CountingRunner {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingRunner {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl hbot_core::CommandRunner for CountingRunner {
        fn run(
            &self,
            _spec: hbot_core::CommandSpec,
        ) -> Result<hbot_core::CommandOutput, hbot_core::CommandDenied> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(hbot_core::CommandOutput {
                status: hbot_core::CommandStatus::Exited {
                    code: Some(0),
                    signal: None,
                },
                stdout: "wired".to_string(),
                stderr: String::new(),
                usage: Default::default(),
                duration: std::time::Duration::from_millis(1),
            })
        }
    }

    /// 用指定沙箱档搭一个只注册 `run_command` 的网关。
    fn command_fixture(
        level: SandboxMode,
        programs: &[&str],
        runner: Arc<dyn hbot_core::CommandRunner>,
    ) -> (ToolGateway, RecordingAudit, TestDir) {
        let dir = TestDir::new("gateway-command");
        let workspace = dir.workspace();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(
            crate::tools::RunCommandTool::with_allowed_programs(programs.iter().copied()),
        ));
        let audit = RecordingAudit::new();
        let gateway = ToolGateway::new(
            Arc::new(registry),
            workspace,
            level,
            Arc::new(ApprovalStore::new()),
            Arc::new(audit.clone()),
        )
        .with_command_runner(runner);
        (gateway, audit, dir)
    }

    #[tokio::test]
    async fn command_runner_is_reached_through_the_sixth_step() {
        let runner = Arc::new(CountingRunner::default());
        let dynamic: Arc<dyn hbot_core::CommandRunner> = runner.clone();
        let (gateway, audit, _dir) = command_fixture(SandboxMode::Danger, &["echo"], dynamic);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let outcome = gateway
            .call(
                session_id(),
                call("run_command", json!({ "command": "echo", "args": ["hi"] })),
                &cancel,
                &approver,
            )
            .await
            .expect("装配了执行器 + danger 档应执行");

        assert_eq!(runner.calls(), 1, "装配后必须真的调到执行器");
        assert_eq!(
            outcome.steps,
            vec![
                GatewayStep::ValidateArgs,
                GatewayStep::ToolExists,
                GatewayStep::SandboxMode,
                GatewayStep::Approval,
                GatewayStep::Execute,
                GatewayStep::Audit,
            ]
        );
        assert_eq!(approver.count(), 1, "run_command 是 mutation，必须走审批");
        assert!(!outcome.output.is_error);
        assert!(outcome.output.content.contains("wired"));
        assert_eq!(audit.len(), 1);
    }

    #[tokio::test]
    async fn read_only_denies_run_command_before_approval_and_never_reaches_the_runner() {
        let runner = Arc::new(CountingRunner::default());
        let dynamic: Arc<dyn hbot_core::CommandRunner> = runner.clone();
        let (gateway, audit, _dir) = command_fixture(SandboxMode::ReadOnly, &["echo"], dynamic);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let error = gateway
            .call(
                session_id(),
                call("run_command", json!({ "command": "echo" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("只读档必须拒");

        assert!(matches!(*error.error, HbotError::Denied { .. }));
        assert_eq!(error.error.code(), -32005);
        assert_eq!(error.failed_at(), Some(GatewayStep::SandboxMode));
        assert_eq!(approver.count(), 0, "沙箱档拒绝不得弹审批");
        assert_eq!(runner.calls(), 0, "只读档一个进程都不许起");
        assert_eq!(audit.len(), 1);
    }

    /// 父目录缺失不再卡在 PathRealpath：`src/lib/x.ts` 直通并落盘。
    #[tokio::test]
    async fn write_file_with_missing_parents_passes_path_realpath() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(WriteFileTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        let outcome = fixture
            .gateway
            .call(
                session_id(),
                call(
                    "write_file",
                    json!({ "path": "src/lib/x.ts", "content": "v" }),
                ),
                &cancel,
                &approver,
            )
            .await
            .expect("父目录缺失不应在 realpath 步被拒");

        assert!(outcome.steps.contains(&GatewayStep::Execute));
        assert_eq!(outcome.steps.last(), Some(&GatewayStep::Audit));
        assert_eq!(fixture.dir.read("src/lib/x.ts"), "v");
        assert_eq!(approver.count(), 1, "write_file 是 mutation，必须审批");
        let key = outcome.key.expect("mutation 必须有审批键");
        assert!(
            key.canonical_args().contains("src/lib/x.ts"),
            "审批键用归一化后的相对路径：{}",
            key.canonical_args()
        );
    }

    #[tokio::test]
    async fn create_dir_creates_nested_path_through_the_gateway() {
        let fixture = Fixture::new(SandboxMode::WorkspaceWrite, &[Arc::new(CreateDirTool)]);
        let approver = RecordingApprover::approving_once();
        let cancel = CancellationToken::new();
        fixture
            .gateway
            .call(
                session_id(),
                call("create_dir", json!({ "path": "src/lib" })),
                &cancel,
                &approver,
            )
            .await
            .expect("嵌套建目录必须过网关（父目录尚不存在）");

        assert!(fixture.dir.path().join("src").join("lib").is_dir());
        assert_eq!(approver.count(), 1, "create_dir 是 mutation，必须审批");
    }

    /// 审批键含**工具名**：放行 `create_dir <path>` 不会顺带放行 `write_file <同一 path>`。
    #[tokio::test]
    async fn remembered_create_dir_approval_does_not_unlock_write_file() {
        let fixture = Fixture::new(
            SandboxMode::WorkspaceWrite,
            &[Arc::new(CreateDirTool), Arc::new(WriteFileTool)],
        );
        let approver = RecordingApprover::approving_always();
        let cancel = CancellationToken::new();

        // 第一次问一次，记下 (create_dir, {path:lib}, workspace-write)。
        fixture
            .gateway
            .call(
                session_id(),
                call("create_dir", json!({ "path": "lib" })),
                &cancel,
                &approver,
            )
            .await
            .expect("create_dir");
        assert_eq!(approver.count(), 1);

        // 同一工具同一参数：不再问（对照组，证明 remember 本身是生效的）。
        fixture
            .gateway
            .call(
                session_id(),
                call("create_dir", json!({ "path": "lib" })),
                &cancel,
                &approver,
            )
            .await
            .expect("create_dir again");
        assert_eq!(approver.count(), 1, "同键不得重复问");

        // 换工具、**同一个 path 字符串**：必须重新问。
        let error = fixture
            .gateway
            .call(
                session_id(),
                call("write_file", json!({ "path": "lib", "content": "" })),
                &cancel,
                &approver,
            )
            .await
            .expect_err("`lib` 已是目录，写入必须失败");
        assert_eq!(
            approver.count(),
            2,
            "create_dir 的放行不得命中 write_file（审批键含工具名）"
        );
        assert!(
            matches!(*error.error, HbotError::Invalid { .. }),
            "got {:?}",
            error.error
        );
        assert_eq!(error.failed_at(), Some(GatewayStep::Execute));
    }
    /// 只答"允不允许"的能力声明端口。
    struct FakeCapabilities {
        allow: bool,
        calls: std::sync::Mutex<Vec<hbot_core::DeviceCapability>>,
    }

    impl FakeCapabilities {
        fn allowing(allow: bool) -> Arc<Self> {
            Arc::new(Self {
                allow,
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<hbot_core::DeviceCapability> {
            self.calls
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }
    }

    impl CapabilityGate for FakeCapabilities {
        fn authorize(
            &self,
            capability: hbot_core::DeviceCapability,
        ) -> Result<(), hbot_core::DeviceError> {
            if let Ok(mut guard) = self.calls.lock() {
                guard.push(capability);
            }
            if self.allow {
                Ok(())
            } else {
                Err(hbot_core::DeviceError::NotDeclared { capability })
            }
        }
    }

    fn capability_fixture(
        gate: Option<Arc<dyn CapabilityGate>>,
    ) -> (ToolGateway, Arc<RecordingTool>) {
        let dir = TestDir::new("gateway-capability");
        let workspace = dir.workspace();
        let tool = Arc::new(
            RecordingTool::new("needs_ocr")
                .requiring_capabilities(&[hbot_core::DeviceCapability::Ocr]),
        );
        let mut registry = ToolRegistry::new();
        registry.register(tool.clone());
        let mut gateway = ToolGateway::new(
            Arc::new(registry),
            workspace,
            SandboxMode::ReadOnly,
            Arc::new(ApprovalStore::new()),
            Arc::new(RecordingAudit::new()),
        );
        if let Some(gate) = gate {
            gateway = gateway.with_capability_gate(gate);
        }
        (gateway, tool)
    }

    #[tokio::test]
    async fn a_tool_that_needs_a_capability_is_refused_when_the_project_has_not_declared_it() {
        let gate = FakeCapabilities::allowing(false);
        let (gateway, tool) = capability_fixture(Some(gate.clone()));
        let approver = RecordingApprover::approving_always();
        let cancel = CancellationToken::new();
        let error = gateway
            .call(
                session_id(),
                call("needs_ocr", json!({})),
                &cancel,
                &approver,
            )
            .await
            .expect_err("没声明能力必须拒绝");

        assert_eq!(
            error.failed_at(),
            Some(GatewayStep::Capability),
            "必须在能力步就停，实际：{:?}",
            error.steps
        );
        assert!(matches!(*error.error, HbotError::Denied { .. }));
        assert!(error.error.to_string().contains("ocr"));
        assert_eq!(gate.calls(), vec![hbot_core::DeviceCapability::Ocr]);
        assert!(tool.calls().is_empty(), "拒绝后工具不得执行");
        assert_eq!(approver.count(), 0, "能力步在审批之前：不该问用户");
    }

    #[tokio::test]
    async fn a_declared_capability_lets_the_call_reach_the_tool() {
        let gate = FakeCapabilities::allowing(true);
        let (gateway, tool) = capability_fixture(Some(gate.clone()));
        let approver = RecordingApprover::approving_always();
        let cancel = CancellationToken::new();
        // `RecordingTool` 的 schema 要求 `path`：能力步放行之后必须能继续走到参数校验。
        let outcome = gateway
            .call(
                session_id(),
                call("needs_ocr", json!({ "path": "a.txt" })),
                &cancel,
                &approver,
            )
            .await
            .expect("声明了就应放行");

        assert_eq!(outcome.steps.first(), Some(&GatewayStep::Capability));
        assert_eq!(gate.calls(), vec![hbot_core::DeviceCapability::Ocr]);
        assert_eq!(tool.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_tool_that_needs_a_capability_fails_closed_without_a_gate() {
        let (gateway, tool) = capability_fixture(None);
        let approver = RecordingApprover::approving_always();
        let cancel = CancellationToken::new();
        let error = gateway
            .call(
                session_id(),
                call("needs_ocr", json!({})),
                &cancel,
                &approver,
            )
            .await
            .expect_err("没装配端口时，要能力的工具必须拒绝");

        assert_eq!(error.failed_at(), Some(GatewayStep::Capability));
        assert!(error.error.to_string().contains("能力声明端口"));
        assert!(tool.calls().is_empty());
    }
}
