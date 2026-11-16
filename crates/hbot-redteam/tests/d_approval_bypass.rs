//! D 面：审批绕过（目标 `crates/hbot-agent/src/gateway.rs` + `approvals.rs`）。
//!
//! 审批系统的威胁模型不是"用户会不会点错"，而是**实现有没有把不同类型的动作当成同一个**。
//! 一旦 `always` 的键不够细（漏了参数、漏了工具、漏了沙箱档），就会变成
//! 「用户批准过一次 `notes.txt`，攻击者就能写 `.hbot/config.json`」。
//!
//! 这一面的四条判据：
//! 1. `always` 的键 = 工具 + **规范化后**的完整参数 + 沙箱档。换路径 / 换正文 / 换档位都要重新问；
//! 2. 只读档下写类工具在**审批之前**就被拒（弹了卡也没意义，因为点了也不该写）；
//! 3. 沙箱档降级后不得复用高权限档下的批准；
//! 4. 参数校验必须在执行前完成（缺必填 / 类型错 / enum 越界 / 超上下限）。

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use hbot_agent::tools::fs::{EditFileTool, WriteFileTool};
use hbot_agent::{
    ApprovalReply, ApprovalRequest, ApprovalStore, Approver, CancellationToken, GatewayError,
    GatewayStep, NullAudit, Risk, SandboxMode, Tool, ToolCallRequest, ToolCtx, ToolGateway,
    ToolOutput, ToolRegistry, validate_args,
};
use hbot_core::{HbotError, SessionId};
use hbot_redteam::TempTree;
use hbot_workspace::Workspace;

fn session() -> SessionId {
    SessionId::from_uuid(uuid::Uuid::from_u128(0xD00D))
}

/// 可编排的审批人：记录每次被问到的键（`describe()` 文本），并返回预设回复。
struct ScriptedApprover {
    calls: Mutex<Vec<String>>,
    reply: Mutex<ApprovalReply>,
}

impl ScriptedApprover {
    fn new(reply: ApprovalReply) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            reply: Mutex::new(reply),
        }
    }

    fn always() -> Self {
        Self::new(ApprovalReply::approve_always())
    }

    fn deny() -> Self {
        Self::new(ApprovalReply::deny())
    }

    /// 之后所有询问都改成这个回复。
    fn set_reply(&self, reply: ApprovalReply) {
        *self.reply.lock().expect("审批回复锁") = reply;
    }

    fn count(&self) -> usize {
        self.calls.lock().expect("审批记录锁").len()
    }
}

#[async_trait]
impl Approver for ScriptedApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        self.calls
            .lock()
            .expect("审批记录锁")
            .push(request.key.describe());
        let reply = *self.reply.lock().expect("审批回复锁");
        Ok(reply)
    }
}

fn gateway_with(
    tree: &TempTree,
    level: SandboxMode,
    store: Arc<ApprovalStore>,
    tools: Vec<Arc<dyn Tool>>,
) -> ToolGateway {
    let workspace = Arc::new(Workspace::open(tree.path().to_path_buf()).expect("打开临时工作区"));
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    ToolGateway::new(
        Arc::new(registry),
        workspace,
        level,
        store,
        Arc::new(NullAudit),
    )
}

fn write_file_tool() -> Arc<dyn Tool> {
    Arc::new(WriteFileTool)
}

fn write_call(path: &str, content: &str) -> ToolCallRequest {
    ToolCallRequest {
        id: "call-w".to_owned(),
        name: "write_file".to_owned(),
        arguments: serde_json::json!({ "path": path, "content": content }),
    }
}

// ── 1. `always` 的键必须细到"同一个动作" ───────────────────────────────────

/// 防的是：用户批准过一次写 `notes.txt`，之后任何一次写盘（换路径、换正文）都不再询问 ——
/// 于是攻击者只要让模型把 payload 写进 `.hbot/` 或 `src/` 就完成了持久化。
#[test]
fn d01_always只对完全相同的动作生效() {
    let tree = TempTree::new("d01");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    // 第一次：批准并记住。
    block_on(gateway.call(session(), write_call("notes.txt", "v1"), &cancel, &approver))
        .expect("第一次应当被批准");
    assert_eq!(approver.count(), 1, "第一次必须问一次");
    assert_eq!(tree.read("notes.txt").as_deref(), Some("v1"));

    // 完全相同的动作：命中记忆，不再询问。
    block_on(gateway.call(session(), write_call("notes.txt", "v1"), &cancel, &approver))
        .expect("记忆命中后应当直接放行");
    assert_eq!(approver.count(), 1, "完全相同的动作不该再问一次");

    // 换正文：必须重新问（否则一次批准 = 永久任意写入）。
    block_on(gateway.call(session(), write_call("notes.txt", "v2"), &cancel, &approver))
        .expect("第二次应当被批准");
    assert_eq!(approver.count(), 2, "换了正文却命中同一个 already-allowed");

    // 换路径：必须重新问。
    block_on(gateway.call(session(), write_call("other.txt", "v1"), &cancel, &approver))
        .expect("第三次应当被批准");
    assert_eq!(approver.count(), 3, "换了路径却命中同一个 already-allowed");
}

