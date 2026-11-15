//! 端到端（无网络）：假 `HttpTransport` 回放一段脚本化 SSE，驱动**真实的**
//! `run_turn` 跑完两轮——assistant 文本 → 工具调用 → 工具结果回填 → 最终文本。
//!
//! 这条测试钉住三件事：
//!
//! 1. SSE 分块拼接（含被拆开的 tool_call arguments）能被组装成一次完整调用；
//! 2. `run_turn` 真的把工具结果回填进下一轮请求（第二次请求的 body 里能看到文件内容）；
//! 3. 请求带上了 `Authorization: Bearer …` 且 body 是 `"stream": true`。
//!
//! 它**不**碰网络、不读真实密钥：传输层是本地假实现，密钥是假字符串。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_agent::provider::openai::{HttpRequest, HttpTransport, ResponseSink};
use hbot_agent::tools::ReadFileTool;
use hbot_agent::{
    CancellationToken, ChatMessage, DenyAllApprover, OpenAiProvider, ProviderError, SandboxMode,
    ToolRegistry, TurnConfig, TurnInput, TurnSink, run_turn,
};
use hbot_workspace::Workspace;

/// 临时项目目录（`Drop` 时删除）。
struct TempProject {
    path: PathBuf,
}

impl TempProject {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-agent-sse-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时项目目录");
        Self { path }
    }

    fn write(&self, relative: &str, content: &str) {
        let target = self.path.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("建父目录");
        }
        std::fs::write(target, content).expect("写夹具文件");
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 按脚本回放 SSE 分块的假传输，并记录请求。
///
/// 内部状态放在 `Arc` 里，所以本体可以 `Clone`：一份交给 provider，一份留给断言。
#[derive(Clone)]
struct ScriptedTransport {
    state: Arc<TransportState>,
}

