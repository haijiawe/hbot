//! QQ 官方适配器的接线测试：`bots.json` → `PlatformRouter` → 被动回复。
//!
//! **不连真网络**，也不起真 QQ 网关。被测的是三件在接线里最容易写错的事：
//!
//! 1. `bots.json` 的 QQ 字段（`appId` / `clientSecret`）能解析，且 `Debug` 里
//!    **看不到 `clientSecret` 明文**（它会被打进日志与错误上下文）；
//! 2. 校验不许放宽：OneBot 缺 `url`、QQ 缺 `clientSecret`、平台值写错 —— 三种都是
//!    `-32002`，消息里点名缺的字段 / 列出允许值（**不许静默当成 OneBot**）；
//! 3. `PlatformRouter` 按 `bots.platform` 精确匹配造出**对应**的适配器，`bot_start`
//!    默认工厂就是它（不是硬编码的 OneBot）；
//! 4. 被动回复必须把入站消息 ID（`d.id`，在信封里是 `reply_to`）原样当 `msg_id`
//!    传下去；`reply_to` 为 `None` 时 QQ 官方**发不出去**，所以必须是 `Err`
//!    （`-32005`），不许伪造成功、不许静默丢弃。
//!
//! 真 store（sqlite）与真 `AppState` 打开在临时目录里：路由那两条用例要证明的正是
//! "默认工厂从 store 的行上读平台"，而不是"两个结构体名不一样"。
//!
//! `QqSender` 是 `pub(crate)` 且必然发 HTTPS，所以第 4 组用例走
//! [`QqAdapter::with_reply_port`] 注入假出站端口（测试缝在 `hbot-rpc` 侧，
//! `hbot-adapters` 的公开 API 一个字节都没动）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hbot_adapters::AdapterError;
use hbot_adapters::SystemClock;
use hbot_adapters::qq::QqConfig;
use hbot_bot::{BotAdapter, ConversationRef, OutboundEnvelope};
use hbot_core::{BotId, ConversationId, ConversationKind, HbotError, OutboundId, ProjectId};
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::config::{BotConfig, read_bot_config};
use hbot_rpc::app::runtime::qq::{self, PassiveReply, QqAdapter};
use hbot_rpc::app::runtime::{AdapterFactory, onebot};

const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_UUID: &str = "11111111-1111-4111-8111-111111111111";
/// 假的 AppSecret。用可搜索的字符串，才能断言它没出现在 `Debug` 里。
const FAKE_SECRET: &str = "qq-client-secret-must-not-leak";

// ---------------------------------------------------------------------------
// 临时目录（与 `bot_runtime.rs` / `debug_commands.rs` 同一套做法）
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-qqrt-{label}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 写一份 `bots.json`（`contents` 是完整 JSON 文本）。
    fn write_bots(&self, contents: &str) -> &Self {
        std::fs::write(self.path.join("bots.json"), contents).expect("写 bots.json");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // sqlite 连接还开着时 Windows 删不掉目录：清理失败不影响断言结果。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

fn project_id() -> ProjectId {
    ProjectId::from_uuid(uuid::Uuid::parse_str(PROJECT_UUID).expect("project uuid"))
}

fn bot_row(id: BotId, platform: &str) -> hbot_store::Bot {
    hbot_store::Bot {
        id,
        project_id: project_id(),
        name: "qq-bot".to_owned(),
        platform: platform.to_owned(),
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: 0,
    }
}

/// 只保留消息文本（`HbotError` 的 `Display` 带 `invalid:` 前缀，断言里别依赖它）。
fn text_of(error: &HbotError) -> String {
    error.to_string()
}

// ---------------------------------------------------------------------------
// 1. QQ 条目能解析，且 Debug 里没有 secret 明文
// ---------------------------------------------------------------------------

#[test]
fn qq_entry_parses_and_debug_hides_the_secret() {
    let dir = TempDir::new("parse-qq");
    dir.write_bots(&format!(
        r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"qq_official",
             "appId":"10001","clientSecret":"{FAKE_SECRET}"}}]}}"#
    ));

    let config = read_bot_config(dir.path(), &bot_id()).expect("qq_official 条目应该能解析");

    // QQ 官方没有用户可配的 ws 地址：不许退化成空串（空串会一路走到 connect）。
    assert_eq!(config.ws_url, None, "QQ 平台不该有 ws_url");
    assert_eq!(config.app_id.as_deref(), Some("10001"));
    assert_eq!(config.client_secret.as_deref(), Some(FAKE_SECRET));
    assert_eq!(config.effective_platform(), qq::PLATFORM);

    let printed = format!("{config:?}");
    assert!(
        !printed.contains(FAKE_SECRET),
        "Debug 输出里出现了 clientSecret 明文：{printed}"
    );
    assert!(
        printed.contains("<redacted>"),
        "secret 要显式打码，而不是整个字段消失：{printed}"
    );
    // appId 不是密钥，允许明文（但仍然只在 debug 级别打日志）。
    assert!(printed.contains("10001"), "{printed}");
}

