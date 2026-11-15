//! Agent 命令：`agent_*`、`edit_*`。
//!
//! ## 这一层只做"翻译"
//!
//! 命令是 UI 与 [`crate::app::agent`] 之间的**薄**适配：解析参数 → 调装配层 →
//! 把结果翻成 [`Response`]。刻意不在这里判审批、不判沙箱、不判路径
//! （`AGENTS.md` §0.6：授权判据只有一份，在 [`hbot_agent::gateway`]）。
//!
//! ## `agent_send` 为什么"立刻返回 ok"
//!
//! 一轮对话可能跑几十步、几分钟。命令若是等回合结束再返回，UI 的输入框就会僵住
//! 且拿不到任何中间进度。所以这里只保证"已接受"：真正的结果通过
//! `agent.delta` / `agent.step` / `agent.tool` / `agent.diff` 事件推回去。
//!
//! ## `edit_apply` 为什么还留着编辑
//!
//! 写盘失败（典型是 `-32003` 文件已被改过）时**不**删除待审编辑：用户的正确反应是
//! 看一眼新内容、重新让 Agent 出 Diff，或手工改回文件后再点一次。删掉就只剩"重跑
//! 一轮"这一条路。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use super::{done, failed};
use crate::app::AppState;
use crate::app::agent;
use crate::dto::{Command, Event, FileChangeKind, Response};
use hbot_core::{ApprovalDecision, ApprovalId, EditId, HbotError, SessionId};

/// `agent_send`：追加用户消息并起一轮，**立刻**返回。
///
/// `session_id` 为 `None` 表示"新建会话"：用 [`IdGenerator`](hbot_core::ports::IdGenerator)
/// 分配 ID 并在 `data: { sessionId }` 里返回，UI 之后带同一个 ID 就接在同一段上下文上。
/// 传了 ID 时行为与 M0 一致（`data: null`），不猜、不覆盖调用方的选择。
pub async fn send(state: &AppState, session_id: Option<SessionId>, text: String) -> Response {
    let agent = Arc::clone(state.agent());
    let config_agent = Arc::clone(&agent);

    // 配置解析要读 `<data_dir>/agent.json`：同步 IO，走阻塞线程池。
    let config = match state.blocking(move || config_agent.resolve_config()).await {
        Ok(config) => config,
        Err(error) => return failed(error),
    };

    // 沙箱档与命令白名单：同一份 `agent.json` / `HBOT_*`，同步读盘 → 走阻塞线程池。
    // 解析失败是**真失败**：不许静默退回默认档（那会把 danger 悄悄降成
    // workspace_write，让用户以为命令能跑却一直被拒）。
    let sandbox_agent = Arc::clone(&agent);
    let sandbox = match state
        .blocking(move || sandbox_agent.resolve_sandbox_config())
        .await
    {
        Ok(sandbox) => sandbox,
        Err(error) => return failed(error),
    };

    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };

    // 项目规则（`<root>/AGENTS.md`）是同步读盘：走阻塞线程池。
    // 读不到不算失败（大多数项目没有这个文件），但要留痕，不要静默吞掉。
    let rules_workspace = Arc::clone(&workspace);
    let project_rules = match state
        .blocking(move || Ok(agent::read_project_rules(&rules_workspace)))
        .await
    {
        Ok(rules) => rules,
        Err(error) => {
            tracing::warn!(error = %error, "读取项目规则失败，本轮不注入 AGENTS.md");
            None
        }
    };

    // 没有会话 ID：新开一个。ID 必须走 `state.ids()`（生成的 ID 只有这一个来源，
    // 直接 `Uuid::new_v4()` 会让测试夹具无法预测 ID）。
    let (session_id, created) = match session_id {
        Some(session_id) => (session_id, false),
        None => (state.ids().session_id(), true),
    };

    match state
        .agent()
        .start_turn(session_id, text, config, sandbox, workspace, project_rules)
    {
        Ok(()) if created => created_session_response(session_id),
        Ok(()) => done(),
        Err(error) => failed(error),
    }
}

