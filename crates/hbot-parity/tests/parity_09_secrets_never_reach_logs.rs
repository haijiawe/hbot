//! parity #09 — 密钥不进日志。
//!
//! `docs/04-implementation-plan.md` §4 第 9 行「现状」= **缺口**，要求「脱敏测试」。
//! 设计承诺见 `docs/02-architecture.md:583`（「日志/错误/审计统一脱敏；不写 DB 明文」）
//! 与 `:634`（脱敏字段含 `Authorization` / `token` / `apiKey`）。
//!
//! 本用例把一份**真的含密钥**的配置喂进真实的失败路径，然后同时检查四个可观测面：
//!
//! 1. `logs` 表（`SandboxHandler` 失败时写的行）；
//! 2. `Event::RunLog` 事件流（同一份文本的另一个出口）；
//! 3. 配置读取的**错误串**（`read_bot_config` / `ProviderConfig::resolve_with`）；
//! 4. `<data_dir>/agent.json` 与 `bots.config_json` 的落库形状。
//!
//! 查法：原文 + 全小写 + 全大写 + 头 12 字 + 尾 12 字都扫一遍（部分泄漏也算泄漏）。
//!
//! 边界：Agent（LLM）链路本身**不写 logs 表也不发 RunLog**（`crates/hbot-rpc/src/app/agent/`
//! 下没有任何 `append_log`／`Event::RunLog`），所以它这一段只能验到
//! `ProviderConfig` 的 `Debug` 脱敏与错误串——报告里会写清这个可验证性边界。

mod support;

use std::sync::Arc;

use hbot_bot::ConversationRef;
use hbot_code::Limits;
use hbot_core::{BotId, ConversationKind, ProjectId};
use hbot_rpc::EventBus;
use hbot_rpc::app::agent::config::{ENV_API_KEY, ENV_BASE_URL, ENV_MODEL, ProviderConfig};
use hbot_rpc::app::runtime::config::{BotConfig, read_bot_config};
use hbot_store::Bot;
use support::{
    DEADLINE, FakeAdapter, FakeFactory, LogCapture, ReplyRelay, TempDir, blocking, bot_spec,
    group_event, logs_of, open_store, sandbox_executor, seed_project, wait_until, write_bot_script,
};
use uuid::Uuid;

const BOT_UUID: u128 = 0xB09;
const PROJECT_UUID: u128 = 0x5151_0009;
const CONVERSATION: &str = "90009";

/// 被追查的密钥。长度够长（> 8）才可能被真实脱敏器识别，也够独特不会撞上正常文本。
const SECRET: &str = "sk-parity-P0-SECRET-9f3a1c7d2b";

/// 一份「合法但会让 handler 失败」的 bot 脚本：先写一行日志，再抛异常。
/// 失败路径是脱敏最容易漏的地方——错误信息经常把「上下文」一起打出来。
const HANDLER_THROWS: &str = r#"
import { defineBot } from 'hbot';
export default defineBot({
  onMessage(ctx) {
    ctx.log('handler 收到：' + ctx.text);
    if (ctx.text === 'boom') {
      throw new Error('boom');
    }
    ctx.reply('ok');
  },
});
"#;

