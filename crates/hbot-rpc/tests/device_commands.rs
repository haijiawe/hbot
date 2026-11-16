//! 设备能力四条命令的集成测试（真 `Dispatcher::with_state` + `dispatch`）。
//!
//! 走真 `AppState`、真 `Store`（临时目录里的 sqlite）、真 `EventBus`，只有时钟是
//! [`ManualClock`] —— 到期结算要能"把表拨过去"，等真的一秒会变成 flaky 测试。
//!
//! 两个刻意的口径：
//!
//! 1. **`device.json` 在 `AppState::open` 之前写进 data_dir**：设置是启动时 load 的，
//!    事后再改文件不会影响已建好的登记表（那正是"启动读一次"的语义）。
//! 2. **清单声明来自真项目目录里的 `hbot.json`**：闸门读的是"当前项目"的声明，所以测试
//!    必须先 `open_project`，否则清单那一闸永远按"没声明"拒。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_core::{BotId, ProjectId, Timestamp};
use hbot_rpc::app::ids::{ManualClock, UuidIds};
use hbot_rpc::dto::{ApiError, Command, Event};
use hbot_rpc::{AppState, Dispatcher};
use hbot_store::{Bot, Project};
use serde_json::{Value, json};

/// 所有测试共用的起始时刻（毫秒），足够大且不等于 0，便于从 `remainingMs` 反推。
const START_MS: i64 = 1_000_000;

/// 一个只有设备声明的合法清单（`hbot-code` 的校验只查形状，不要求入口文件存在）。
const MANIFEST: &str = r#"{
  "name": "device-test",
  "platform": { "kind": "local" },
  "permissions": { "http": [], "device": { "sub": ["screen", "input", "window", "clipboard"], "apps": ["msedge.exe"] } }
}"#;

/// 全套打开：宿主开关 on、四个子能力都放行。
const ENABLED_SETTINGS: &str = r#"{
  "enabled": true,
  "capabilities": ["screen", "input", "window", "clipboard"],
  "allowedApps": [],
  "defaultMinutes": 5,
  "maxMinutes": 30,
  "maxActionsPerSession": 10000
}"#;

/// 开关 on 但宿主一个子能力都没放行（空 = 全拒）。
const EMPTY_CAPABILITIES_SETTINGS: &str = r#"{ "enabled": true, "capabilities": [] }"#;

/// 只有 app 白名单、开关没动（默认 false）：用来验证 `device_status` 的 `allowedApps` 取设置值。
const APPS_ONLY_SETTINGS: &str = r#"{ "allowedApps": ["msedge.exe"] }"#;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-device-commands-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Fixture {
    /// 让临时目录活到测试结束（Drop 时删干净）。
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    clock: Arc<ManualClock>,
    bot_id: BotId,
}

