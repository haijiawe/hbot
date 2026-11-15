//! 审批通道：把网关的"需要审批"变成一次 UI 往返。
//!
//! ## 单一授权路径
//!
//! 这里**不判**"该不该问"——那是 [`hbot_agent::gateway`] 的事（参数 → 存在 → 沙箱 →
//! realpath → 审批规则 → 执行 → 审计）。本模块只做传输：
//!
//! 1. 网关调用 [`Approver::approve`]；
//! 2. 本模块分配 [`ApprovalId`]、登记一个等待槽、推 `approval.request`；
//! 3. `agent_approve` 把 [`ApprovalReply`] 塞回等待槽；
//! 4. 网关拿到答复后**自己**决定要不要把 `(tool, 规范化参数, sandbox_mode)` 记成
//!    "别再问"（[`hbot_agent::ApprovalStore::allow_exactly`]）。
//!
//! 第 4 步留在网关里是有意的：如果在这里"顺手"记一条按工具名的规则，就会重演旧项目
//! 的 P0-7（对 `a.txt` 点过"别再问"后，同一轮里 `b.txt` 也不再问）。
//!
//! ## 取消与超时
//!
//! * 用户取消（`agent_cancel`）⇒ 所有该会话的等待槽立刻按拒绝答复，回合不会卡住；
//! * 无人答复超过 [`APPROVAL_TIMEOUT`] ⇒ 按拒绝处理（**不是**"允许"）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_agent::{ApprovalReply, ApprovalRequest, Approver, Risk};
use hbot_core::{ApprovalDecision, ApprovalId, HbotError, IdGenerator, SessionId};
use tokio::sync::oneshot;

use crate::app::EventBus;
use crate::dto::{Event, RiskLevel};

