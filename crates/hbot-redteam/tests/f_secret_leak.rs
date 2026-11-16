//! F 面：密钥泄漏（跨 crate）。
//!
//! 这一面测的不是"密钥会不会被写进磁盘"，而是**密钥会不会出现在人能看到的地方**：
//! `{:?}` 调试输出、日志行、错误文案、错误链。红队视角下这些地方最危险，因为：
//!
//! * `Debug` 输出经常被 `tracing::debug!(?config)`、`dbg!`、panic 消息顺手带出去；
//! * 错误链会一路冒泡到 UI、RPC 响应、崩溃报告，**每一层都可能被单独格式化**；
//! * 解析错误最容易踩坑：serde 的错误消息里带原文的话，一份写错的 `bots.json`
//!   就把 token 泄进日志了。
//!
//! 统一用假密钥 `sk-test-DEADBEEF`：它足够像真 token，又绝不会被误认为是真的。

use std::path::Path;

use hbot_agent::{SandboxMode, ToolCallRequest};
use hbot_core::{BotId, CommandSpec};
use hbot_redteam::{TempTree, error_chain};
use hbot_rpc::app::agent::config::ProviderConfig;
use hbot_rpc::app::agent::log::{
    REDACTED, describe_arguments, is_secret_key, redact_json, redact_text,
};
use hbot_rpc::app::runtime::config::{bots_file, read_bot_config};

/// 测试专用假密钥。**任何**输出里出现这个串就是泄漏。
const SECRET: &str = "sk-test-DEADBEEF";

fn assert_no_secret(what: &str, text: &str) {
    assert!(!text.contains(SECRET), "{what} 里出现了密钥原文：\n{text}");
}

// ── 1. `Debug` 输出 ─────────────────────────────────────────────────────────

/// 防的是：`tracing::debug!(?config)` 一行就把机器人 access token 打进日志文件，
/// 而日志会被用户打包发给别人排查问题。
#[test]
fn f01_bot_config的debug输出必须脱敏() {
    let tree = TempTree::new("f01");
    let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0xF01));
    std::fs::write(
        bots_file(tree.path()),
        format!(
            r#"{{"bots":[{{"id":"{bot_id}","url":"wss://bot.example/ws","token":"{SECRET}"}}]}}"#
        ),
    )
    .expect("写 bots.json");

    let config = read_bot_config(tree.path(), &bot_id).expect("读取 bot 配置");
    let debug = format!("{config:?}");
    assert_no_secret("BotConfig 的 Debug", &debug);
    assert!(
        debug.contains("redacted"),
        "脱敏应当留下可见标记（否则读日志的人以为 token 是空的）：{debug}"
    );
}

/// 防的是：模型 provider 配置里的 `api_key` 被 `{:?}` 打出来（这条路径几乎一定
/// 会在"连不上模型"的排查日志里被打印）。
#[test]
fn f02_provider_config的debug输出必须脱敏() {
    let tree = TempTree::new("f02");
    let get_env = |name: &str| -> Option<String> {
        match name {
            "HBOT_LLM_API_KEY" => Some(SECRET.to_owned()),
            "HBOT_LLM_BASE_URL" => Some("https://api.example/v1".to_owned()),
            "HBOT_LLM_MODEL" => Some("test-model".to_owned()),
            _ => None,
        }
    };
    let config = ProviderConfig::resolve_with(tree.path(), &get_env).expect("解析 provider 配置");

    let debug = format!("{config:?}");
    assert_no_secret("ProviderConfig 的 Debug", &debug);
    assert!(debug.contains("redacted"), "脱敏应当留下可见标记：{debug}");
}