/// 新建会话时 `agent_send` 的 `data`：`{ "sessionId": "..." }`。
///
/// 手搭 `Value` 而不是 `json!`：`json!` 对非字面量走 `to_value().unwrap()`，
/// 库代码里不留 unwrap（`AGENTS.md` §5）；`SessionId` 是字符串新类型，拼一个
/// 对象没有失败路径。
fn created_session_response(session_id: SessionId) -> Response {
    let mut data = serde_json::Map::new();
    data.insert(
        "sessionId".to_owned(),
        serde_json::Value::String(session_id.to_string()),
    );
    Response::with_data(serde_json::Value::Object(data))
}

/// `agent_cancel`：取消当前回合（幂等）。
pub async fn cancel(state: &AppState, session_id: SessionId) -> Response {
    state.agent().cancel(session_id);
    done()
}

/// `agent_approve`：给一次等待中的审批投票。
///
/// `always=true` 只登记**这一次完整动作**的规则键（工具 + 规范化参数 + 沙箱档），
/// 由网关的 [`hbot_agent::ApprovalStore`] 落实——本层不碰规则表，
/// 所以不会重犯 `docs/00-assessment.md` P0-7 的"按大类放行"。
pub async fn approve(
    state: &AppState,
    approval_id: ApprovalId,
    decision: ApprovalDecision,
    always: bool,
) -> Response {
    match state
        .agent()
        .resolve_approval(approval_id, decision, always)
    {
        Ok(()) => done(),
        Err(error) => failed(error),
    }
}

/// `edit_apply`：把待审编辑（可只接受部分分块）写进项目。
///
/// `accept_hunks` 为空 = 全部拒绝（协议里写明"空表示全部拒绝"），此时**不写盘**，
/// 直接把编辑丢掉。
pub async fn edit_apply(state: &AppState, edit_id: EditId, accept_hunks: Vec<u32>) -> Response {
    let agent = Arc::clone(state.agent());

    let edit = match agent.peek_edit(edit_id) {
        Ok(edit) => edit,
        Err(error) => return failed(error),
    };

    if accept_hunks.is_empty() {
        agent.forget_edit(edit_id);
        return done();
    }

    let accepted: BTreeSet<u32> = accept_hunks.into_iter().collect();
    let known: BTreeSet<u32> = edit.hunks.iter().map(|hunk| hunk.index).collect();
    if let Some(unknown) = accepted.iter().find(|index| !known.contains(index)) {
        // 未知分块索引是调用方 bug：报错而不是静默忽略（否则用户以为接受了，其实没写）。
        return failed(HbotError::invalid(format!(
            "accept_hunks 里有不存在的分块索引 {unknown}（本编辑只有 {} 块）",
            edit.hunks.len()
        )));
    }

    // 全接受时直接用提案的新内容（免去重建时的边界误差）；部分接受则按块重组。
    let content = if accepted.len() == edit.hunks.len() {
        edit.new_content.clone()
    } else {
        let base = edit.old_content.as_deref().unwrap_or("");
        match hbot_agent::apply_hunks(base, &edit.hunks, &accepted) {
            Ok(content) => content,
            Err(error) => return failed(error),
        }
    };

    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };

    let path = edit.path.clone();
    let base_hash = edit.base_hash.clone();
    let is_new = edit.is_new();

    // 新建文件的"基线"是"文件不存在"：提案之后它若被别人创建出来，同样算冲突。
    if is_new {
        let probe = Arc::clone(&workspace);
        let probe_path = path.clone();
        let exists = state
            .blocking(move || {
                let target = probe.root().join(Path::new(&probe_path));
                Ok(std::fs::symlink_metadata(target).is_ok())
            })
            .await;
        match exists {
            Ok(true) => {
                return failed(HbotError::conflict(format!(
                    "文件 `{path}` 在提案之后已被创建，拒绝覆盖：请重新发起编辑"
                )));
            }
            Ok(false) => {}
            Err(error) => return failed(error),
        }
    }

    let write_workspace = Arc::clone(&workspace);
    let write_path = path.clone();
    let write_content = content.clone();
    let write_hash = base_hash.clone();
    let written = state
        .blocking(move || {
            write_workspace.write_file(
                Path::new(&write_path),
                &write_content,
                write_hash.as_deref(),
            )
        })
        .await;

    match written {
        Ok(file) => {
            agent.forget_edit(edit_id);
            state.events().send(Event::ProjectChanged {
                path: edit.path.clone(),
                kind: if is_new {
                    FileChangeKind::Created
                } else {
                    FileChangeKind::Updated
                },
            });
            tracing::debug!(
                edit_id = %edit_id,
                path = edit.path.as_str(),
                hash = file.hash.as_str(),
                accepted = accepted.len(),
                hunks = edit.hunks.len(),
                "已落盘待审编辑"
            );
            done()
        }
        // 失败时**保留**编辑：`-32003` 表示文件已被改过，用户还能再点一次。
        Err(error) => failed(error),
    }
}

