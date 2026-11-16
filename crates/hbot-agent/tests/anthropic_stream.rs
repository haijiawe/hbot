//! 端到端（无网络）：假 `HttpTransport` 回放**真实抓包**的 Anthropic Messages SSE
//! （`D:\hbot-target\logs\m3resp.txt`，81 行），驱动真实的 `run_turn` 跑完两轮 ——
//! 思考块 + `tool_use` 工具调用 → 工具真的执行 → 工具结果回填 → 最终文本。
//!
//! 这条测试钉住四件事：
//!
//! 1. `thinking_delta` **不**进正文，也不出现在任何推给 UI 的增量里；
//! 2. `input_json_delta` 的三段分片能拼成 `{"path":"notes.txt","content":"hello"}`；
//! 3. `run_turn` 真的执行了工具并把结果回填进第二次请求（`tool_result` 块）；
//! 4. 线上形状：`/v1/messages`、`x-api-key` + `anthropic-version`、顶层 `system`、
//!    `tools[].input_schema`、永远 `"stream": true`、**没有** `tool_choice` / `[DONE]`。
//!
//! 它**不**碰网络、不读真实密钥：传输层是本地假实现，密钥是假字符串。
//!
//! 事件顺序与线上完全一致（含 `event:` 行），只有思考增量被压成一条 ——
//! 真实抓包里有 17 条，这里留 1 条就足以证明"思考内容被丢弃"。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_agent::provider::AnthropicProvider;
use hbot_agent::provider::anthropic::ANTHROPIC_VERSION;
use hbot_agent::provider::openai::{HttpRequest, HttpTransport, ResponseSink};
use hbot_agent::tools::WriteFileTool;
use hbot_agent::{
    ApprovalReply, ApprovalRequest, ApprovalStore, Approver, CancellationToken, ChatMessage,
    NullAudit, ProviderError, SandboxMode, ToolGateway, ToolRegistry, TurnConfig, TurnInput,
    TurnSink, run_turn,
};
use hbot_core::HbotError;
use hbot_workspace::Workspace;
use serde_json::Value;

/// 抓包里那个真实的工具调用 id（回填时必须原样带回）。
const TOOL_CALL_ID: &str = "call_bf503cf7b9644970a13e816d";

/// 临时项目目录（`Drop` 时删除）。
struct TempProject {
    path: PathBuf,
}

impl TempProject {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-agent-anthropic-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时项目目录");
        Self { path }
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path.join(relative)).expect("读回工具写出的文件")
    }

    fn exists(&self, relative: &str) -> bool {
        self.path.join(relative).exists()
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 按脚本回放 SSE 分块的假传输，并记录收到的请求。
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
            .ok_or_else(|| ProviderError::fatal(HbotError::internal("脚本已用尽")))?;
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

/// 永远批准（这个测试关心的是"解析 + 回填"，不是审批策略）。
struct ApproveAllApprover;

#[async_trait]
impl Approver for ApproveAllApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        Ok(ApprovalReply::approve_once())
    }
}

/// 一条 `event:` + `data:` 事件行（与线上完全同形，含 `event:` 行）。
fn event(name: &str, payload: &str) -> String {
    format!("event: {name}\ndata: {payload}\n\n")
}

/// 把事件的 `data:` 负载按 `chunk_size` 切块 —— 模拟 TCP 任意断点。
fn chunks_of(events: &[String], chunk_size: usize) -> Vec<Vec<u8>> {
    let mut stream = String::new();
    for line in events {
        stream.push_str(line);
    }
    stream
        .into_bytes()
        .chunks(chunk_size)
        .map(<[u8]>::to_vec)
        .collect()
}