/// 防的是：`always` 的键只取工具名，于是"批准过一次 `write_file`"等于"以后任意写盘都免审"。
#[test]
fn d02_always不跨工具生效() {
    let tree = TempTree::new("d02");
    tree.write("a.txt", "hello world");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::clone(&store),
        vec![write_file_tool(), Arc::new(EditFileTool)],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    block_on(gateway.call(
        session(),
        write_call("a.txt", "hello world"),
        &cancel,
        &approver,
    ))
    .expect("写文件应当被批准");
    assert_eq!(approver.count(), 1);

    let edit = ToolCallRequest {
        id: "call-e".to_owned(),
        name: "edit_file".to_owned(),
        arguments: serde_json::json!({
            "path": "a.txt",
            "old_string": "hello",
            "new_string": "goodbye",
            "replace_all": false,
        }),
    };
    block_on(gateway.call(session(), edit, &cancel, &approver)).expect("编辑应当被批准");
    assert_eq!(
        approver.count(),
        2,
        "换了一个工具却复用了上一个工具的 always 授权"
    );
}

/// 防的是：规范化只做了一半 —— `notes.txt` / `./notes.txt` / `sub/../notes.txt`
/// 落到同一个文件上，却因为字面不同而各自成为"新动作"（那是过度询问，安全但难用）；
/// 反过来，真正危险的是**不同文件被规范化成同一个键**。这条用例把两边都钉住：
/// 同一个文件的不同写法**允许**共用一个键（它们是同一个动作），
/// 但不同文件**必须**是两个键。
#[test]
fn d03_路径规范化后的同文件可共用但不同文件必须分开() {
    let tree = TempTree::new("d03");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    block_on(gateway.call(session(), write_call("notes.txt", "x"), &cancel, &approver))
        .expect("第一次写应当被批准");
    assert_eq!(approver.count(), 1);

    // 同一个文件的另一种写法：命中同一个键是**正确**的（否则每次写法不同都要问一遍）。
    block_on(gateway.call(
        session(),
        write_call("./notes.txt", "x"),
        &cancel,
        &approver,
    ))
    .expect("同一文件的等价写法应当命中记忆");
    assert_eq!(
        approver.count(),
        1,
        "`./notes.txt` 与 `notes.txt` 是同一个文件，应当共用同一个 always 键"
    );

    // 不同文件：必须是新键。
    block_on(gateway.call(session(), write_call("notes2.txt", "x"), &cancel, &approver))
        .expect("写第二个文件应当被批准");
    assert_eq!(approver.count(), 2, "两个不同文件共用了同一个 always 键");
}