#[tokio::test]
async fn secrets_never_reach_logs_or_error_strings() {
    if tokio::time::timeout(DEADLINE, case()).await.is_err() {
        panic!("parity_09：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn case() {
    let dir = TempDir::new("09-secrets");
    let store = open_store(&dir);
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    seed_project(&store, project_id, "E:\\hbot-parity-09").await;

    let project_root = dir.join("project");
    write_bot_script(&project_root, HANDLER_THROWS);

    // ---------- 1. 真 bots.json（含 token）→ 真 read_bot_config → 真 AdapterFactory ----------
    let bots_json = format!(
        r#"{{"bots":[{{"id":"{BOT_UUID_LITERAL}","url":"ws://127.0.0.1:1","token":"{SECRET}"}}]}}"#,
        BOT_UUID_LITERAL = bot_id
    );
    dir.write("project/bots.json", &bots_json);

    let config_root = project_root.clone();
    let config: BotConfig = blocking(move || read_bot_config(&config_root, &bot_id))
        .await
        .expect("读 bots.json");

    // 密钥确实被读进来了（否则后面的「没泄漏」是假的——本来就没这个值）。
    assert_eq!(
        config.access_token.as_deref(),
        Some(SECRET),
        "bots.json 里的 token 没读到，后面的脱敏断言没有意义"
    );

    // 走真实的工厂边界：密钥到了「外部世界」手上，但日志里不许出现。
    let adapter = FakeAdapter::new(vec![ConversationRef::new(
        CONVERSATION,
        ConversationKind::Group,
    )]);
    let factory = FakeFactory::new(Arc::clone(&adapter));
    let bot_row = Bot {
        id: bot_id,
        project_id,
        name: "parity".to_owned(),
        platform: "onebot11".to_owned(),
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: 1_700_000_000_000,
    };
    hbot_rpc::app::runtime::AdapterFactory::create(&factory, &bot_row, &config).expect("建适配器");
    assert_eq!(
        factory.seen_access_token().as_deref(),
        Some(SECRET),
        "工厂没有收到 token：这条路径根本没被覆盖到"
    );

    // ---------- 2. 畸形 bots.json：错误串不许把文档内容（含密钥）回显 ----------
    let broken_root = dir.join("broken-project");
    std::fs::create_dir_all(&broken_root).expect("建 broken-project");
    std::fs::write(
        broken_root.join("bots.json"),
        format!(
            r#"{{"bots":[{{"id":"{BOT_UUID_LITERAL}","url":42,"token":"{SECRET}"}}]}}"#,
            BOT_UUID_LITERAL = bot_id
        ),
    )
    .expect("写畸形 bots.json");
    let broken_root_for_read = broken_root.clone();
    let broken = blocking(move || read_bot_config(&broken_root_for_read, &bot_id)).await;
    let broken_error = match broken {
        Ok(_) => panic!("畸形 bots.json（url 是数字）居然读成功了"),
        Err(error) => error.to_string(),
    };
    assert_no_secret(&broken_error, "read_bot_config 的错误串");

    // ---------- 3. Agent 配置：真 resolve_with（密钥经环境注入）+ Debug 脱敏 ----------
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).expect("建 data 目录");
    std::fs::write(
        data_dir.join("agent.json"),
        r#"{"baseUrl":"https://llm.invalid/v1","apiKey":"file-key-should-not-win","model":"m"}"#,
    )
    .expect("写 agent.json");

    let get_env = |name: &str| -> Option<String> {
        match name {
            ENV_API_KEY => Some(SECRET.to_owned()),
            ENV_BASE_URL => Some("https://llm.invalid/v1".to_owned()),
            ENV_MODEL => Some("parity-model".to_owned()),
            _ => None,
        }
    };
    let provider_config =
        ProviderConfig::resolve_with(&data_dir, &get_env).expect("解析 provider 配置");
    assert_eq!(
        provider_config.api_key(),
        SECRET,
        "环境变量里的 apiKey 没被读进来，脱敏断言没有意义"
    );
    let rendered = format!("{provider_config:?}");
    assert!(
        rendered.contains("<redacted>"),
        "ProviderConfig 的 Debug 没有把 apiKey 打码：{rendered}"
    );
    assert_no_secret(&rendered, "ProviderConfig 的 Debug 输出");

    // 同一份密钥喂进一条会失败的解析路径：baseUrl 非法 → 错误串不许带密钥。
    // 注意这里**不再**给 ENV_BASE_URL（环境变量优先于文件，给了就会把文件里的非法值盖掉）。
    std::fs::write(
        data_dir.join("agent.json"),
        r#"{"baseUrl":"ftp://nope","model":"m"}"#,
    )
    .expect("写非法 agent.json");
    let get_env_no_base_url = |name: &str| -> Option<String> {
        match name {
            ENV_API_KEY => Some(SECRET.to_owned()),
            ENV_MODEL => Some("parity-model".to_owned()),
            _ => None,
        }
    };
    let bad = ProviderConfig::resolve_with(&data_dir, &get_env_no_base_url);
    let bad_error = match bad {
        Ok(_) => panic!("非 http(s) 的 baseUrl 居然解析成功了"),
        Err(error) => error.to_string(),
    };
    assert!(
        bad_error.contains("http"),
        "非法 baseUrl 的错误串没提到 http(s)：{bad_error}"
    );
    assert_no_secret(&bad_error, "ProviderConfig::resolve_with 的错误串");

    // ---------- 4. 让真 bot 跑两条消息，然后扫 logs 表 + RunLog 事件 ----------
    //
    // 两条而不是一条：成功那条证明 `ctx.log` 的 info 日志真的进库（扫描不空转），
    // 失败那条证明错误路径（错误串里可能夹带上下文）也不带密钥。
    let events = Arc::new(EventBus::new());
    let capture = LogCapture::spawn(events.subscribe());
    let (handler, replies_rx) = sandbox_executor(
        Arc::clone(&store),
        events,
        bot_id,
        project_root.clone(),
        Limits::new(),
    );
    let supervisor = support::build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        handler,
        bot_spec(bot_id, project_id, "onebot11"),
        |_deps| {},
    );
    let relay = ReplyRelay::spawn(replies_rx, Arc::clone(&supervisor));
    supervisor.start().await.expect("supervisor 启动");

    supervisor
        .inject(group_event(CONVERSATION, "evt-secret-001", "hello"))
        .await
        .expect("入队成功那条");
    wait_until("成功那条回复已经发出", || {
        !adapter.sent().is_empty()
    })
    .await;

    supervisor
        .inject(group_event(CONVERSATION, "evt-secret-002", "boom"))
        .await
        .expect("入队失败那条");
    // 现状记录（2026-11-16 05:56 前后，`crates/hbot-code` 正被另一个并行单元改动：
    // `runtime/host_api.rs` / `runtime/isolate.rs`）：在那种中间态下曾经观察到
    // `capture.lines()` 拿不到两条（ctx.log 的 info 行 与 throw 的 error 行只来了一条），
    // 断言在这里超时。**断言本身不下调**——D2 的验收口径就是"失败必须留痕、成功必须留痕"，
    // 待集成阶段（hbot-code 改动落定后）复验这一条。
    wait_until(
        "失败那条被写进日志（info + error 两条 RunLog）",
        || capture.lines().len() >= 2,
    )
    .await;

    let rows = logs_of(&store, bot_id).await;
    assert!(
        !rows.is_empty(),
        "没有任何日志行，这条用例的扫描是空转（失败路径没被覆盖）"
    );
    // 反空转：证明这段 JS 真的在 QuickJS 里跑过（日志是 handler 自己写的），
    // 而不是「编译就失败了，只留下一条编译错误」。
    assert!(
        rows.iter()
            .any(|row| row.level == "info" && row.line.contains("handler 收到：hello")),
        "handler 自己写的 ctx.log 没进 logs 表（JS 可能压根没跑）：{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.line.contains("boom")),
        "JS 抛出的错误没被记录（失败路径没被覆盖）：{rows:?}"
    );
    for row in &rows {
        assert_no_secret(&row.line, &format!("logs 表的 {} 级日志", row.level));
    }
    for (level, line) in capture.entries() {
        assert_no_secret(&line, &format!("RunLog 事件的 {level:?} 日志"));
    }

    // ---------- 5. 落库形状：bot 行与回复都不许带密钥 ----------
    let stored_bot = blocking({
        let store = Arc::clone(&store);
        move || store.get_bot(&bot_id)
    })
    .await
    .expect("读 bots 行")
    .expect("bots 行存在");
    assert_no_secret(&stored_bot.config_json, "bots.config_json");

    // 回复行：既有「还在 pending 的」，也有「已经发出去的」（发送成功后
    // `list_pending_outbound_for_bot` 就看不见了），所以两路都扫。
    let outbound_ids: Vec<hbot_store::OutboundId> = adapter
        .sent()
        .into_iter()
        .map(|envelope| envelope.id)
        .collect();
    assert!(
        !outbound_ids.is_empty(),
        "假适配器一条回复都没收到，outbound 扫描会空转"
    );
    let mut outbound_rows = Vec::new();
    for id in outbound_ids {
        let row = blocking({
            let store = Arc::clone(&store);
            move || store.get_outbound(&id)
        })
        .await
        .expect("读 outbound")
        .expect("outbound 行存在");
        outbound_rows.push(row);
    }
    outbound_rows.extend(
        blocking({
            let store = Arc::clone(&store);
            move || store.list_pending_outbound_for_bot(&bot_id, 10)
        })
        .await
        .expect("读 pending outbound"),
    );
    for row in &outbound_rows {
        assert_no_secret(&row.text, "outbound.text");
        assert_no_secret(&row.target_json, "outbound.target_json");
        if let Some(reply_to) = &row.reply_to {
            assert_no_secret(reply_to, "outbound.reply_to");
        }
    }

    assert_eq!(
        config.access_token.as_deref(),
        Some(SECRET),
        "配置里的 token 被意外清空了"
    );
    assert!(
        relay.errors().is_empty(),
        "回复搬运报了错：{:?}",
        relay.errors()
    );
    assert!(
        ASSERTED.load(std::sync::atomic::Ordering::SeqCst) >= 4,
        "扫过的可观测面太少（{}），这条用例可能空转通过了",
        ASSERTED.load(std::sync::atomic::Ordering::SeqCst)
    );

    supervisor.stop().await.expect("supervisor 停止");
}