/// 第一轮：真实抓包的形状 —— 思考块 + `write_file` 工具调用（入参三段分片）。
fn tool_use_events() -> Vec<String> {
    vec![
        event(
            "message_start",
            r#"{"type":"message_start","message":{"type":"message","model":"deepseek-v4.1-flash-1017","usage":{"input_tokens":63,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"billing_usage":{"source":"oai_chat"}},"role":"assistant","id":"chatcmpl-1b05469a","content":[]}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The user wants me to write \"hello\" into notes.txt."}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "content_block_start",
            &format!(
                r#"{{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"{TOOL_CALL_ID}","name":"write_file","input":{{}}}}}}"#
            ),
        ),
        // 入参被切成三段，正是抓包里的那三段。
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"path\": \"notes.txt\""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":", \"content\": \"hello\"}"}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","usage":{"input_tokens":58,"cache_read_input_tokens":256,"output_tokens":97,"billing_usage":{"source":"oai_chat","semantic":"openai","openai_usage":{"prompt_tokens":314,"completion_tokens":97,"total_tokens":411}}},"delta":{"stop_reason":"tool_use"}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

/// 第二轮：工具跑完后模型收尾（纯文本，`end_turn`）。
fn final_text_events() -> Vec<String> {
    vec![
        event(
            "message_start",
            r#"{"type":"message_start","message":{"type":"message","model":"deepseek-v4.1-flash-1017","usage":{"input_tokens":120,"output_tokens":0},"content":[]}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"已写入 notes.txt。"}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","usage":{"output_tokens":9},"delta":{"stop_reason":"end_turn"}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

fn session_id() -> hbot_core::SessionId {
    "00000000-0000-0000-0000-000000000000"
        .parse()
        .expect("固定会话 id")
}

fn header_of<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn run_turn_drives_anthropic_tool_use_and_writes_the_file() {
    let project = TempProject::new("tool-use");
    let workspace = Arc::new(Workspace::open(project.path.clone()).expect("打开 workspace"));

    // 每轮都切成 7 字节的小块：任意切点都不能把一行/一个 UTF-8 字符切断。
    let transport = ScriptedTransport::new(vec![
        chunks_of(&tool_use_events(), 7),
        chunks_of(&final_text_events(), 7),
    ]);
    let provider = AnthropicProvider::new(
        // 刻意**不带** `/v1`：证明 URL 规则补得上。
        "http://provider.invalid",
        // 假密钥：只用于断言请求头。
        "sk-ant-test-0123456789abcdef",
        transport.clone(),
    );

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WriteFileTool));
    let gateway = ToolGateway::new(
        Arc::new(registry),
        Arc::clone(&workspace),
        SandboxMode::WorkspaceWrite,
        Arc::new(ApprovalStore::new()),
        Arc::new(NullAudit),
    );

    let cancel = CancellationToken::new();
    let approver = ApproveAllApprover;
    let input = TurnInput {
        session_id: session_id(),
        system_prompt: "你是 hbot 的助手。".to_string(),
        messages: vec![ChatMessage::user("把 hello 写进 notes.txt，只调工具。")],
    };
    let config = TurnConfig::new("deepseek-v4.1-flash-1017").with_max_steps(4);
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

    // ---- 1. 循环真的跑了工具、并把结果回填 ----
    assert_eq!(outcome.steps, 2, "应当正好两步");
    assert_eq!(sink.steps, vec![0, 1], "每一步都要通知输出口");
    assert_eq!(sink.tool_calls, vec!["write_file".to_string()]);
    assert_eq!(sink.tool_results.len(), 1, "工具结果要回填");
    let (call_id, content, is_error) = &sink.tool_results[0];
    assert_eq!(call_id, TOOL_CALL_ID, "id 必须原样回传上游");
    assert!(!*is_error, "写文件不该失败：{content}");

    // ---- 2. 参数真的被解析成了正确的 JSON（不是空对象） ----
    assert!(
        project.exists("notes.txt"),
        "工具必须真的写出 notes.txt；工具输出：{content}"
    );
    assert_eq!(
        project.read("notes.txt"),
        "hello",
        "内容必须来自 partial_json"
    );

    // ---- 3. 思考块没有混进正文 ----
    assert_eq!(
        sink.deltas, "已写入 notes.txt。",
        "正文只能是 text_delta；思考内容不得出现：{}",
        sink.deltas
    );
    assert!(
        !sink.deltas.contains("The user wants me"),
        "思考内容不得推给 UI"
    );
    // 抓包里 message_delta 的 usage 才是终值（58 / 97），billing_usage 必须被忽略。
    assert_eq!(outcome.usage.prompt_tokens, 58 + 120);
    assert_eq!(outcome.usage.completion_tokens, 97 + 9);
    assert_eq!(outcome.usage.total_tokens, 58 + 120 + 97 + 9);

    // ---- 4. 线上形状 ----
    assert_eq!(transport.request_count(), 2);
    let first = transport.request(0).expect("第一次请求");
    assert_eq!(first.url, "http://provider.invalid/v1/messages");
    assert_eq!(
        header_of(&first, "x-api-key"),
        Some("sk-ant-test-0123456789abcdef")
    );
    assert_eq!(
        header_of(&first, "anthropic-version"),
        Some(ANTHROPIC_VERSION)
    );
    assert_eq!(header_of(&first, "content-type"), Some("application/json"));
    assert!(
        header_of(&first, "authorization").is_none(),
        "Anthropic 用 x-api-key，不发 Bearer"
    );

    let body: Value = serde_json::from_str(&first.body).expect("请求 body 是 JSON");
    assert_eq!(body["stream"], Value::Bool(true), "必须流式");
    assert_eq!(body["model"], "deepseek-v4.1-flash-1017");
    assert!(
        body["max_tokens"].as_u64().is_some_and(|value| value > 0),
        "max_tokens 必填"
    );
    assert_eq!(body["system"], "你是 hbot 的助手。", "system 是顶层字符串");
    let messages = body["messages"].as_array().expect("messages 是数组");
    assert!(
        messages.iter().all(|message| message["role"] != "system"),
        "system 不得混进 messages：{body}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["type"], "text");
    assert_eq!(
        messages[0]["content"][0]["text"],
        "把 hello 写进 notes.txt，只调工具。"
    );
    assert_eq!(body["tools"][0]["name"], "write_file");
    assert!(
        body["tools"][0]["input_schema"].is_object(),
        "工具声明用 input_schema：{}",
        body["tools"][0]
    );
    assert!(
        body["tools"][0].get("function").is_none(),
        "不能发 OpenAI 的 function 包装"
    );
    assert!(
        body.get("tool_choice").is_none(),
        "Anthropic 不发 tool_choice"
    );

    // 第二次请求：assistant 里是 tool_use 块，随后是一条含 tool_result 的 user 消息。
    let second = transport.request(1).expect("第二次请求");
    let body: Value = serde_json::from_str(&second.body).expect("请求 body 是 JSON");
    let messages = body["messages"].as_array().expect("messages 是数组");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("第二次请求应带 assistant 回合");
    let tool_use = assistant["content"]
        .as_array()
        .expect("assistant content 是块数组")
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("assistant 里应有 tool_use 块");
    assert_eq!(tool_use["id"], TOOL_CALL_ID);
    assert_eq!(tool_use["name"], "write_file");
    assert_eq!(tool_use["input"]["path"], "notes.txt");
    assert_eq!(tool_use["input"]["content"], "hello");

    let result_block = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| block["type"] == "tool_result")
        .expect("第二次请求应带 tool_result 块");
    assert_eq!(result_block["tool_use_id"], TOOL_CALL_ID);
    assert!(
        result_block["content"]
            .as_str()
            .unwrap_or_default()
            .contains("notes.txt"),
        "回填内容应来自工具输出：{result_block}"
    );
}

#[tokio::test]
async fn max_tokens_truncation_is_reported_as_length_not_silent_success() {
    let project = TempProject::new("truncated");
    let workspace = Arc::new(Workspace::open(project.path.clone()).expect("打开 workspace"));

    // 上游在 max_tokens 处截断：有 message_stop，但正文只写了一半。
    let truncated = vec![
        event(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"写到一半"}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","usage":{"output_tokens":4096},"delta":{"stop_reason":"max_tokens"}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let transport = ScriptedTransport::new(vec![chunks_of(&truncated, 11)]);
    let provider = AnthropicProvider::new("http://provider.invalid/v1", "k", transport);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WriteFileTool));
    let gateway = ToolGateway::new(
        Arc::new(registry),
        Arc::clone(&workspace),
        SandboxMode::WorkspaceWrite,
        Arc::new(ApprovalStore::new()),
        Arc::new(NullAudit),
    );

    let cancel = CancellationToken::new();
    let approver = ApproveAllApprover;
    let input = TurnInput {
        session_id: session_id(),
        system_prompt: "sys".to_string(),
        messages: vec![ChatMessage::user("写长一点")],
    };
    let config = TurnConfig::new("m").with_max_steps(2);
    let mut sink = RecordingTurnSink::default();

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        run_turn(
            &provider, &gateway, input, &config, &mut sink, &cancel, &approver,
        ),
    )
    .await
    .expect("run_turn 不能挂住")
    .expect("截断也要正常收尾（但 finish_reason 必须说出来）");

    assert_eq!(outcome.steps, 1);
    assert_eq!(
        outcome.finish_reason,
        hbot_agent::FinishReason::Length,
        "max_tokens 必须映射成 Length，不能报成 Stop"
    );
    assert_eq!(outcome.text, "写到一半");
    assert!(sink.tool_calls.is_empty());
}