/// 防的是：沙箱档没进键 —— 在 `danger` 下批准过的动作，切回 `workspace_write`（或更低档）
/// 之后仍然免审，等于一次高权限批准永久有效。
#[test]
fn d04_沙箱档变化后不得复用已批准的动作() {
    let tree = TempTree::new("d04");
    let store = Arc::new(ApprovalStore::new());
    let cancel = CancellationToken::new();

    // 先在 danger 档批准并记住。
    let danger = gateway_with(
        &tree,
        SandboxMode::Danger,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    let approver = ScriptedApprover::always();
    block_on(danger.call(session(), write_call("notes.txt", "x"), &cancel, &approver))
        .expect("danger 档应当被批准");
    assert_eq!(approver.count(), 1);

    // 切到 workspace_write 档（更低）：同一个动作必须重新问。
    let lower = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    block_on(lower.call(session(), write_call("notes.txt", "x"), &cancel, &approver))
        .expect("换档后重新批准应当通过");
    assert_eq!(
        approver.count(),
        2,
        "沙箱档变了却复用了上一档的 always 授权（键里没有 sandbox）"
    );
}

// ── 2. 只读档必须在审批之前就拒 ─────────────────────────────────────────────

/// 防的是：只读档下 `write_file` 先弹审批卡，用户点了"通过"就真的写盘 ——
/// 用户以为"我批准了所以可以"，实际是沙箱契约被打穿。
#[test]
fn d05_只读档的写入必须在审批之前被拒() {
    let tree = TempTree::new("d05");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::ReadOnly,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    let error: GatewayError =
        block_on(gateway.call(session(), write_call("notes.txt", "x"), &cancel, &approver))
            .expect_err("只读档必须拒绝写入");

    assert_eq!(
        approver.count(),
        0,
        "只读档还弹了审批卡：点了通过也不该写，所以根本不该问"
    );
    assert_eq!(
        error.failed_at(),
        Some(GatewayStep::SandboxMode),
        "应当在 SandboxMode 这一步被拒，实际步骤：{:?}",
        error.steps
    );
    assert!(!tree.exists("notes.txt"), "只读档却写了盘");
}

/// 防的是：只读档下 `edit_file` 走"改已有文件"的路径绕过写限制
/// （它有独立的最小沙箱要求，漏配就是后门）。
#[test]
fn d06_只读档的编辑同样在审批之前被拒() {
    let tree = TempTree::new("d06");
    tree.write("a.txt", "hello world");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::ReadOnly,
        Arc::clone(&store),
        vec![Arc::new(EditFileTool)],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    let edit = ToolCallRequest {
        id: "call-e".to_owned(),
        name: "edit_file".to_owned(),
        arguments: serde_json::json!({
            "path": "a.txt",
            "old_string": "hello",
            "new_string": "goodbye",
        }),
    };
    let error = block_on(gateway.call(session(), edit, &cancel, &approver))
        .expect_err("只读档必须拒绝编辑");
    assert_eq!(approver.count(), 0, "只读档还问了审批");
    assert_eq!(error.failed_at(), Some(GatewayStep::SandboxMode));
    assert_eq!(
        tree.read("a.txt").as_deref(),
        Some("hello world"),
        "文件被改了"
    );
}

/// 防的是：把"低档位能做的事"当成"高档位也能做"的反向错误 —— 这里验证
/// 升档不会自动继承，且降档后**已拒绝**的记录也不会被复用成放行。
#[test]
fn d07_拒绝的记录不会被复用成放行() {
    let tree = TempTree::new("d07");
    let store = Arc::new(ApprovalStore::new());
    let gateway = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::clone(&store),
        vec![write_file_tool()],
    );
    let approver = ScriptedApprover::deny();
    let cancel = CancellationToken::new();

    let first = block_on(gateway.call(session(), write_call("notes.txt", "x"), &cancel, &approver));
    assert!(first.is_err(), "拒绝后必须失败");
    assert!(!tree.exists("notes.txt"));

    // 同一次会话里第二次（用户这次批准）必须能通过，而不是被上一次的拒绝永久锁死。
    approver.set_reply(ApprovalReply::approve_once());
    block_on(gateway.call(session(), write_call("notes.txt", "x"), &cancel, &approver))
        .expect("一次性批准应当放行");
    assert_eq!(tree.read("notes.txt").as_deref(), Some("x"));
}

// ── 3. 参数闸必须在执行前完成 ───────────────────────────────────────────────

/// 防的是：缺必填 / 类型错（`"12"` 冒充数字）/ 多余参数被放过，工具拿到半截参数后
/// 用默认值做出危险动作（例如 `count` 缺省成"全部删除"）。
#[test]
fn d08_缺必填与类型错必须在执行前拒() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "count": { "type": "integer" },
        },
        "required": ["path", "count"],
        "additionalProperties": false,
    });

    // 缺必填。
    let error = validate_args("probe", &schema, &serde_json::json!({"path": "a"}))
        .expect_err("缺必填必须拒");
    assert!(
        error.to_string().contains("count"),
        "错误应当点名缺的参数：{error}"
    );

    // 类型错：字符串冒充数字。
    let error = validate_args(
        "probe",
        &schema,
        &serde_json::json!({"path": "a", "count": "12"}),
    )
    .expect_err("字符串冒充数字必须拒");
    assert!(
        error.to_string().contains("integer") || error.to_string().contains("类型"),
        "错误应当说明类型不符：{error}"
    );

    // 浮点冒充整数是允许的（JSON 没有 int/float 之分），但 NaN 之类不行。
    validate_args(
        "probe",
        &schema,
        &serde_json::json!({"path": "a", "count": 12.0}),
    )
    .expect("12.0 是合法整数");

    // 多余参数。
    let error = validate_args(
        "probe",
        &schema,
        &serde_json::json!({"path": "a", "count": 1, "force": true}),
    )
    .expect_err("additionalProperties=false 时多余参数必须拒");
    assert!(
        error.to_string().contains("force"),
        "错误应当点名多余参数：{error}"
    );
}