/// 无人答复时的等待上限。超时按拒绝处理。
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// [`Risk`] → UI 的风险等级（颜色/文案用）。
pub fn risk_level(risk: Risk) -> RiskLevel {
    match risk {
        Risk::Read => RiskLevel::Low,
        // 写文件与联网都是"会、或可能、改变外部状态"，但都可回滚/受限。
        Risk::Write | Risk::Network => RiskLevel::Medium,
        // 起进程、连外部系统：不可逆或越出项目边界。
        Risk::Command | Risk::External => RiskLevel::High,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一个等待答复的审批。
struct PendingApproval {
    session_id: SessionId,
    sender: oneshot::Sender<ApprovalReply>,
}

/// 审批等待槽登记表（进程级）。
///
/// 用 `std::sync::Mutex`：临界区里只有 HashMap 操作，**不跨 await 持锁**
/// （等待答复时锁已释放）。
#[derive(Default)]
pub struct ApprovalInbox {
    entries: Mutex<HashMap<ApprovalId, PendingApproval>>,
}

impl ApprovalInbox {
    /// 空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 正在等待答复的条数（诊断/测试用）。
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// 是否没有等待中的审批。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 登记并等待一次审批答复。
    ///
    /// 返回 `Err(cancelled)` 表示回合被取消；返回 `Err(denied)` 表示等待超时。
    pub async fn request(
        &self,
        ids: &dyn IdGenerator,
        events: &EventBus,
        session_id: SessionId,
        request: &ApprovalRequest,
        cancel: &hbot_agent::CancellationToken,
    ) -> Result<ApprovalReply, HbotError> {
        let approval_id = ids.approval_id();
        let (sender, receiver) = oneshot::channel();
        {
            let mut entries = lock(&self.entries);
            entries.insert(approval_id, PendingApproval { session_id, sender });
        }

        // 先登记再推事件：UI 收到事件后立刻 `agent_approve` 也不会"答复丢失"。
        events.send(Event::ApprovalRequest {
            approval_id,
            tool: request.tool.clone(),
            args: request.args.clone(),
            risk: risk_level(request.risk),
        });

        let reply = tokio::select! {
            reply = receiver => reply,
            () = cancel.cancelled() => {
                self.forget(approval_id);
                return Err(hbot_agent::cancelled_error());
            }
            () = tokio::time::sleep(APPROVAL_TIMEOUT) => {
                self.forget(approval_id);
                return Err(HbotError::denied(format!(
                    "等待审批超时（{} 分钟），按拒绝处理",
                    APPROVAL_TIMEOUT.as_secs() / 60
                )));
            }
        };

        match reply {
            Ok(reply) => Ok(reply),
            // 发送端被丢弃只可能是登记表被清空（会话取消/进程收尾）。
            Err(_) => {
                self.forget(approval_id);
                Err(hbot_agent::cancelled_error())
            }
        }
    }

    /// 落一次用户答复。未知 id 是 `-32001`（不是静默成功）。
    pub fn resolve(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        always: bool,
    ) -> Result<(), HbotError> {
        let pending = lock(&self.entries).remove(&approval_id);
        let Some(pending) = pending else {
            return Err(HbotError::not_found("approval", approval_id.to_string()));
        };

        let reply = match decision {
            ApprovalDecision::Approve => {
                if always {
                    ApprovalReply::approve_always()
                } else {
                    ApprovalReply::approve_once()
                }
            }
            ApprovalDecision::Deny => ApprovalReply::deny(),
        };

        if pending.sender.send(reply).is_err() {
            // 回合已经结束（比如被取消）：用户的答复无处可去，但不是错误。
            tracing::warn!(
                approval_id = %approval_id,
                session_id = %pending.session_id,
                "审批答复无人接收：本轮已结束"
            );
        }
        Ok(())
    }

    /// 把某个会话里所有等待中的审批按拒绝答复，返回处理条数。
    pub fn deny_all(&self, session_id: SessionId) -> usize {
        let mut entries = lock(&self.entries);
        let ids: Vec<ApprovalId> = entries
            .iter()
            .filter(|(_, pending)| pending.session_id == session_id)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(pending) = entries.remove(id) {
                let _ = pending.sender.send(ApprovalReply::deny());
            }
        }
        ids.len()
    }

    /// 丢弃一个等待槽（不答复）。接收端会得到 `cancelled`。
    fn forget(&self, approval_id: ApprovalId) {
        lock(&self.entries).remove(&approval_id);
    }
}

/// 一轮对话的审批实现：把网关的询问转给 [`ApprovalInbox`]。
pub struct SessionApprover {
    inbox: std::sync::Arc<ApprovalInbox>,
    ids: std::sync::Arc<dyn IdGenerator>,
    events: std::sync::Arc<EventBus>,
    session_id: SessionId,
    cancel: hbot_agent::CancellationToken,
}

impl SessionApprover {
    /// 装配一轮对话的审批端口。
    pub fn new(
        inbox: std::sync::Arc<ApprovalInbox>,
        ids: std::sync::Arc<dyn IdGenerator>,
        events: std::sync::Arc<EventBus>,
        session_id: SessionId,
        cancel: hbot_agent::CancellationToken,
    ) -> Self {
        Self {
            inbox,
            ids,
            events,
            session_id,
            cancel,
        }
    }
}

#[async_trait]
impl Approver for SessionApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        self.inbox
            .request(
                self.ids.as_ref(),
                self.events.as_ref(),
                self.session_id,
                request,
                &self.cancel,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hbot_agent::{
        ApprovalPolicy, ApprovalStore, CancellationToken, Risk, ToolCallRequest, ToolGateway,
        ToolRegistry,
    };
    use hbot_core::SandboxMode;
    use hbot_workspace::Workspace;
    use serde_json::{Value, json};

    use super::*;
    use crate::app::test_support::TempDir;
    use crate::app::{EventBus, ids::UuidIds};

    /// 临时项目目录 + 打开好的 workspace。
    fn fixture(label: &str) -> (TempDir, Arc<Workspace>) {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let workspace = Workspace::open(project).expect("打开 workspace");
        (dir, Arc::new(workspace))
    }

    fn read_project(dir: &TempDir, relative: &str) -> String {
        std::fs::read_to_string(dir.path().join("project").join(relative)).expect("读项目文件")
    }

    struct NullAudit;

    impl hbot_agent::AuditSink for NullAudit {
        fn record(&self, _event: &hbot_agent::AuditEvent) {}
    }

    fn request(canonical_args: Value, risk: Risk) -> ApprovalRequest {
        let key = hbot_agent::ApprovalKey::new(
            "write_file",
            &canonical_args,
            SandboxMode::WorkspaceWrite,
        );
        ApprovalRequest {
            key,
            tool: "write_file".to_string(),
            args: canonical_args,
            risk,
            sandbox: SandboxMode::WorkspaceWrite,
        }
    }

    fn session() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::nil())
    }

    #[tokio::test]
    async fn request_is_answered_by_resolve() {
        let inbox = Arc::new(ApprovalInbox::new());
        let ids = Arc::new(UuidIds) as Arc<dyn IdGenerator>;
        let events = Arc::new(EventBus::new());
        let mut receiver = events.subscribe();
        let cancel = CancellationToken::new();
        let req = request(json!({ "path": "a.txt", "content": "x" }), Risk::Write);

        let waiter = {
            let inbox = Arc::clone(&inbox);
            let ids = Arc::clone(&ids);
            let events = Arc::clone(&events);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                inbox
                    .request(ids.as_ref(), events.as_ref(), session(), &req, &cancel)
                    .await
            })
        };

        let event = receiver.recv().await.expect("approval.request");
        let Event::ApprovalRequest {
            approval_id,
            tool,
            args,
            risk,
        } = event
        else {
            panic!("必须是 approval.request");
        };
        assert_eq!(tool, "write_file");
        assert_eq!(args, json!({ "path": "a.txt", "content": "x" }));
        assert_eq!(risk, RiskLevel::Medium);
        assert_eq!(inbox.len(), 1);

        inbox
            .resolve(approval_id, ApprovalDecision::Approve, true)
            .expect("答复");
        let reply = waiter.await.expect("task").expect("approval reply");
        assert!(reply.is_approved());
        assert!(reply.remember);
        assert_eq!(inbox.len(), 0);
    }

    #[tokio::test]
    async fn resolving_an_unknown_id_is_not_found() {
        let inbox = ApprovalInbox::new();
        let error = inbox
            .resolve(
                ApprovalId::from_uuid(uuid::Uuid::nil()),
                ApprovalDecision::Approve,
                false,
            )
            .expect_err("未知审批 id");
        assert_eq!(error.code(), -32001);
    }

    #[tokio::test]
    async fn cancel_answers_pending_approvals_with_denied() {
        let inbox = Arc::new(ApprovalInbox::new());
        let ids = Arc::new(UuidIds) as Arc<dyn IdGenerator>;
        let events = Arc::new(EventBus::new());
        let cancel = CancellationToken::new();
        let req = request(json!({ "path": "a.txt" }), Risk::Write);

        let waiter = {
            let inbox = Arc::clone(&inbox);
            let ids = Arc::clone(&ids);
            let events = Arc::clone(&events);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                inbox
                    .request(ids.as_ref(), events.as_ref(), session(), &req, &cancel)
                    .await
            })
        };
        while inbox.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        let error = waiter.await.expect("task").expect_err("取消");
        assert_eq!(error.code(), -32005);
    }

    #[tokio::test]
    async fn deny_all_closes_every_pending_approval_of_the_session() {
        let inbox = Arc::new(ApprovalInbox::new());
        let ids = Arc::new(UuidIds) as Arc<dyn IdGenerator>;
        let events = Arc::new(EventBus::new());
        let cancel = CancellationToken::new();
        let req = request(json!({ "path": "a.txt" }), Risk::Write);

        let waiter = {
            let inbox = Arc::clone(&inbox);
            let ids = Arc::clone(&ids);
            let events = Arc::clone(&events);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                inbox
                    .request(ids.as_ref(), events.as_ref(), session(), &req, &cancel)
                    .await
            })
        };
        while inbox.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(inbox.deny_all(session()), 1);
        let reply = waiter.await.expect("task").expect("答复");
        assert!(!reply.is_approved());
        assert_eq!(inbox.len(), 0);
    }

    /// P0-7 的回归测试：`always` 只对**同一个**完整动作有效。
    ///
    /// 第一次写 `a.txt` 时用户点"别再问"，网关把这一个
    /// `(write_file, {path:a.txt,content:x}, workspace_write)` 记成允许；
    /// 第二次写**另一个路径**必须重新问人。
    #[tokio::test]
    async fn always_does_not_auto_approve_a_different_action() {
        let (dir, workspace) = fixture("agent-approval-scope");
        let store = Arc::new(ApprovalStore::new());
        let inbox = Arc::new(ApprovalInbox::new());
        let ids = Arc::new(UuidIds) as Arc<dyn IdGenerator>;
        let events = Arc::new(EventBus::new());
        let cancel = CancellationToken::new();
        let registry = Arc::new(ToolRegistry::default_tools());
        let gateway = ToolGateway::new(
            Arc::clone(&registry),
            Arc::clone(&workspace),
            SandboxMode::WorkspaceWrite,
            Arc::clone(&store),
            Arc::new(NullAudit),
        );
        let approver = SessionApprover::new(
            Arc::clone(&inbox),
            Arc::clone(&ids),
            Arc::clone(&events),
            session(),
            cancel.clone(),
        );

        // 一个"用户"：收到审批就点同意，并且第一次点"别再问"。
        let prompts = Arc::new(AtomicUsize::new(0));
        let mut receiver = events.subscribe();
        let ui = {
            let inbox = Arc::clone(&inbox);
            let prompts = Arc::clone(&prompts);
            tokio::spawn(async move {
                while let Ok(event) = receiver.recv().await {
                    if let Event::ApprovalRequest { approval_id, .. } = event {
                        let seen = prompts.fetch_add(1, Ordering::SeqCst) + 1;
                        let always = seen == 1;
                        let _ = inbox.resolve(approval_id, ApprovalDecision::Approve, always);
                    }
                }
            })
        };

        let call = |path: &str, content: &str| ToolCallRequest {
            id: format!("call-{path}"),
            name: "write_file".to_string(),
            arguments: json!({ "path": path, "content": content }),
        };

        let first = gateway
            .call(session(), call("a.txt", "x"), &cancel, &approver)
            .await
            .expect("第一个写入获批");
        assert_eq!(first.approval, Some(ApprovalDecision::Approve));
        assert_eq!(read_project(&dir, "a.txt"), "x");
        // 只记了这一个完整动作。
        assert_eq!(store.len(), 1);
        let remembered = store.keys().into_iter().next().expect("一条规则");
        assert_eq!(remembered.tool(), "write_file");
        assert!(remembered.canonical_args().contains("a.txt"));
        assert_eq!(
            store.policy_for(&remembered),
            ApprovalPolicy::AlwaysAllowThisExactKey(remembered.clone())
        );

        // 第二个路径：必须再问一次（若被自动放行，这里会卡住直到超时）。
        let second = gateway
            .call(session(), call("b.txt", "y"), &cancel, &approver)
            .await
            .expect("第二个写入也必须获批");
        assert_eq!(second.approval, Some(ApprovalDecision::Approve));
        assert_eq!(read_project(&dir, "b.txt"), "y");
        // 两次都问了人：`always` 没有把"写文件"整类放行。
        assert_eq!(prompts.load(Ordering::SeqCst), 2);
        ui.abort();
    }

    /// 同一次写入但内容不同，也要重新问。
    #[tokio::test]
    async fn always_is_scoped_to_the_exact_content_too() {
        let (dir, _workspace) = fixture("agent-approval-content");
        let store = Arc::new(ApprovalStore::new());

        let key = hbot_agent::ApprovalKey::new(
            "write_file",
            &json!({ "path": "a.txt", "content": "x" }),
            SandboxMode::WorkspaceWrite,
        );
        store.allow_exactly(key);

        // 同一路径、不同内容：策略必须是 Ask。
        let other = hbot_agent::ApprovalKey::new(
            "write_file",
            &json!({ "path": "a.txt", "content": "y" }),
            SandboxMode::WorkspaceWrite,
        );
        assert_eq!(store.policy_for(&other), ApprovalPolicy::Ask);
        // 只是登记规则，没有落盘（落盘要用户 edit_apply 或工具执行）。
        assert!(!dir.path().join("project").join("a.txt").exists());
    }

    #[test]
    fn risk_levels_map_from_risk() {
        assert_eq!(risk_level(Risk::Read), RiskLevel::Low);
        assert_eq!(risk_level(Risk::Write), RiskLevel::Medium);
        assert_eq!(risk_level(Risk::Network), RiskLevel::Medium);
        assert_eq!(risk_level(Risk::Command), RiskLevel::High);
        assert_eq!(risk_level(Risk::External), RiskLevel::High);
    }
}