/// 断言一段文本里没有任何形式的密钥。
///
/// 查四种形态：原文、全大写、忽略大小写、以及密钥的头尾各 12 个字符
/// （只回显一半同样算泄漏）。
#[track_caller]
fn assert_no_secret(haystack: &str, where_: &str) {
    ASSERTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let lowered = haystack.to_lowercase();
    let secret_lower = SECRET.to_lowercase();

    assert!(
        !haystack.contains(SECRET),
        "{where_}里出现了密钥原文：{haystack}"
    );
    assert!(
        !haystack.contains(&SECRET.to_uppercase()),
        "{where_}里出现了密钥的全大写形式：{haystack}"
    );
    assert!(
        !lowered.contains(&secret_lower),
        "{where_}里出现了密钥（忽略大小写）：{haystack}"
    );

    // 前后缀：只泄漏一半同样算泄漏。
    let head = &SECRET[..12];
    let tail = &SECRET[SECRET.len() - 12..];
    for (label, fragment) in [("前缀", head), ("后缀", tail)] {
        assert!(
            !lowered.contains(&fragment.to_lowercase()),
            "{where_}里出现了密钥的{label} `{fragment}`：{haystack}"
        );
    }
}

/// 扫过的样本数（用例末尾断言它 > 0：防止「一个可观测面都没读到」时空转通过）。
static ASSERTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