/// 建一套"真"环境：临时 data_dir（可带 `device.json`）+ 临时项目目录（可带 `hbot.json`）
/// + 一个落库的 bot + 接好 state 的 Dispatcher。
async fn fixture(settings_json: Option<&str>, manifest_json: Option<&str>) -> Fixture {
    let dir = TempDir::new("case");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("创建 data_dir");
    if let Some(json) = settings_json {
        std::fs::write(data_dir.join("device.json"), json).expect("写 device.json");
    }

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(project_dir.join("src")).expect("创建项目目录");
    // hbot-code 的清单校验要求入口文件存在（`entry` 指向必须落地）。
    std::fs::write(project_dir.join("src").join("main.ts"), "export {};\n").expect("写入口文件");
    if let Some(json) = manifest_json {
        std::fs::write(project_dir.join("hbot.json"), json).expect("写 hbot.json");
    }

    let clock = Arc::new(ManualClock::new(Timestamp::from_unix_millis(START_MS)));
    let state = Arc::new(
        AppState::open(&data_dir, clock.clone(), Arc::new(UuidIds)).expect("打开 AppState"),
    );
    state.open_project(&project_dir).await.expect("打开项目");

    let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0x0b07_0000_0000_0001));
    let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0x0b07_0000_0000_0002));
    state
        .store()
        .insert_project(&Project {
            id: project_id,
            name: "设备测试项目".to_owned(),
            path: project_dir.display().to_string(),
            created_at: 0,
        })
        .expect("insert_project");
    state
        .store()
        .insert_bot(&Bot {
            id: bot_id,
            project_id,
            name: "设备测试 bot".to_owned(),
            platform: "local".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 0,
        })
        .expect("insert_bot");

    let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
    Fixture {
        _dir: dir,
        state,
        dispatcher,
        clock,
        bot_id,
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn status_command(bot_id: &BotId) -> Command {
    Command::DeviceStatus { bot_id: *bot_id }
}

fn grant_command(bot_id: &BotId, sub: &[&str], apps: &[&str], minutes: Option<u32>) -> Command {
    Command::DeviceGrant {
        bot_id: *bot_id,
        sub: sub.iter().map(|name| (*name).to_owned()).collect(),
        allowed_apps: apps.iter().map(|app| (*app).to_owned()).collect(),
        minutes,
    }
}

fn revoke_command(bot_id: &BotId) -> Command {
    Command::DeviceRevoke { bot_id: *bot_id }
}

fn request_command(bot_id: &BotId, sub: &[&str], reason: &str) -> Command {
    Command::DeviceRequest {
        bot_id: *bot_id,
        sub: sub.iter().map(|name| (*name).to_owned()).collect(),
        reason: reason.to_owned(),
    }
}

/// 派发一条命令，要求成功且带 data（`data: null` 会在这里炸）。
async fn data_of(dispatcher: &Dispatcher, command: Command) -> Value {
    dispatcher
        .dispatch(command)
        .await
        .into_result()
        .expect("命令应当成功")
        .expect("data 不该是 null")
}

/// 派发一条命令，要求失败。
async fn error_of(dispatcher: &Dispatcher, command: Command) -> ApiError {
    dispatcher
        .dispatch(command)
        .await
        .into_result()
        .expect_err("命令应当失败")
}

/// `device_status` 的 data。
async fn status_of(dispatcher: &Dispatcher, bot_id: &BotId) -> Value {
    data_of(dispatcher, status_command(bot_id)).await
}

// ---------------------------------------------------------------------------
// ① 无会话也要给对象
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_without_a_session_is_none_and_still_carries_an_object() {
    let fixture = fixture(Some(APPS_ONLY_SETTINGS), Some(MANIFEST)).await;

    let data = status_of(&fixture.dispatcher, &fixture.bot_id).await;

    assert!(data.is_object(), "data 必须是对象，不能是 null：{data}");
    assert_eq!(data["state"], json!("none"));
    assert_eq!(data["sub"], json!([]));
    // 没有会话时 app 白名单取设置里的值：UI 要能提前显示"允许哪些窗口"。
    assert_eq!(data["allowedApps"], json!(["msedge.exe"]));
    assert_eq!(data["sessionId"], Value::Null);
    assert_eq!(data["grantedAt"], Value::Null);
    assert_eq!(data["deadline"], Value::Null);
    assert_eq!(data["remainingMs"], Value::Null);
    assert_eq!(data["botId"], json!(fixture.bot_id.to_string()));
}

// ---------------------------------------------------------------------------
// ② 默认设置（enabled=false）→ -32002 且文案提到 device.enabled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grant_is_refused_when_the_settings_switch_is_off() {
    // 不写 device.json：默认值 enabled = false（空 = 全拒，这不是"没配置所以随便来"）。
    let fixture = fixture(None, Some(MANIFEST)).await;

    let error = error_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(5)),
    )
    .await;

    assert_eq!(error.code, -32002, "未启用属于参数/状态错：{error:?}");
    assert!(
        error.message.contains("device.enabled"),
        "要告诉用户去开哪个开关：{}",
        error.message
    );
    // 被拒之后不能留下任何会话。
    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("none"));
}