/// `edit_reject`：丢掉一条待审编辑。
pub async fn edit_reject(state: &AppState, edit_id: EditId) -> Response {
    match state.agent().drop_edit(edit_id) {
        Ok(()) => done(),
        Err(error) => failed(error),
    }
}

/// 路由入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::AgentSend { session_id, text } => send(state, session_id, text).await,
        Command::AgentCancel { session_id } => cancel(state, session_id).await,
        Command::AgentApprove {
            approval_id,
            decision,
            always,
        } => approve(state, approval_id, decision, always).await,
        Command::EditApply {
            edit_id,
            accept_hunks,
        } => edit_apply(state, edit_id, accept_hunks).await,
        Command::EditReject { edit_id } => edit_reject(state, edit_id).await,
        other => super::unimplemented(other.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent::{
        AgentManager, MISSING_CONFIG_MESSAGE, ProviderConfig, ProviderFactory,
    };
    use crate::app::test_support::async_temp_state_with_project;
    use hbot_agent::{
        Completion, CompletionRequest, FinishReason, Provider, StreamSink, ToolCallRequest,
        cancelled_error,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 一条"剧本"：先吐若干增量，再返回这个 completion。
    struct ScriptedReply {
        deltas: Vec<String>,
        completion: Completion,
    }

    fn reply(deltas: &[&str], completion: Completion) -> ScriptedReply {
        ScriptedReply {
            deltas: deltas.iter().map(|delta| (*delta).to_string()).collect(),
            completion,
        }
    }

    fn tool_reply(text: &str, call_id: &str, name: &str, args: serde_json::Value) -> ScriptedReply {
        reply(
            &[text],
            Completion {
                content: Some(text.to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: args,
                }],
                usage: hbot_agent::Usage::default(),
                finish_reason: FinishReason::ToolCalls,
            },
        )
    }

    fn text_reply(text: &str) -> ScriptedReply {
        reply(
            &[text],
            Completion {
                content: Some(text.to_string()),
                tool_calls: Vec::new(),
                usage: hbot_agent::Usage::default(),
                finish_reason: FinishReason::Stop,
            },
        )
    }

    /// 按剧本回放的 provider（无网络）。
    struct ScriptedProvider {
        replies: Mutex<Vec<ScriptedReply>>,
        requests: AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(replies: Vec<ScriptedReply>) -> Self {
            Self {
                replies: Mutex::new(replies),
                requests: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
            sink: &mut dyn StreamSink,
        ) -> Result<Completion, HbotError> {
            request.cancel.check()?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut replies = self
                    .replies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if replies.is_empty() {
                    None
                } else {
                    Some(replies.remove(0))
                }
            };
            let Some(reply) = next else {
                // 剧本跑完了还在被问：直接收尾，让测试失败在断言上而不是超时上。
                return Ok(Completion {
                    content: Some("（剧本结束）".to_string()),
                    tool_calls: Vec::new(),
                    usage: hbot_agent::Usage::default(),
                    finish_reason: FinishReason::Stop,
                });
            };
            for delta in &reply.deltas {
                sink.on_text_delta(delta);
            }
            Ok(reply.completion)
        }
    }

    /// 一直等到被取消的 provider：用来证明 `agent_cancel` 真能停住循环。
    struct CancelWaiterProvider;

    #[async_trait::async_trait]
    impl Provider for CancelWaiterProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
            _sink: &mut dyn StreamSink,
        ) -> Result<Completion, HbotError> {
            for _ in 0..600 {
                if request.cancel.is_cancelled() {
                    return Err(cancelled_error());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(HbotError::timeout("fake provider", 3_000))
        }
    }

    /// 一律失败的 provider：用来验证"回合失败也别把会话历史清空"。
    struct FailingProvider;

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
            _sink: &mut dyn StreamSink,
        ) -> Result<Completion, HbotError> {
            Err(HbotError::internal("fake provider 炸了"))
        }
    }

    fn config() -> ProviderConfig {
        ProviderConfig::for_test("http://127.0.0.1:1/v1", "", "test-model", 8)
    }

    /// 把测试用 AgentManager 装进 `AppState`。
    fn with_test_agent(
        state: AppState,
        config: Result<ProviderConfig, HbotError>,
        factory: Box<ProviderFactory>,
    ) -> Arc<AppState> {
        let manager = AgentManager::for_test(
            state.data_dir().to_path_buf(),
            Arc::clone(state.events()),
            Arc::clone(state.ids()),
            Arc::clone(state.clock()),
            config,
            factory,
        );
        Arc::new(state.with_agent(Arc::new(manager)))
    }

    /// 等这一轮结束（失败在超时上，而不是无限等待）。
    async fn wait_idle(state: &AppState, session_id: SessionId) {
        for _ in 0..600 {
            if !state.agent().is_busy(session_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("回合没有在 3 秒内结束");
    }

    fn session() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::nil())
    }

    #[tokio::test]
    async fn agent_send_runs_a_full_turn_over_scripted_provider() {
        let (dir, state, project) = async_temp_state_with_project().await;
        std::fs::write(project.join("a.txt"), "hello\n").expect("写夹具");
        assert!(dir.path().exists());

        let factory: Box<ProviderFactory> = Box::new(|_config: &ProviderConfig| {
            Ok(Box::new(ScriptedProvider::new(vec![
                tool_reply(
                    "先读文件。",
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                ),
                text_reply("文件里是 hello。"),
            ])) as Box<dyn Provider>)
        });
        let state = with_test_agent(state, Ok(config()), factory);

        let mut events = state.events().subscribe();
        let response = send(&state, Some(session()), "a.txt 里是什么？".to_string()).await;
        assert!(response.into_result().is_ok(), "agent_send 必须立刻成功");

        wait_idle(&state, session()).await;

        let mut deltas = String::new();
        let mut steps = Vec::new();
        let mut tools = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                Event::AgentDelta { text, .. } => deltas.push_str(&text),
                Event::AgentStep { step, .. } => steps.push(step),
                Event::AgentTool { call, .. } => tools.push(call),
                _ => {}
            }
        }

        assert!(steps.contains(&0), "必须推过 agent.step：{steps:?}");
        assert_eq!(tools.len(), 1, "只该执行一个工具");
        assert_eq!(tools[0].name, "read_file");
        let result = tools[0].result.as_ref().expect("工具结果");
        assert!(
            result["content"]
                .as_str()
                .unwrap_or_default()
                .contains("hello"),
            "工具结果应带上文件内容：{result}"
        );
        assert!(
            deltas.contains("先读文件。"),
            "增量里应有第一段文本：{deltas}"
        );
        assert!(
            deltas.contains("文件里是 hello。"),
            "增量里应有最终文本：{deltas}"
        );

        // 历史：用户 → 助手（含工具调用）→ 工具结果 → 助手（最终）。
        assert_eq!(state.agent().history_len(session()), 4);
        assert!(!state.agent().is_busy(session()));
    }

    #[tokio::test]
    async fn agent_cancel_stops_the_turn_and_does_not_wedge_the_session() {
        let (_dir, state, _project) = async_temp_state_with_project().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let factory: Box<ProviderFactory> = Box::new(move |_config: &ProviderConfig| {
            let nth = counter.fetch_add(1, Ordering::SeqCst);
            if nth == 0 {
                Ok(Box::new(CancelWaiterProvider) as Box<dyn Provider>)
            } else {
                Ok(
                    Box::new(ScriptedProvider::new(vec![text_reply("第二轮跑完了。")]))
                        as Box<dyn Provider>,
                )
            }
        });
        let state = with_test_agent(state, Ok(config()), factory);

        let mut events = state.events().subscribe();
        send(&state, Some(session()), "开始".to_string())
            .await
            .into_result()
            .expect("起一轮");
        // 等它真的在跑（provider 里在睡觉）。
        for _ in 0..200 {
            if state.agent().is_busy(session()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(state.agent().is_busy(session()), "应该正在跑");

        let cancelled = state.agent().cancel(session());
        assert!(cancelled, "应当确实取消了一个正在跑的回合");
        wait_idle(&state, session()).await;

        let mut notes = String::new();
        while let Ok(event) = events.try_recv() {
            if let Event::AgentDelta { text, .. } = event {
                notes.push_str(&text);
            }
        }
        assert!(notes.contains("已取消"), "取消要留下可见说明：{notes}");

        // 取消不能让会话历史被清空：用户那句话必须还在（否则下一轮模型看不见上下文）。
        assert_eq!(
            state.agent().history_len(session()),
            1,
            "取消后历史里应只剩那条用户消息"
        );

        // 会话没有卡死：第二轮能起来并跑完。
        send(&state, Some(session()), "再来一次".to_string())
            .await
            .into_result()
            .expect("取消后必须还能发");
        wait_idle(&state, session()).await;
        assert!(!state.agent().is_busy(session()));
    }

    /// 回合失败（不是取消）也必须保住历史：用户那句话和之前几轮都在。
    #[tokio::test]
    async fn a_failed_turn_keeps_the_history_intact() {
        let (_dir, state, _project) = async_temp_state_with_project().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let factory: Box<ProviderFactory> = Box::new(move |_config: &ProviderConfig| {
            let nth = counter.fetch_add(1, Ordering::SeqCst);
            if nth == 0 {
                Ok(
                    Box::new(ScriptedProvider::new(vec![text_reply("第一轮好了。")]))
                        as Box<dyn Provider>,
                )
            } else {
                Ok(Box::new(FailingProvider) as Box<dyn Provider>)
            }
        });
        let state = with_test_agent(state, Ok(config()), factory);
        let mut events = state.events().subscribe();

        send(&state, Some(session()), "第一句".to_string())
            .await
            .into_result()
            .expect("第一轮");
        wait_idle(&state, session()).await;
        assert_eq!(state.agent().history_len(session()), 2, "用户 + 助手");

        send(&state, Some(session()), "第二句".to_string())
            .await
            .into_result()
            .expect("第二轮");
        wait_idle(&state, session()).await;

        let mut notes = String::new();
        while let Ok(event) = events.try_recv() {
            if let Event::AgentDelta { text, .. } = event {
                notes.push_str(&text);
            }
        }
        assert!(notes.contains("本轮失败"), "失败要留下可见说明：{notes}");

        // 关键：历史没有被半截回答覆盖掉——第一轮的问答 + 第二句用户消息都在。
        assert_eq!(
            state.agent().history_len(session()),
            3,
            "失败后历史应是 [用户1, 助手1, 用户2]"
        );
    }

    /// 造一条待审编辑：走真实的 `EditProposer`，顺便验证 diff 事件。
    fn propose_edit(
        state: &Arc<AppState>,
        path: &str,
        old: &str,
        new: &str,
        base_hash: Option<String>,
    ) -> EditId {
        let proposal = match base_hash {
            Some(hash) => {
                hbot_agent::ProposedEdit::modify(path, Some(old.to_string()), Some(hash), new)
            }
            None => hbot_agent::ProposedEdit::create(path, new),
        };
        hbot_agent::EditProposer::propose(state.agent().as_ref(), session(), "call_test", proposal)
            .expect("登记待审编辑")
    }

    #[tokio::test]
    async fn edit_apply_with_stale_hash_conflicts_and_keeps_the_file() {
        let (_dir, state, project) = async_temp_state_with_project().await;
        std::fs::write(project.join("a.txt"), "old\n").expect("写夹具");
        let hash = hbot_workspace::hash_bytes(b"old\n");
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        let edit_id = propose_edit(&state, "a.txt", "old\n", "new\n", Some(hash));
        assert_eq!(state.agent().pending_edits(), 1);

        // 提案之后磁盘被改过（模拟用户/另一个进程写盘）。
        std::fs::write(project.join("a.txt"), "changed-by-user\n").expect("改文件");

        let response = edit_apply(&state, edit_id, vec![0]).await;
        let error = response.into_result().expect_err("基线哈希过期必须失败");
        assert_eq!(error.code, -32003, "冲突必须是 -32003");
        assert!(
            error.message.contains("changed on disk") || error.message.contains("已被改动"),
            "冲突说明应指出文件被改过：{}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).expect("读文件"),
            "changed-by-user\n",
            "冲突时绝不能覆盖文件"
        );
        assert_eq!(
            state.agent().pending_edits(),
            1,
            "失败时保留编辑，用户还能重试"
        );
    }

    #[tokio::test]
    async fn edit_apply_writes_the_file_and_emits_project_changed() {
        let (_dir, state, project) = async_temp_state_with_project().await;
        std::fs::write(project.join("a.txt"), "old\n").expect("写夹具");
        let hash = hbot_workspace::hash_bytes(b"old\n");
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        let edit_id = propose_edit(&state, "a.txt", "old\n", "new\n", Some(hash));
        let mut events = state.events().subscribe();

        edit_apply(&state, edit_id, Vec::new())
            .await
            .into_result()
            .expect("空 accept_hunks = 全部拒绝，本身不报错");
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).expect("读文件"),
            "old\n",
            "全部拒绝时不该写盘"
        );
        assert_eq!(state.agent().pending_edits(), 0, "拒绝后编辑应被丢弃");

        let edit_id = propose_edit(
            &state,
            "a.txt",
            "old\n",
            "new\n",
            Some(hbot_workspace::hash_bytes(b"old\n")),
        );
        edit_apply(&state, edit_id, vec![0])
            .await
            .into_result()
            .expect("接受应成功");
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).expect("读文件"),
            "new\n"
        );
        assert_eq!(state.agent().pending_edits(), 0);

        let mut changed = false;
        while let Ok(event) = events.try_recv() {
            if let Event::ProjectChanged { path, kind } = event {
                assert_eq!(path, "a.txt");
                assert_eq!(kind, FileChangeKind::Updated);
                changed = true;
            }
        }
        assert!(changed, "落盘后要推 project.changed");
    }

    #[tokio::test]
    async fn edit_reject_drops_the_edit() {
        let (_dir, state, project) = async_temp_state_with_project().await;
        std::fs::write(project.join("a.txt"), "old\n").expect("写夹具");
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        let edit_id = propose_edit(
            &state,
            "a.txt",
            "old\n",
            "new\n",
            Some(hbot_workspace::hash_bytes(b"old\n")),
        );
        edit_reject(&state, edit_id)
            .await
            .into_result()
            .expect("拒绝");
        assert_eq!(state.agent().pending_edits(), 0);
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).expect("读文件"),
            "old\n"
        );

        // 同一条再拒一次：已经不在了，必须是 -32001 而不是静默成功。
        let error = edit_reject(&state, edit_id)
            .await
            .into_result()
            .expect_err("未知编辑必须报错");
        assert_eq!(error.code, -32001);
    }

    #[tokio::test]
    async fn unknown_edit_apply_is_not_found() {
        let (_dir, state, _project) = async_temp_state_with_project().await;
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        let missing = EditId::from_uuid(uuid::Uuid::nil());
        let error = edit_apply(&state, missing, vec![0])
            .await
            .into_result()
            .expect_err("未知编辑");
        assert_eq!(error.code, -32001);
    }

    #[tokio::test]
    async fn missing_config_is_an_honest_32002() {
        let (dir, state) = crate::app::test_support::temp_state();
        assert!(dir.path().exists());
        let state = with_test_agent(
            state,
            Err(HbotError::invalid(MISSING_CONFIG_MESSAGE)),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        // 没有项目也先报"没配模型"：配置比项目更基础，先报更近的那个原因。
        let response = send(&state, Some(session()), "你好".to_string()).await;
        let error = response.into_result().expect_err("缺配置必须失败");
        assert_eq!(error.code, -32002);
        assert!(
            error.message.contains(MISSING_CONFIG_MESSAGE),
            "文案是契约（含 invalid 前缀）：{}",
            error.message
        );
    }

    #[tokio::test]
    async fn approve_with_unknown_id_is_not_found() {
        let (_dir, state) = crate::app::test_support::temp_state();
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );

        let error = approve(
            &state,
            ApprovalId::from_uuid(uuid::Uuid::nil()),
            ApprovalDecision::Approve,
            false,
        )
        .await
        .into_result()
        .expect_err("未知审批");
        assert_eq!(error.code, -32001);
    }

    #[tokio::test]
    async fn cancel_without_a_session_is_idempotent() {
        let (_dir, state) = crate::app::test_support::temp_state();
        let state = with_test_agent(
            state,
            Ok(config()),
            Box::new(|_config: &ProviderConfig| {
                Ok(Box::new(ScriptedProvider::new(Vec::new())) as Box<dyn Provider>)
            }),
        );
        cancel(&state, session())
            .await
            .into_result()
            .expect("取消是幂等的");
    }

    // 不带 sessionId = 新建会话：核心分配的 id 必须回传，否则 UI 没有"同一个会话"可续，
    // 每句话都会开新会话（历史清零）。带 id 的调用行为不变（不回 data）。
    #[tokio::test]
    async fn agent_send_without_a_session_id_returns_the_new_one() {
        let (_dir, state, project) = async_temp_state_with_project().await;
        assert!(project.exists());

        let factory: Box<ProviderFactory> = Box::new(|_config: &ProviderConfig| {
            Ok(Box::new(ScriptedProvider::new(vec![
                text_reply("第一轮结束。"),
                text_reply("第二轮结束。"),
            ])) as Box<dyn Provider>)
        });
        let state = with_test_agent(state, Ok(config()), factory);

        let data = send(&state, None, "第一轮".to_string())
            .await
            .into_result()
            .expect("新建会话并起一轮必须成功")
            .expect("新建会话必须回 data.sessionId");

        let mut keys: Vec<&str> = data
            .as_object()
            .expect("agent_send 的 data 必须是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["sessionId"], "data 只有 sessionId 一个键");

        let session_id: SessionId = data["sessionId"]
            .as_str()
            .expect("sessionId 必须是字符串（ID 是 transparent newtype）")
            .parse()
            .expect("sessionId 必须是合法的 UUID");
        assert_ne!(session_id, session(), "不能是硬编码的 uuid");

        wait_idle(&state, session_id).await;
        let after_first = state.agent().history_len(session_id);
        assert!(after_first > 0, "这一轮的历史应该落在会话里");

        // 带着同一个 id 再说一句：复用会话，历史接着涨，且不再回 data。
        let second = send(&state, Some(session_id), "第二轮".to_string())
            .await
            .into_result()
            .expect("带 sessionId 的调用行为不变");
        assert!(second.is_none(), "带 sessionId 的调用不该再回 sessionId");

        wait_idle(&state, session_id).await;
        assert!(
            state.agent().history_len(session_id) > after_first,
            "第二轮必须接着同一个会话的历史：{} → {}",
            after_first,
            state.agent().history_len(session_id)
        );
        assert_eq!(
            state.agent().session_count(),
            1,
            "同一个 id 必须复用同一个会话，不能悄悄开第二个"
        );
    }
}
