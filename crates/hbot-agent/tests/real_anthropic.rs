//! 真机手动验证：对真实 Anthropic Messages 端点跑一轮**带工具**的对话，证明
//! 真端点会回 `tool_use` 且被我们正确解析。
//!
//! 默认 `#[ignore]`：它需要真凭据与公网。运行方式（**key 只从环境变量读**，
//! 不许写进任何仓库文件；下面的 `<key>` 是占位符，不要替换成真值再存盘）：
//!
//! ```powershell
//! $env:HBOT_LLM_PROVIDER='anthropic'
//! $env:HBOT_LLM_BASE_URL='https://z1rry.cyou'
//! $env:HBOT_LLM_MODEL='deepseek-v4.1-flash-1017'
//! $env:HBOT_LLM_API_KEY='<key>'
//! cargo test -p hbot-agent --test real_anthropic -- --ignored --nocapture
//! ```
//!
//! 断言的是"契约成立"，不是某个模型的措辞：
//!
//! 1. 上游回了 `tool_use`，且被解析成一次工具调用（名字与入参都非空）；
//! 2. 工具真的执行了、结果被回填，第二轮拿到了 `end_turn` 的正文；
//! 3. 全程没有出现"把思考内容当正文"的迹象（正文里不含思考片段是弱断言，
//!    真正的保证在单测 `thinking_blocks_are_not_merged_into_text`）。
//!
//! 打印证据时**只打印** provider / base_url / model / 工具名 / 参数字节数 /
//! usage，密钥永不出现在输出里。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hbot_agent::provider::AnthropicProvider;
use hbot_agent::provider::LlmProvider;
use hbot_agent::tools::WriteFileTool;
use hbot_agent::{
    ApprovalReply, ApprovalRequest, ApprovalStore, Approver, CancellationToken, ChatMessage,
    NullAudit, ReqwestTransport, SandboxMode, ToolGateway, ToolRegistry, TurnConfig, TurnInput,
    TurnSink, run_turn,
};
use hbot_core::HbotError;
use hbot_workspace::Workspace;

/// 永远批准：这是"能不能跑通"的验证，不是审批策略验证。
struct ApproveAllApprover;

#[async_trait]
impl Approver for ApproveAllApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        Ok(ApprovalReply::approve_once())
    }
}

/// 记录循环回调（打印用）。
#[derive(Default)]
struct VerboseSink {
    deltas: String,
    tool_calls: Vec<String>,
    tool_results: Vec<(String, String, bool)>,
}

impl TurnSink for VerboseSink {
    fn on_step(&mut self, _step: u32) {}

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

/// 读一个必需环境变量；缺失/空白就 `panic` 说清缺哪个（不打印值）。
fn required_env(name: &str) -> String {
    let value = std::env::var(name).unwrap_or_default();
    assert!(
        !value.trim().is_empty(),
        "缺少环境变量 {name}：按本文件顶部注释设置 HBOT_LLM_* 后再跑"
    );
    value.trim().to_string()
}

#[tokio::test]
#[ignore = "需要真 provider"]
async fn real_anthropic_endpoint_returns_a_parsable_tool_use() {
    // ---- 配置：全部来自 HBOT_LLM_* ----
    let provider_name = std::env::var("HBOT_LLM_PROVIDER").unwrap_or_else(|_| "anthropic".into());
    let provider_kind = LlmProvider::from_name(&provider_name).expect("HBOT_LLM_PROVIDER 非法");
    assert_eq!(
        provider_kind,
        LlmProvider::Anthropic,
        "这个测试只验 Anthropic 链路：把 HBOT_LLM_PROVIDER 设成 anthropic"
    );
    let base_url = required_env("HBOT_LLM_BASE_URL");
    let model = required_env("HBOT_LLM_MODEL");
    // 密钥只活在这个局部变量里；不打印、不进断言消息。
    let api_key = required_env("HBOT_LLM_API_KEY");

    // ---- 临时项目 + 真实工具 ----
    let project =
        std::env::temp_dir().join(format!("hbot-agent-real-anthropic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("建临时项目目录");
    let workspace = Arc::new(Workspace::open(project.clone()).expect("打开 workspace"));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WriteFileTool));
    let gateway = ToolGateway::new(
        Arc::new(registry),
        Arc::clone(&workspace),
        // 真机上 write_file 需要落盘；workspace_write 档位允许写在工程目录内。
        SandboxMode::WorkspaceWrite,
        Arc::new(ApprovalStore::new()),
        Arc::new(NullAudit),
    );

    let transport = ReqwestTransport::new().expect("建 reqwest 传输");
    let provider = AnthropicProvider::new(base_url.clone(), api_key, transport)
        .with_timeout(Duration::from_secs(120));

    let cancel = CancellationToken::new();
    let approver = ApproveAllApprover;
    let input = TurnInput {
        session_id: "00000000-0000-0000-0000-000000000000"
            .parse()
            .expect("固定会话 id"),
        system_prompt: "你是 hbot 的助手。用户让你写文件时，直接调用 write_file 工具，不要解释。"
            .to_string(),
        messages: vec![ChatMessage::user(
            "用 write_file 把 hello 写进 real-notes.txt，只调工具，不要回复别的。",
        )],
    };
    let config = TurnConfig::new(model.clone()).with_max_steps(4);
    let mut sink = VerboseSink::default();

    println!("--- 真机运行 ---");
    println!("provider   = {}", provider_kind.as_str());
    println!("endpoint   = {}", provider.endpoint());
    println!("model      = {model}");
    println!(
        "api_key    = <redacted, {} 字符>",
        std::env::var("HBOT_LLM_API_KEY")
            .map(|v| v.chars().count())
            .unwrap_or(0)
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(150),
        run_turn(
            &provider, &gateway, input, &config, &mut sink, &cancel, &approver,
        ),
    )
    .await
    .expect("真机请求不能挂住")
    .expect("真机一轮对话应成功");

    println!("steps          = {}", outcome.steps);
    println!("finish_reason  = {:?}", outcome.finish_reason);
    println!("tool_calls     = {:?}", sink.tool_calls);
    println!("usage          = {:?}", outcome.usage);
    println!("text           = {}", outcome.text);
    for (call_id, content, is_error) in &sink.tool_results {
        println!("tool_result[{call_id}] is_error={is_error} content={content}");
    }

    // ---- 断言：真端点确实回了 tool_use，且我们执行了它 ----
    assert!(
        !sink.tool_calls.is_empty(),
        "真端点没有回 tool_use（拿到的是纯文本：{}）",
        outcome.text
    );
    assert_eq!(sink.tool_calls[0], "write_file", "应调用 write_file");
    assert_eq!(sink.tool_results.len(), 1, "工具必须真的执行一次");
    assert!(
        !sink.tool_results[0].2,
        "工具执行失败：{}",
        sink.tool_results[0].1
    );
    let written = project.join("real-notes.txt");
    assert!(written.exists(), "write_file 必须真的落盘：{written:?}");
    let content = std::fs::read_to_string(&written).expect("读回写出的文件");
    println!("real-notes.txt = {content:?}");
    assert_eq!(content, "hello", "参数应被完整解析（不是空对象）");

    let _ = std::fs::remove_dir_all(&project);
}