// ---------------------------------------------------------------------------
// ③ 开关 on 但宿主一个子能力都没放行（空 = 全拒）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grant_is_refused_when_the_settings_capability_list_is_empty() {
    let fixture = fixture(Some(EMPTY_CAPABILITIES_SETTINGS), Some(MANIFEST)).await;

    let error = error_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(5)),
    )
    .await;

    assert_eq!(error.code, -32002, "{error:?}");
    assert!(
        error.message.contains("device.capabilities"),
        "要点出是宿主设置里没有放行：{}",
        error.message
    );
    assert_eq!(
        status_of(&fixture.dispatcher, &fixture.bot_id).await["state"],
        json!("none")
    );
}

// ---------------------------------------------------------------------------
// ④ 正常 grant → status 为 active，sub / allowedApps 与请求一致，remainingMs > 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grant_then_status_is_active_with_the_requested_set() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;

    let granted = data_of(
        &fixture.dispatcher,
        // minutes 缺省 → 用设置的 defaultMinutes（5）。
        grant_command(&fixture.bot_id, &["screen", "input"], &["msedge.exe"], None),
    )
    .await;

    assert_eq!(granted["state"], json!("active"));
    assert_eq!(granted["sub"], json!(["screen", "input"]));
    assert_eq!(granted["allowedApps"], json!(["msedge.exe"]));
    assert_eq!(granted["remainingMs"], json!(5 * 60 * 1000));
    let session_id = granted["sessionId"].as_str().expect("新会话要有 id");
    assert!(!session_id.is_empty());

    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("active"));
    assert_eq!(status["sub"], json!(["screen", "input"]));
    assert_eq!(status["allowedApps"], json!(["msedge.exe"]));
    assert_eq!(status["sessionId"], json!(session_id));
    assert_eq!(status["remainingMs"], json!(5 * 60 * 1000));
    assert!(status["deadline"].as_i64().expect("要有 deadline") > START_MS);
}

// ---------------------------------------------------------------------------
// ⑤ minutes 999 被截到上限 30 分钟（善意超限截断，不是静默放宽安全）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn minutes_over_the_cap_are_clamped_to_the_settings_maximum() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;

    let granted = data_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(999)),
    )
    .await;

    assert_eq!(granted["state"], json!("active"));
    assert_eq!(
        granted["remainingMs"],
        json!(30 * 60 * 1000),
        "超过 maxMinutes 要截到 30 分钟"
    );
    let deadline = granted["deadline"].as_i64().expect("要有 deadline");
    assert_eq!(deadline, START_MS + 30 * 60 * 1000);
}

// ---------------------------------------------------------------------------
// ⑥ revoke 之后 status 是 revoked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_turns_the_session_revoked() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let granted = data_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(5)),
    )
    .await;

    let response = fixture
        .dispatcher
        .dispatch(revoke_command(&fixture.bot_id))
        .await;
    assert!(
        response.into_result().expect("撤销应当成功").is_none(),
        "device_revoke 的 data 是 null"
    );

    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("revoked"));
    // 最近的会话仍然可查询（撤销不是"删行"）。
    assert_eq!(status["sessionId"], granted["sessionId"]);
    assert_eq!(status["remainingMs"], Value::Null);
    assert_eq!(status["sub"], json!(["screen"]));

    let row = fixture
        .state
        .store()
        .latest_device_session(&fixture.bot_id)
        .expect("读设备会话")
        .expect("库里应当有这一行");
    assert_eq!(row.state, "revoked");
    assert!(row.ended_at.is_some(), "撤销必须写 ended_at（docs/09 §7）");
}