struct TransportState {
    /// 每次 `post` 取走下一个脚本（已按 chunk 切好）。
    scripts: Mutex<VecDeque<Vec<Vec<u8>>>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    fn new(scripts: Vec<Vec<Vec<u8>>>) -> Self {
        Self {
            state: Arc::new(TransportState {
                scripts: Mutex::new(scripts.into()),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    fn request_count(&self) -> usize {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn request(&self, index: usize) -> Option<HttpRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn post(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
        sink: &mut dyn ResponseSink,
    ) -> Result<(), ProviderError> {
        cancel.check().map_err(|_| ProviderError::cancelled())?;
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        let script = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| ProviderError::fatal(hbot_core::HbotError::internal("脚本已用尽")))?;
        for chunk in script {
            cancel.check().map_err(|_| ProviderError::cancelled())?;
            sink.on_chunk(&chunk)?;
        }
        Ok(())
    }
}

/// 记录循环回调的 sink。
#[derive(Default)]
struct RecordingTurnSink {
    steps: Vec<u32>,
    deltas: String,
    tool_calls: Vec<String>,
    tool_results: Vec<(String, String, bool)>,
}

impl TurnSink for RecordingTurnSink {
    fn on_step(&mut self, step: u32) {
        self.steps.push(step);
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.deltas.push_str(delta);
    }

    fn on_tool_call(&mut self, call: &hbot_agent::ToolCallRequest) {
        self.tool_calls.push(call.name.clone());
    }

    fn on_tool_result(&mut self, call_id: &str, output: &hbot_agent::ToolOutput) {
        self.tool_results
            .push((call_id.to_string(), output.content.clone(), output.is_error));
    }
}

/// 把若干 SSE 事件行拼成一段字节流（模拟"一次 TCP 读到多行"）。
fn sse(events: &[&str]) -> Vec<u8> {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(event);
        out.push_str("\n\n");
    }
    out.into_bytes()
}

fn tool_call_chunks() -> Vec<Vec<u8>> {
    // 刻意把 arguments 拆在三个 chunk 里、每个 chunk 又以半行结束：
    // 真实网络就是这样断的，`SseLineBuffer` 必须自己拼回完整行。
    vec![
        sse(&[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","content":"我先读文件。"}}]}"#,
        ]),
        sse(&[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        ]),
        sse(&[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        ]),
        sse(&[
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
        ]),
        b"data: [DONE]\n\n".to_vec(),
    ]
}

fn final_text_chunks() -> Vec<Vec<u8>> {
    vec![
        sse(&[r#"{"id":"c2","choices":[{"index":0,"delta":{"content":"文件里是 hello。"}}]}"#]),
        sse(&[r#"{"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#]),
        b"data: [DONE]\n\n".to_vec(),
    ]
}

fn session_id() -> hbot_core::SessionId {
    "00000000-0000-0000-0000-000000000000"
        .parse()
        .expect("固定会话 id")
}

#[tokio::test]
async fn run_turn_streams_sse_tool_call_then_final_text() {
    let project = TempProject::new("turn");
    project.write("a.txt", "hello\n");
    let workspace = Arc::new(Workspace::open(project.path.clone()).expect("打开 workspace"));

    let transport = ScriptedTransport::new(vec![tool_call_chunks(), final_text_chunks()]);
    let provider = OpenAiProvider::new(
        "http://provider.invalid/v1",
        // 假密钥：只用于断言请求头，不出现在任何断言失败信息里。
        "sk-test-0123456789abcdef",
        transport.clone(),
    );

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadFileTool));
    let gateway = hbot_agent::ToolGateway::new(
        Arc::new(registry),
        Arc::clone(&workspace),
        SandboxMode::WorkspaceWrite,
        Arc::new(hbot_agent::ApprovalStore::new()),
        Arc::new(hbot_agent::NullAudit),
    );

    let cancel = CancellationToken::new();
    let approver = DenyAllApprover;
    let input = TurnInput {
        session_id: session_id(),
        system_prompt: "你是 hbot 的助手。".to_string(),
        messages: vec![ChatMessage::user("a.txt 里是什么？")],
    };
    let config = TurnConfig::new("test-model").with_max_steps(4);
    let mut sink = RecordingTurnSink::default();

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        run_turn(
            &provider, &gateway, input, &config, &mut sink, &cancel, &approver,
        ),
    )
    .await
    .expect("run_turn 不能挂住")
    .expect("两轮都应成功");

    // 两步：第一步拿到工具调用，第二步拿到最终文本。
    assert_eq!(outcome.steps, 2, "应当正好两步");
    assert_eq!(sink.steps, vec![0, 1], "每一步都要通知输出口");
    assert_eq!(sink.tool_calls, vec!["read_file".to_string()]);
    assert_eq!(sink.tool_results.len(), 1, "工具结果要回填");
    let (call_id, content, is_error) = &sink.tool_results[0];
    assert_eq!(call_id, "call_1");
    assert!(!*is_error, "读文件不该失败：{content}");
    assert!(
        content.contains("hello"),
        "工具输出应带上文件内容：{content}"
    );

    assert!(
        sink.deltas.contains("我先读文件。"),
        "增量：{}",
        sink.deltas
    );
    assert!(
        sink.deltas.contains("文件里是 hello。"),
        "增量：{}",
        sink.deltas
    );
    assert_eq!(outcome.usage.total_tokens, 18);

    // 历史：system + user + assistant(tool_calls) + tool + assistant(final)。
    assert_eq!(outcome.messages.len(), 5);
    assert!(matches!(outcome.messages[0], ChatMessage::System { .. }));

    // 两次请求都打到了 /chat/completions，带 Bearer 头且是流式请求。
    assert_eq!(transport.request_count(), 2);
    let first = transport.request(0).expect("第一次请求");
    assert_eq!(first.url, "http://provider.invalid/v1/chat/completions");
    assert!(
        first
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                && value == "Bearer sk-test-0123456789abcdef"),
        "必须带 Authorization: Bearer"
    );
    let body: serde_json::Value = serde_json::from_str(&first.body).expect("请求 body 是 JSON");
    assert_eq!(body["stream"], serde_json::Value::Bool(true));
    assert_eq!(body["model"], "test-model");

    // 第二次请求的 messages 里必须有工具结果（证明"回填 → 再问"这条链通了）。
    let second = transport.request(1).expect("第二次请求");
    let body: serde_json::Value = serde_json::from_str(&second.body).expect("请求 body 是 JSON");
    let messages = body["messages"].as_array().expect("messages 是数组");
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("第二次请求应带工具结果");
    assert_eq!(tool_message["tool_call_id"], "call_1");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap_or_default()
            .contains("hello"),
        "回填内容应含文件正文：{tool_message}"
    );
}