// ---------------------------------------------------------------------------
// 2. OneBot 缺 url → -32002 且点名 url
// ---------------------------------------------------------------------------

#[test]
fn onebot_entry_without_url_is_rejected() {
    let dir = TempDir::new("onebot-no-url");
    dir.write_bots(&format!(
        r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"onebot11"}}]}}"#
    ));

    let error = read_bot_config(dir.path(), &bot_id()).expect_err("缺 url 必须失败");
    assert_eq!(error.code(), -32002, "{error}");
    assert!(
        text_of(&error).contains("url"),
        "错误消息要点名缺 url：{error}"
    );

    // 不写 `platform` 等价于 `onebot11`：同样必须要求 url（缺省不是"随便连"）。
    let dir = TempDir::new("onebot-default-no-url");
    dir.write_bots(&format!(r#"{{"bots":[{{"id":"{BOT_UUID}"}}]}}"#));
    let error = read_bot_config(dir.path(), &bot_id()).expect_err("缺省就是 onebot11，也要 url");
    assert_eq!(error.code(), -32002, "{error}");
    assert!(text_of(&error).contains("url"), "{error}");
}

// ---------------------------------------------------------------------------
// 3. QQ 缺 clientSecret / appId → -32002 且点名那个字段
// ---------------------------------------------------------------------------

#[test]
fn qq_entry_without_client_secret_is_rejected() {
    let dir = TempDir::new("qq-no-secret");
    dir.write_bots(&format!(
        r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"qq_official","appId":"10001"}}]}}"#
    ));

    let error = read_bot_config(dir.path(), &bot_id()).expect_err("缺 clientSecret 必须失败");
    assert_eq!(error.code(), -32002, "{error}");
    assert!(
        text_of(&error).contains("clientSecret"),
        "错误消息要点名 clientSecret：{error}"
    );

    let dir = TempDir::new("qq-no-appid");
    dir.write_bots(&format!(
        r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"qq_official","clientSecret":"{FAKE_SECRET}"}}]}}"#
    ));
    let error = read_bot_config(dir.path(), &bot_id()).expect_err("缺 appId 必须失败");
    assert_eq!(error.code(), -32002, "{error}");
    assert!(
        text_of(&error).contains("appId"),
        "错误消息要点名 appId：{error}"
    );
}

// ---------------------------------------------------------------------------
// 4. 未知平台 → -32002 且同时列出两个允许值（不许静默当 OneBot）
// ---------------------------------------------------------------------------

#[test]
fn unknown_platform_is_rejected_and_lists_allowed_values() {
    let dir = TempDir::new("unknown-platform");
    // telegram 带一个看起来合法的 url：如果实现"静默降成 OneBot"，这里就解析成功了。
    dir.write_bots(&format!(
        r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"telegram",
             "url":"ws://127.0.0.1:3001"}}]}}"#
    ));

    let error = read_bot_config(dir.path(), &bot_id()).expect_err("未知平台必须失败");
    assert_eq!(error.code(), -32002, "{error}");
    let text = text_of(&error);
    assert!(text.contains(onebot::PLATFORM), "要列出 onebot11：{error}");
    assert!(text.contains(qq::PLATFORM), "要列出 qq_official：{error}");
}