// ---------------------------------------------------------------------------
// ⑦ revoke 幂等：连撤两次都成功，没有活跃会话也成功
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_is_idempotent_even_without_a_live_session() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;

    // 从没有过会话：撤销也应当成功（幂等，不是 -32001/-32003）。
    fixture
        .dispatcher
        .dispatch(revoke_command(&fixture.bot_id))
        .await
        .into_result()
        .expect("没有会话时撤销也应当成功");

    data_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(5)),
    )
    .await;

    for attempt in 0..2 {
        fixture
            .dispatcher
            .dispatch(revoke_command(&fixture.bot_id))
            .await
            .into_result()
            .unwrap_or_else(|error| panic!("第 {attempt} 次撤销应当成功：{error:?}"));
    }

    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("revoked"));
}

// ---------------------------------------------------------------------------
// ⑧ 换会话：旧的被 end_live_device_sessions 收尾（内存只剩新的那条）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_grant_replaces_the_previous_session() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;

    let first = data_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(5)),
    )
    .await;
    // 拨 1 秒再开第二条：`latest_device_session` 按 (created_at DESC, id DESC) 定序，
    // 同一毫秒内的两行只能靠随机 UUID 定序 —— 测试要的是确定性，所以把时间错开。
    fixture.clock.advance(1_000);
    let second = data_of(
        &fixture.dispatcher,
        grant_command(
            &fixture.bot_id,
            &["screen", "input"],
            &["msedge.exe"],
            Some(5),
        ),
    )
    .await;

    assert_ne!(
        first["sessionId"], second["sessionId"],
        "第二次 grant 必须是新会话，不能复用旧 id"
    );
    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("active"));
    assert_eq!(
        status["sessionId"], second["sessionId"],
        "内存里只剩新的那条"
    );
    assert_eq!(status["sub"], json!(["screen", "input"]));

    // 库里最新的那行是新的、active；旧行已被收尾成 revoked —— 存储层没有
    // `get_device_session(id)`，所以旧行只能按语义核对（见终报"等待别人的缺口"）。
    let latest = fixture
        .state
        .store()
        .latest_device_session(&fixture.bot_id)
        .expect("读设备会话")
        .expect("库里应当有会话");
    assert_eq!(latest.id, second["sessionId"].as_str().expect("新会话 id"));
    assert_eq!(latest.state, "active");
    assert!(latest.ended_at.is_none(), "活跃会话不该有 ended_at");
}

// ---------------------------------------------------------------------------
// ⑨ device_request 只发事件、不建会话、不改 status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_only_emits_an_event_and_never_opens_a_session() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut events = fixture.state.events().subscribe();
    let before = status_of(&fixture.dispatcher, &fixture.bot_id).await;

    let response = fixture
        .dispatcher
        .dispatch(request_command(
            &fixture.bot_id,
            &["screen"],
            "我想截一张桌面截图",
        ))
        .await;
    assert!(
        response.into_result().expect("请求应当成功").is_none(),
        "device_request 的 data 是 null"
    );

    // 状态一个字节都不该变：请求≠授权。
    let after = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(before, after, "device_request 不许改任何状态");
    assert_eq!(after["state"], json!("none"), "不许在这里开会话");
    assert!(
        fixture
            .state
            .store()
            .latest_device_session(&fixture.bot_id)
            .expect("读设备会话")
            .is_none(),
        "不许往库里写会话行"
    );

    let event = events.try_recv().expect("要收到 device.request 事件");
    let Event::DeviceRequest {
        bot_id,
        sub,
        reason,
        request_id,
    } = event
    else {
        panic!("事件类型不对：{event:?}");
    };
    assert_eq!(bot_id, fixture.bot_id);
    assert_eq!(sub, vec!["screen".to_owned()]);
    assert_eq!(reason, "我想截一张桌面截图");
    assert!(!request_id.is_empty(), "request_id 不能是空串");
}