/// 防的是：schema 里写了 `enum` / `minimum` / `maximum`，参数闸却不查 ——
/// 于是 `{"mode":"delete"}`、`count:99` 这类越界值一路进到工具实现里。
/// `run_command` 的 `timeout_ms` 目前靠工具自己再校验一遍才挡住，
/// 但这是**每个工具各自记得**，不是闸门保证的（新增工具漏一句就是缺口）。
///
/// 已确认为缺陷 RT-02，见 `docs/security/red-team-findings.md`；修复后本用例是它的守卫。
#[test]
fn d09_enum与上下限必须在执行前拒() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": ["read", "write"] },
            "count": { "type": "integer", "minimum": 1, "maximum": 3 },
        },
        "required": ["mode", "count"],
    });

    let error = validate_args(
        "probe",
        &schema,
        &serde_json::json!({"mode": "delete", "count": 2}),
    )
    .expect_err("enum 越界必须拒");
    assert!(error.to_string().contains("mode"), "{error}");

    let error = validate_args(
        "probe",
        &schema,
        &serde_json::json!({"mode": "read", "count": 99}),
    )
    .expect_err("超过 maximum 必须拒");
    assert!(error.to_string().contains("count"), "{error}");

    let error = validate_args(
        "probe",
        &schema,
        &serde_json::json!({"mode": "read", "count": 0}),
    )
    .expect_err("低于 minimum 必须拒");
    assert!(error.to_string().contains("count"), "{error}");
}

/// 端到端版本：一个声明了 `enum` 的工具，网关必须在 `ValidateArgs` 这一步拒掉越界值，
/// **不能**先执行再让工具自己检查（工具可能根本不检查，越界值就成了一次真实动作）。
#[test]
fn d10_越界参数不得进入工具执行() {
    struct EnumTool;

    #[async_trait]
    impl Tool for EnumTool {
        fn name(&self) -> &'static str {
            "enum_tool"
        }
        fn description(&self) -> &'static str {
            "红队夹具：声明了 enum 与上下限的工具"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["read", "write"] },
                    "count": { "type": "integer", "minimum": 1, "maximum": 3 },
                },
                "required": ["mode", "count"],
                "additionalProperties": false,
            })
        }
        fn risk(&self) -> Risk {
            Risk::Write
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, HbotError> {
            Ok(ToolOutput::text("ran"))
        }
    }

    let tree = TempTree::new("d10");
    let gateway = gateway_with(
        &tree,
        SandboxMode::WorkspaceWrite,
        Arc::new(ApprovalStore::new()),
        vec![Arc::new(EnumTool)],
    );
    let approver = ScriptedApprover::always();
    let cancel = CancellationToken::new();

    let call = ToolCallRequest {
        id: "call-enum".to_owned(),
        name: "enum_tool".to_owned(),
        arguments: serde_json::json!({ "mode": "delete", "count": 99 }),
    };
    let error: GatewayError = block_on(gateway.call(session(), call, &cancel, &approver))
        .expect_err("越界参数必须在执行前被拒");
    assert_eq!(
        error.failed_at(),
        Some(GatewayStep::ValidateArgs),
        "越界参数应当停在 ValidateArgs，实际：{:?}",
        error.steps
    );
}

// ── 4. 审计必须记下"被拒"这类事件 ───────────────────────────────────────────

/// 防的是：被拒的动作没有审计记录，事后无法回答"那次注入有没有尝试写盘"。
#[test]
fn d11_被拒的动作也要留审计() {
    #[derive(Default)]
    struct CountingAudit {
        denied: Mutex<usize>,
        total: Mutex<usize>,
    }

    impl hbot_agent::AuditSink for CountingAudit {
        fn record(&self, event: &hbot_agent::AuditEvent) {
            *self.total.lock().expect("审计计数锁") += 1;
            if matches!(event.outcome, hbot_agent::AuditOutcome::Denied { .. }) {
                *self.denied.lock().expect("审计计数锁") += 1;
            }
        }
    }

    let tree = TempTree::new("d11");
    let workspace = Arc::new(Workspace::open(tree.path().to_path_buf()).expect("打开临时工作区"));
    let mut registry = ToolRegistry::new();
    registry.register(write_file_tool());
    let audit = Arc::new(CountingAudit::default());
    let gateway = ToolGateway::new(
        Arc::new(registry),
        workspace,
        SandboxMode::ReadOnly,
        Arc::new(ApprovalStore::new()),
        Arc::clone(&audit) as Arc<dyn hbot_agent::AuditSink>,
    );
    let _ = &gateway;

    // 重新构造一次，让 arc 强转后仍能读计数。
    let cancel = CancellationToken::new();
    let approver = ScriptedApprover::always();
    let _ = block_on(gateway.call(session(), write_call("notes.txt", "x"), &cancel, &approver));

    assert_eq!(
        *audit.total.lock().expect("审计计数锁"),
        1,
        "每次调用都应有且仅有一条审计"
    );
    assert_eq!(
        *audit.denied.lock().expect("审计计数锁"),
        1,
        "被拒的动作必须是 Denied 审计，否则事后查不到这次尝试"
    );
}

// ── 夹具 ───────────────────────────────────────────────────────────────────

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("建 tokio 运行时")
        .block_on(future)
}