/// 防的是：错误链被逐层格式化（`{:?}` + `source()`）时，某一层的载荷把密钥带出来。
/// 这里用一条**真的会带载荷**的路径：`CommandSpec` 的 `env` 里放着注入给子进程的密钥。
#[test]
fn f03_command_spec的debug输出必须脱敏() {
    let mut spec = CommandSpec::new("cmd", std::env::temp_dir());
    spec.env = vec![
        ("OPENAI_API_KEY".to_owned(), SECRET.to_owned()),
        ("HBOT_LLM_TOKEN".to_owned(), SECRET.to_owned()),
    ];
    spec.args = vec![format!("Bearer {SECRET}")];

    let debug = format!("{spec:?}");
    assert_no_secret("CommandSpec 的 Debug", &debug);

    // 工具调用参数同样会经 `{:?}` 进日志（provider 层的 `ToolCallRequest` 派生 Debug）。
    let call = ToolCallRequest {
        id: "call-f03".to_owned(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({
            "command": "curl",
            "args": ["-H", format!("Authorization: Bearer {SECRET}")],
        }),
    };
    assert_no_secret("ToolCallRequest 的 Debug", &format!("{call:?}"));
}

/// 防的是：入站消息原文（`payload`）被 `{:?}` 打出来 —— 适配器经常把整条 payload
/// 打进日志，而 payload 里可能带平台下发的 `Authorization`/`token` 字段。
#[test]
fn f04_inbound_message的debug输出必须脱敏() {
    let message = hbot_core::InboundMessage {
        id: hbot_core::MessageId::from_uuid(uuid::Uuid::from_u128(0xF04)),
        bot_id: BotId::from_uuid(uuid::Uuid::from_u128(0xF04)),
        conversation_id: hbot_core::ConversationId::from_uuid(uuid::Uuid::from_u128(0xF04)),
        external_id: "ext-f04".to_owned(),
        sender_id: "user-1".to_owned(),
        payload: serde_json::json!({
            "text": "hi",
            "authorization": format!("Bearer {SECRET}"),
            "token": SECRET,
        }),
        received_at: hbot_core::Timestamp::from_unix_millis(1),
        processed_at: None,
    };
    assert_no_secret("InboundMessage 的 Debug", &format!("{message:?}"));
}

// ── 2. 配置解析错误 ─────────────────────────────────────────────────────────

/// 防的是：用户把 `bots.json` 写错（少个逗号、类型写错），错误消息把**原文**回显，
/// 于是"一条解析错误"就变成了"把 token 贴进日志/工单"。
#[test]
fn f05_配置解析错误不得回显密钥原文() {
    let tree = TempTree::new("f05");
    let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0xF05));
    let cases = [
        // 少一个右括号：JSON 语法错误。
        format!(r#"{{"bots":[{{"id":"{bot_id}","url":"wss://x","token":"{SECRET}""#),
        // 类型写错：token 应该是字符串。
        format!(r#"{{"bots":[{{"id":"{bot_id}","url":"wss://x","token":{{"v":"{SECRET}"}}}}]}}"#),
        // token 混进了一个未知字段旁边（additionalProperties 不严格时也会失败）。
        format!(r#"{{"bots":[{{"id":"{bot_id}","url":"","token":"{SECRET}"}}]}}"#),
    ];

    for (index, content) in cases.iter().enumerate() {
        std::fs::write(bots_file(tree.path()), content).expect("写 bots.json");
        let error = read_bot_config(tree.path(), &bot_id).expect_err("坏配置必须报错");
        let rendered = error_chain(&error);
        assert_no_secret(&format!("第 {index} 个坏配置的错误链"), &rendered);
    }
}

// ── 3. 日志脱敏工具本身 ─────────────────────────────────────────────────────

/// 防的是：脱敏函数只认 `Bearer`/裸 `token` 一种写法，于是 `api_key=xxx`、
/// `access_token=xxx`、`OPENAI_API_KEY=xxx` 这类**下划线连接**的键名原样进日志 ——
/// 而这恰恰是 provider 报错（`401: api_key=...`）与环境变量转储最常见的形状。
#[test]
fn f06_日志脱敏必须覆盖下划线连接的键名() {
    let lines = [
        format!("api_key: {SECRET}"),
        format!("api_key={SECRET}"),
        format!("access_token={SECRET}"),
        format!("access_token: {SECRET}"),
        format!("OPENAI_API_KEY={SECRET}"),
        format!("my_api_key={SECRET}"),
        format!("client_secret={SECRET}"),
    ];
    for line in &lines {
        let redacted = redact_text(line);
        assert_no_secret(&format!("redact_text({line})"), &redacted);
        assert!(
            redacted.contains(REDACTED),
            "脱敏后应当留下 `{REDACTED}` 标记：{redacted}"
        );
    }
}

/// 防的是：脱敏函数整体失效（这条是无下划线写法的正对照，与 `f06` 互补：
/// `f06` 变红说明覆盖不全，这条变红说明脱敏根本没跑）。
#[test]
fn f06b_日志脱敏对无下划线写法必须生效() {
    let lines = [
        format!("Authorization: Bearer {SECRET}"),
        format!("x-api-key: {SECRET}"),
        format!("token={SECRET}"),
        format!("token: {SECRET}"),
        format!("key={SECRET}"),
        format!("password: {SECRET}"),
        format!("credential={SECRET}"),
        format!("cookie: {SECRET}"),
        format!("apiKey={SECRET}"),
        format!("secret=\"{SECRET}\""),
        format!("401: key={SECRET}"),
    ];
    for line in &lines {
        let redacted = redact_text(line);
        assert_no_secret(&format!("redact_text({line})"), &redacted);
        assert!(
            redacted.contains(REDACTED),
            "脱敏后应当留下 `{REDACTED}` 标记：{redacted}"
        );
    }

    // 结构化形态：敏感**键名**整段替换，不管值长什么样。
    let value = serde_json::json!({
        "text": "hi",
        "authorization": format!("Bearer {SECRET}"),
        "apiKey": SECRET,
        "nested": { "token": SECRET, "note": format!("401: token={SECRET}") },
    });
    let redacted = redact_json(&value);
    assert_no_secret("redact_json", &redacted.to_string());

    // 键名判定：`is_secret_key` 走的是 `contains`，所以下划线写法在这里是**能**命中的
    // （这也说明 RT-08 只是自由文本路径的缺口，不是全局失效）。
    for key in [
        "token",
        "access_token",
        "apiKey",
        "api_key",
        "OPENAI_API_KEY",
        "client_secret",
        "secret",
        "password",
    ] {
        assert!(is_secret_key(key), "`{key}` 应当被判为敏感键");
    }
    assert!(!is_secret_key("keywords"), "`keywords` 不该被判为敏感键");
    assert!(
        !is_secret_key("tokenizer"),
        "`tokenizer` 不该被判为敏感键（误报会让日志失去可读性）"
    );
}

/// 防的是：工具参数进日志时只截断不脱敏 —— `run_command` 的 `args` 里放
/// `Authorization: Bearer xxx` 是最常见的一手泄漏。
#[test]
fn f07_工具参数进日志必须脱敏() {
    let arguments = serde_json::json!({
        "command": "curl",
        "args": ["-H", format!("Authorization: Bearer {SECRET}"), "-d", format!("token={SECRET}")],
        "env": { "OPENAI_API_KEY": SECRET },
    });
    let described = describe_arguments(&arguments);
    assert_no_secret("describe_arguments", &described);
    assert!(
        described.contains("curl"),
        "脱敏不能把非敏感内容也抹掉（否则日志没用）：{described}"
    );
}

// ── 4. 审批/网关错误文案 ────────────────────────────────────────────────────

/// 防的是：审批被拒时，错误文案把**被拒动作的参数原文**带出去。这是一条真实泄漏路径：
/// 拒绝消息会进 UI、进 RPC 响应、进日志，而参数里可能正好是被窃取的密钥。
#[test]
fn f08_审批拒绝文案不得回显参数原文() {
    use std::sync::Arc;

    use async_trait::async_trait;
    use hbot_agent::tools::fs::WriteFileTool;
    use hbot_agent::{
        ApprovalReply, ApprovalRequest, ApprovalStore, Approver, CancellationToken, NullAudit,
        ToolGateway, ToolRegistry,
    };
    use hbot_core::HbotError;
    use hbot_workspace::Workspace;

    struct Deny;
    #[async_trait]
    impl Approver for Deny {
        async fn approve(&self, _request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
            Ok(ApprovalReply::deny())
        }
    }

    let tree = TempTree::new("f08");
    let workspace = Arc::new(Workspace::open(tree.path().to_path_buf()).expect("打开工作区"));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WriteFileTool));
    let gateway = ToolGateway::new(
        Arc::new(registry),
        workspace,
        SandboxMode::WorkspaceWrite,
        Arc::new(ApprovalStore::new()),
        Arc::new(NullAudit),
    );

    let call = ToolCallRequest {
        id: "call-f08".to_owned(),
        name: "write_file".to_owned(),
        arguments: serde_json::json!({
            "path": "notes.txt",
            "content": format!("API_KEY={SECRET}"),
        }),
    };
    let cancel = CancellationToken::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("建运行时");
    let error = runtime
        .block_on(gateway.call(
            hbot_core::SessionId::from_uuid(uuid::Uuid::from_u128(0xF08)),
            call,
            &cancel,
            &Deny,
        ))
        .expect_err("审批被拒必须失败");

    assert_no_secret("审批拒绝文案", &error.describe());
    assert_no_secret("网关错误链", &error_chain(&error));
}

/// 防的是：`Path`/文件系统错误在**每一层**都被格式化一次时，绝对路径与文件名
/// 泄漏项目在磁盘上的位置（红队视角下这是侦察信息：用户名、项目名、目录结构）。
///
/// 实测（本机 Windows）：`read_file("does-not-exist.txt")` 的错误链三层都写着
/// `\\?\C:\Users\<用户名>\AppData\Local\Temp\<测试目录>\does-not-exist.txt`。
/// 这不是「只在诡异输入上才泄漏」，普通的不存在路径就会泄漏 → RT-01。
/// RT-01 已修：错误链里现在只剩相对项目根的 `does-not-exist.txt`（不含根前缀、
/// `\\?\` 或用户名），本用例守着这条。
#[test]
fn f09_路径错误不得泄漏绝对路径() {
    let tree = TempTree::new("f09");
    let workspace = hbot_workspace::Workspace::open(tree.path().to_path_buf()).expect("打开工作区");
    let error = workspace
        .read_file(Path::new("does-not-exist.txt"), 4096)
        .expect_err("读不存在的文件必须报错");
    let rendered = error_chain(&error);
    hbot_redteam::assert_no_path_leak(&rendered, tree.path());
}