// ---------------------------------------------------------------------------
// 5. 路由：默认工厂按 store 里的平台选适配器
// ---------------------------------------------------------------------------

/// 真 `AppState`（真 sqlite）的一个薄壳：只用到 store 与默认适配器工厂。
fn open_state(dir: &TempDir) -> AppState {
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("创建项目目录");
    let state = AppState::open(
        dir.path().join("data"),
        Arc::new(SystemClock),
        Arc::new(UuidIds),
    )
    .expect("打开 AppState");
    state
        .store()
        .insert_project(&hbot_store::Project {
            id: project_id(),
            name: "qq-project".to_owned(),
            path: project.display().to_string(),
            created_at: 0,
        })
        .expect("插入 project");
    state
}

/// 一个测试里可能要种好几个 bot：`bots.id` 是主键，所以每个都要新 id。
fn seed_bot(state: &AppState, platform: &str) -> hbot_store::Bot {
    static SEEDED: AtomicU64 = AtomicU64::new(0);
    let seq = SEEDED.fetch_add(1, Ordering::Relaxed);
    let id = BotId::from_uuid(uuid::Uuid::from_u128(
        0x2222_0000_0000_4000_8000_0000_0000_0000 + u128::from(seq),
    ));
    let row = bot_row(id, platform);
    state.store().insert_bot(&row).expect("插入 bot");
    state
        .store()
        .get_bot(&row.id)
        .expect("读 bots")
        .expect("刚插入的 bot 一定读得到")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_picks_the_adapter_by_platform() {
    let dir = TempDir::new("router");
    let state = open_state(&dir);
    let factory = state.adapter_factory();

    // onebot11 → OneBot 适配器（`create` 只校验配置：对没人监听的地址也返回 Ok，
    // 这是 `tests/bot_runtime.rs` 钉住的既有契约）。
    let stored = seed_bot(&state, onebot::PLATFORM);
    let adapter = factory
        .create(&stored, &BotConfig::new("ws://127.0.0.1:1"))
        .expect("onebot11 应该造出 OneBot 适配器");
    assert_eq!(
        adapter.id(),
        onebot::PLATFORM,
        "router 给 onebot11 的适配器 id 不对"
    );

    // qq_official → QQ 官方适配器（同样不碰网络：凭据在 `create` 里只做校验）。
    let stored = seed_bot(&state, qq::PLATFORM);
    let config = BotConfig::qq("10001", FAKE_SECRET);
    let adapter = factory
        .create(&stored, &config)
        .expect("qq_official 应该造出 QQ 官方适配器");
    assert_eq!(
        adapter.id(),
        qq::PLATFORM,
        "router 给 qq_official 的适配器 id 不对"
    );

    // 未知平台：路由必须报错并列出允许值。默认工厂如果是硬编码的 OneBot，
    // 这里会得到"不是 onebot11"那句 —— 断言同时看两个平台名就能分辨。
    let stored = seed_bot(&state, "telegram");
    // `Arc<dyn BotAdapter>` 没有 `Debug`，所以不用 `expect_err`，直接 match。
    let error = match factory.create(&stored, &BotConfig::new("ws://127.0.0.1:1")) {
        Ok(adapter) => panic!("未知平台不许造出适配器，实际拿到 id = `{}`", adapter.id()),
        Err(error) => error,
    };
    assert_eq!(error.code(), -32002, "{error}");
    let text = text_of(&error);
    assert!(text.contains(onebot::PLATFORM), "{error}");
    assert!(text.contains(qq::PLATFORM), "{error}");

    // 两个具体工厂都还在，并且各自仍然校验平台（测试注入能力保留）。
    let stored_onebot = seed_bot(&state, onebot::PLATFORM);
    let error =
        match qq::QqAdapterFactory.create(&stored_onebot, &BotConfig::new("ws://127.0.0.1:1")) {
            Ok(adapter) => panic!(
                "QQ 工厂不能拿 OneBot 的 bot 启动，实际拿到 `{}`",
                adapter.id()
            ),
            Err(error) => error,
        };
    assert_eq!(error.code(), -32002, "{error}");

    let stored_qq = seed_bot(&state, qq::PLATFORM);
    let error = match onebot::OneBotAdapterFactory
        .create(&stored_qq, &BotConfig::qq("10001", FAKE_SECRET))
    {
        Ok(adapter) => panic!(
            "OneBot 工厂不能拿 QQ 的 bot 启动，实际拿到 `{}`",
            adapter.id()
        ),
        Err(error) => error,
    };
    assert_eq!(error.code(), -32002, "{error}");
}

// ---------------------------------------------------------------------------
// 6. bots.json 显式 platform 与库里的列不一致 → 报错（不猜哪边对）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_rejects_platform_mismatch_between_store_and_bots_json() {
    let dir = TempDir::new("router-mismatch");
    let state = open_state(&dir);
    let stored = seed_bot(&state, onebot::PLATFORM);

    let mut config = BotConfig::new("ws://127.0.0.1:1");
    config.platform = Some(qq::PLATFORM.to_owned());

    let error = match state.adapter_factory().create(&stored, &config) {
        Ok(adapter) => panic!("平台不一致必须失败，实际拿到 `{}`", adapter.id()),
        Err(error) => error,
    };
    assert_eq!(error.code(), -32002, "{error}");
    let text = text_of(&error);
    assert!(text.contains("不一致"), "要说清是两边对不上：{error}");
    assert!(text.contains(onebot::PLATFORM), "{error}");
    assert!(text.contains(qq::PLATFORM), "{error}");
}