// ---------------------------------------------------------------------------
// ⑩ 到期结算：把表拨过 deadline，直接调 sweep_expired（不等真的 1 秒）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sweep_expired_settles_the_session_after_the_deadline() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    data_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["screen"], &[], Some(1)),
    )
    .await;
    let mut events = fixture.state.events().subscribe();

    // 拨过 1 分钟的 deadline，然后**同一轮内**结算：这中间没有 await，
    // 1 秒 tick 任务不可能插进来，所以下面的计数是确定的。
    fixture.clock.advance(60_001);
    let settled = fixture
        .state
        .device()
        .sweep_expired()
        .expect("到期结算应当成功");
    assert_eq!(settled, 1, "只有一条到期会话");

    let status = status_of(&fixture.dispatcher, &fixture.bot_id).await;
    assert_eq!(status["state"], json!("expired"));
    assert_eq!(status["remainingMs"], Value::Null);
    // 到期的会话保留可查询，但不再 live。
    assert!(
        status["sessionId"].as_str().is_some(),
        "到期后仍要能查到最近那条会话"
    );

    let event = events.try_recv().expect("要收到 device.status 事件");
    let Event::DeviceStatus { status: pushed } = event else {
        panic!("事件类型不对：{event:?}");
    };
    assert_eq!(pushed.state, "expired");

    // 库里也落地成 expired + ended_at（docs/09 §7 要求写结束时刻）。
    let row = fixture
        .state
        .store()
        .latest_device_session(&fixture.bot_id)
        .expect("读设备会话")
        .expect("库里应当有会话");
    assert_eq!(row.state, "expired");
    assert!(row.ended_at.is_some(), "到期必须写 ended_at");

    // 再结算一次：没有 live 会话，返回 0（幂等）。
    assert_eq!(
        fixture
            .state
            .device()
            .sweep_expired()
            .expect("第二次结算应当成功"),
        0
    );
}

// ---------------------------------------------------------------------------
// 错误码：-32001 bot 不存在（四条命令都钉住）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_bot_is_reported_as_not_found_on_all_four_commands() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let unknown = BotId::from_uuid(uuid::Uuid::from_u128(0xdead_beef_0000_0001));

    let commands = [
        status_command(&unknown),
        grant_command(&unknown, &["screen"], &[], Some(5)),
        revoke_command(&unknown),
        request_command(&unknown, &["screen"], "试试"),
    ];
    for command in commands {
        let kind = command.kind();
        let error = error_of(&fixture.dispatcher, command).await;
        assert_eq!(error.code, -32001, "{kind} 应当报 -32001：{error:?}");
        if let Some(data) = &error.data {
            assert_eq!(data["entity"], json!("bot"), "{kind} 的 data 要标出实体");
        }
    }
}

// ---------------------------------------------------------------------------
// 错误码：-32002 子能力名不认识，且列出允许值
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_capability_is_rejected_with_the_allowed_values() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;

    let error = error_of(
        &fixture.dispatcher,
        grant_command(&fixture.bot_id, &["scren"], &[], Some(5)),
    )
    .await;

    assert_eq!(error.code, -32002, "{error:?}");
    assert!(error.message.contains("scren"), "{}", error.message);
    assert!(
        error.message.contains("clipboard"),
        "要列出全部允许值：{}",
        error.message
    );
    assert_eq!(
        status_of(&fixture.dispatcher, &fixture.bot_id).await["state"],
        json!("none"),
        "参数错不许留下半开的会话"
    );
}

// ---------------------------------------------------------------------------
// reason 超长：截到 500 字符（按字符，不按字节）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_truncates_an_overlong_reason() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut events = fixture.state.events().subscribe();

    let long_reason = "截".repeat(600);
    fixture
        .dispatcher
        .dispatch(request_command(&fixture.bot_id, &["screen"], &long_reason))
        .await
        .into_result()
        .expect("请求应当成功");

    let event = events.try_recv().expect("要收到 device.request 事件");
    let Event::DeviceRequest { reason, .. } = event else {
        panic!("事件类型不对：{event:?}");
    };
    assert_eq!(reason.chars().count(), 500, "reason 要截到 500 个字符");
    assert!(reason.chars().all(|character| character == '截'));
}