// ---------------------------------------------------------------------------
// 7/8. 被动回复：reply_to 必须变成 msg_id；缺失就是真错误
// ---------------------------------------------------------------------------

/// 记录参数的假出站端口（生产路径是 `QqSender`，`pub(crate)` 且必然发 HTTPS）。
#[derive(Default)]
struct FakeReply {
    /// `(openid, msg_id, content)`。
    group: Mutex<Vec<(String, String, String)>>,
    c2c: Mutex<Vec<(String, String, String)>>,
}

impl FakeReply {
    fn group(&self) -> Vec<(String, String, String)> {
        self.group.lock().expect("群回复记录锁").clone()
    }

    fn c2c(&self) -> Vec<(String, String, String)> {
        self.c2c.lock().expect("单聊回复记录锁").clone()
    }
}

fn receipt(msg_id: &str) -> hbot_adapters::qq::SendReceipt {
    hbot_adapters::qq::SendReceipt {
        message_id: Some("platform-message".to_owned()),
        msg_seq: 1,
        reply_to: msg_id.to_owned(),
    }
}

#[async_trait]
impl PassiveReply for FakeReply {
    async fn send_group(
        &self,
        group_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<hbot_adapters::qq::SendReceipt, AdapterError> {
        self.group.lock().expect("群回复记录锁").push((
            group_openid.to_owned(),
            msg_id.to_owned(),
            content.to_owned(),
        ));
        Ok(receipt(msg_id))
    }

    async fn send_c2c(
        &self,
        user_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<hbot_adapters::qq::SendReceipt, AdapterError> {
        self.c2c.lock().expect("单聊回复记录锁").push((
            user_openid.to_owned(),
            msg_id.to_owned(),
            content.to_owned(),
        ));
        Ok(receipt(msg_id))
    }
}

fn envelope(kind: ConversationKind, external_id: &str, reply_to: Option<&str>) -> OutboundEnvelope {
    OutboundEnvelope {
        id: OutboundId::from_uuid(uuid::Uuid::from_u128(
            0x0e00_0000_0000_0000_0000_0000_0000_0001,
        )),
        bot_id: bot_id(),
        conversation_id: ConversationId::from_uuid(uuid::Uuid::from_u128(
            0x0c00_0000_0000_0000_0000_0000_0000_0001,
        )),
        conversation: ConversationRef::new(external_id, kind),
        text: "pong".to_owned(),
        reply_to: reply_to.map(str::to_owned),
        attempts: 1,
    }
}

fn fake_adapter(fake: &Arc<FakeReply>) -> QqAdapter {
    QqAdapter::with_reply_port(
        QqConfig::new("10001", FAKE_SECRET),
        Arc::clone(fake) as Arc<dyn PassiveReply>,
    )
    .expect("凭据合法就能建适配器")
}

#[tokio::test]
async fn passive_reply_carries_the_inbound_message_id() {
    let fake = Arc::new(FakeReply::default());
    let adapter = fake_adapter(&fake);

    // 群：`external_id` 是 group_openid，`reply_to` 是 d.id。
    adapter
        .send(envelope(
            ConversationKind::Group,
            "group-openid-1",
            Some("msg-123"),
        ))
        .await
        .expect("群被动回复应该成功");
    assert_eq!(
        fake.group(),
        vec![(
            "group-openid-1".to_owned(),
            "msg-123".to_owned(),
            "pong".to_owned()
        )],
        "入站消息 ID 必须原样作为 msg_id 传给 send_group_passive"
    );

    // 单聊：`external_id` 是 user_openid，端点换成 c2c。
    adapter
        .send(envelope(
            ConversationKind::Private,
            "user-openid-1",
            Some("msg-456"),
        ))
        .await
        .expect("单聊被动回复应该成功");
    assert_eq!(
        fake.c2c(),
        vec![(
            "user-openid-1".to_owned(),
            "msg-456".to_owned(),
            "pong".to_owned()
        )],
        "单聊必须走 send_c2c_passive，且带上 d.id"
    );
    assert_eq!(fake.group().len(), 1, "单聊不该走群端点");
}

#[tokio::test]
async fn passive_reply_without_reply_to_is_an_error() {
    let fake = Arc::new(FakeReply::default());
    let adapter = fake_adapter(&fake);

    // ① 没有 reply_to：QQ 官方只允许被动回复，主动推送没实现 → 真错误，不伪造成功。
    let error = adapter
        .send(envelope(ConversationKind::Group, "group-openid-1", None))
        .await
        .expect_err("reply_to 为 None 时必须报错，不许返回 Ok");
    assert_eq!(error.code(), -32005, "平台不允许的动作是 denied：{error}");
    assert!(
        text_of(&error).contains("d.id") || text_of(&error).contains("reply_to"),
        "错误要说清缺的是触发消息 ID：{error}"
    );
    assert!(fake.group().is_empty(), "缺 msg_id 时不许调出去");

    // ② 空串 / 全空白按缺失处理（不要把空 msg_id 发给平台）。
    for blank in ["", "   "] {
        let error = adapter
            .send(envelope(
                ConversationKind::Group,
                "group-openid-1",
                Some(blank),
            ))
            .await
            .expect_err("空 msg_id 必须报错");
        assert_eq!(error.code(), -32005, "{error}");
    }
    assert!(fake.group().is_empty(), "空 msg_id 也不许调出去");

    // ③ 频道会话：QQ 官方适配器没有这个端点（另一套端点没验证过）→ 明确报错。
    let error = adapter
        .send(envelope(
            ConversationKind::Channel,
            "channel-1",
            Some("msg-789"),
        ))
        .await
        .expect_err("频道会话没有被动回复端点");
    assert_eq!(error.code(), -32005, "{error}");
    assert!(
        text_of(&error).contains("channel"),
        "错误里要带会话类型：{error}"
    );
    assert!(fake.group().is_empty() && fake.c2c().is_empty());

    // ④ 还没订阅过（没有出站端口）：也是真错误（-32006），不是静默丢弃。
    let unsubscribed = QqAdapter::new(QqConfig::new("10001", FAKE_SECRET)).expect("合法凭据");
    let error = unsubscribed
        .send(envelope(
            ConversationKind::Group,
            "group-openid-1",
            Some("msg-123"),
        ))
        .await
        .expect_err("没有出站端口时必须报错");
    assert_eq!(error.code(), -32006, "{error}");
    assert!(
        text_of(&error).contains("subscribe"),
        "要说清是还没订阅：{error}"
    );
}
