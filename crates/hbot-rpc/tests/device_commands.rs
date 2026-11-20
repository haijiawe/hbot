//! 设备能力六条命令的集成测试（真 `Dispatcher::with_state` + `dispatch`）。
//!
//! 走真 `AppState`、真 `Store`（临时目录里的 sqlite）、真 `EventBus`，只有时钟是
//! [`ManualClock`] —— 到期结算要能"把表拨过去"，等真的一秒会变成 flaky 测试。
//!
//! 两个刻意的口径：
//!
//! 1. **`device.json` 在 `AppState::open` 之前写进 data_dir**：那是启动时 load 的那份。
//!    启动之后再改文件也**会**生效，但要等 1 秒的 mtime 轮询（`device_settings_set`
//!    写完的文件同理）；测试里要"立刻生效"就直接调 `registry.reload_settings_if_changed()`，
//!    别去 sleep 一秒。
//! 2. **清单声明来自真项目目录里的 `hbot.json`**：闸门读的是"当前项目"的声明，所以测试
//!    必须先 `open_project`，否则清单那一闸永远按"没声明"拒。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_core::{BotId, ProjectId, Timestamp};
use hbot_rpc::app::ids::{ManualClock, UuidIds};
use hbot_rpc::dto::{ApiError, Command, DeviceSettings, Event};
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

// ---------------------------------------------------------------------------
// ⑪ device_settings_get：默认值就是真值（读内存，不顺手落盘）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_get_returns_the_defaults_and_never_writes_a_file() {
    // 不给 device.json：走 `DeviceSettings::default()`。
    let fixture = fixture(None, Some(MANIFEST)).await;

    let data = data_of(&fixture.dispatcher, Command::DeviceSettingsGet).await;

    assert!(data.is_object(), "data 必须是对象：{data}");
    assert_eq!(data["enabled"], json!(false), "默认全关");
    assert_eq!(
        data["capabilities"],
        json!([]),
        "空清单 = 一个子能力都不放行"
    );
    assert_eq!(data["allowedApps"], json!([]), "空白名单 = 不按应用收口");
    assert_eq!(data["defaultMinutes"], json!(5));
    assert_eq!(data["maxMinutes"], json!(30));
    assert_eq!(data["maxActionsPerSession"], json!(10000));
    assert_eq!(
        data.as_object().expect("对象").len(),
        6,
        "字段就这六个（camelCase），别多加：{data}"
    );
    assert!(
        !fixture
            .state
            .device()
            .data_dir()
            .join("device.json")
            .exists(),
        "get 是纯读：不许顺手把默认值写下去"
    );
}

// ---------------------------------------------------------------------------
// ⑫ device_settings_set：全链路（校验 → 落盘 → 换内存 → 广播 → 返回新值）
// ---------------------------------------------------------------------------

fn settings_command(settings: DeviceSettings) -> Command {
    Command::DeviceSettingsSet { settings }
}

/// 一份全合法的基线载荷；各用例只改一处。
fn valid_settings() -> DeviceSettings {
    DeviceSettings {
        enabled: true,
        capabilities: vec!["screen".to_owned(), "ocr".to_owned()],
        allowed_apps: vec!["notepad.exe".to_owned()],
        default_minutes: 5,
        max_minutes: 20,
        max_actions_per_session: 100,
    }
}

#[tokio::test]
async fn settings_set_writes_disk_updates_memory_and_broadcasts_one_event() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let path = fixture.state.device().data_dir().join("device.json");
    let before = fixture.state.device().settings();
    assert!(before.enabled, "夹具起点是全开");
    let mut events = fixture.state.events().subscribe();

    let data = data_of(
        &fixture.dispatcher,
        settings_command(DeviceSettings {
            enabled: false,
            // 故意乱序：返回体里的顺序必须按枚举声明序稳定下来。
            capabilities: vec!["window".to_owned(), "ocr".to_owned()],
            // 大小写 + 前后空白 + 重复项：归一化后只留一条 `notepad.exe`。
            allowed_apps: vec!["  Notepad.EXE ".to_owned(), "NOTEPAD.exe".to_owned()],
            default_minutes: 7,
            max_minutes: 12,
            max_actions_per_session: 42,
        }),
    )
    .await;

    // ① 返回体就是写入后的那份（不是"被校验修正过"的静默版本）。
    assert_eq!(data["enabled"], json!(false));
    assert_eq!(data["capabilities"], json!(["ocr", "window"]));
    assert_eq!(data["allowedApps"], json!(["notepad.exe"]));
    assert_eq!(data["defaultMinutes"], json!(7));
    assert_eq!(data["maxMinutes"], json!(12));
    assert_eq!(data["maxActionsPerSession"], json!(42));

    // ② 内存换了。
    let in_memory = fixture.state.device().settings();
    assert_ne!(in_memory, before, "内存必须换成新值");
    assert!(!in_memory.enabled);
    assert_eq!(in_memory.allowed_apps, vec!["notepad.exe".to_owned()]);
    assert_eq!(in_memory.default_minutes, 7);
    assert_eq!(in_memory.max_minutes, 12);

    // ③ 磁盘也换了，而且是 camelCase、没有 .tmp 残留（原子写）。
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("设置应已落盘")).expect("合法 JSON");
    assert_eq!(on_disk["enabled"], json!(false));
    assert_eq!(on_disk["allowedApps"], json!(["notepad.exe"]));
    assert_eq!(on_disk["maxMinutes"], json!(12));
    assert_eq!(on_disk["maxActionsPerSession"], json!(42));
    assert!(
        on_disk.get("max_minutes").is_none(),
        "磁盘上也是 camelCase：{on_disk}"
    );
    assert!(
        !path.with_extension("json.tmp").exists(),
        "rename 成功后不许留下 device.json.tmp"
    );

    // ④ `device.settings` 广播了一条，内容是新值。
    let event = events.try_recv().expect("成功后必须广播 device.settings");
    let Event::DeviceSettings { settings } = event else {
        panic!("事件类型不对：{event:?}");
    };
    assert!(!settings.enabled);
    assert_eq!(
        settings.capabilities,
        vec!["ocr".to_owned(), "window".to_owned()]
    );
    assert_eq!(settings.allowed_apps, vec!["notepad.exe".to_owned()]);
    assert_eq!(settings.max_actions_per_session, 42);

    // ⑤ get 读的是内存：立刻就能看到同一份。
    assert_eq!(
        data_of(&fixture.dispatcher, Command::DeviceSettingsGet).await,
        data,
        "get 不该再去读盘（否则写盘刚失败时也会显示成功）"
    );

    // ⑥ 没有任何多余事件（一次写入只广播一条设置）。
    assert!(
        events.try_recv().is_err(),
        "一次 device_settings_set 只许广播一条 device.settings"
    );
}

// ---------------------------------------------------------------------------
// ⑬ 校验：每种非法输入都要 -32002，且**磁盘一个字节都不许变**
// ---------------------------------------------------------------------------

/// 派发一条应当被 `-32002` 拒掉的设置写入，并钉住四件事：
/// 错误码、磁盘没变、内存没变、没广播事件（"绝不静默修正"）。
async fn assert_settings_rejected(
    fixture: &Fixture,
    payload: DeviceSettings,
    hint: &str,
) -> ApiError {
    let path = fixture.state.device().data_dir().join("device.json");
    let disk_before = std::fs::read(&path).expect("夹具应当已写过 device.json");
    let memory_before = fixture.state.device().settings();
    let mut events = fixture.state.events().subscribe();

    let error = error_of(&fixture.dispatcher, settings_command(payload)).await;

    assert_eq!(error.code, -32002, "{hint}：{error:?}");
    assert!(
        !error.message.trim().is_empty(),
        "{hint}：错误要带可读文案（用户得知道怎么改）"
    );
    assert_eq!(
        std::fs::read(&path).expect("device.json 还在"),
        disk_before,
        "{hint}：校验失败绝不许改磁盘 —— 静默修正会让用户以为自己设的就是生效的那份"
    );
    assert_eq!(
        fixture.state.device().settings(),
        memory_before,
        "{hint}：内存也不许变"
    );
    assert!(
        events.try_recv().is_err(),
        "{hint}：失败不许广播 device.settings"
    );
    error
}

#[tokio::test]
async fn settings_set_rejects_an_unknown_capability_name() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut payload = valid_settings();
    payload.capabilities = vec!["screen".to_owned(), "scren".to_owned()];

    let error = assert_settings_rejected(&fixture, payload, "未知子能力").await;
    assert!(
        error.message.contains("scren"),
        "要点出哪个名字：{}",
        error.message
    );
    assert!(
        error.message.contains("clipboard"),
        "要列出全部允许值：{}",
        error.message
    );
}

#[tokio::test]
async fn settings_set_rejects_a_max_minutes_over_the_hard_cap() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut payload = valid_settings();
    payload.max_minutes = 31;

    let error = assert_settings_rejected(&fixture, payload, "maxMinutes=31").await;
    assert!(error.message.contains("maxMinutes"), "{}", error.message);
}

#[tokio::test]
async fn settings_set_rejects_a_default_minutes_over_the_max() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut payload = valid_settings();
    payload.max_minutes = 10;
    payload.default_minutes = 11;

    let error = assert_settings_rejected(&fixture, payload, "defaultMinutes > maxMinutes").await;
    assert!(
        error.message.contains("defaultMinutes"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn settings_set_rejects_an_empty_app_name() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut payload = valid_settings();
    payload.allowed_apps = vec!["notepad.exe".to_owned(), "   ".to_owned()];

    let error = assert_settings_rejected(&fixture, payload, "空应用名").await;
    assert!(
        error.message.contains("第 2 项"),
        "要指出第几项：{}",
        error.message
    );
}

#[tokio::test]
async fn settings_set_rejects_a_zero_action_budget() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let mut payload = valid_settings();
    payload.max_actions_per_session = 0;

    let error = assert_settings_rejected(&fixture, payload, "maxActionsPerSession=0").await;
    assert!(
        error.message.contains("maxActionsPerSession"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// ⑭ 落盘失败不许污染内存（否则"界面显示成功，文件还是旧的"）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_write_leaves_memory_untouched_and_broadcasts_nothing() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    let data_dir = fixture.state.device().data_dir().to_path_buf();
    let path = data_dir.join("device.json");
    let disk_before = std::fs::read(&path).expect("夹具应当已写过 device.json");
    let memory_before = fixture.state.device().settings();
    let mut events = fixture.state.events().subscribe();

    // 让**临时文件**写不出来：`device.json.tmp` 先占成一个同名目录。
    // 这样 `save()` 在 `fs::write(temp)` 那一步就失败（不重试、也不碰真文件），
    // 与"rename 撞 EPERM"那种临时故障分开测 —— 那种会重试，最后也可能成功。
    let temp = path.with_extension("json.tmp");
    std::fs::create_dir(&temp).expect("占住临时文件名");

    let error = error_of(&fixture.dispatcher, settings_command(valid_settings())).await;

    assert_eq!(error.code, -32006, "写盘失败要走 internal：{error:?}");
    assert!(
        error.message.contains("写盘失败"),
        "文案要说清是落盘失败：{}",
        error.message
    );
    assert_eq!(
        fixture.state.device().settings(),
        memory_before,
        "落盘失败绝不许改内存（内存必须晚于磁盘）"
    );
    assert_eq!(
        std::fs::read(&path).expect("device.json 还在"),
        disk_before,
        "失败后磁盘上还是旧的那份"
    );
    assert!(events.try_recv().is_err(), "失败不许广播 device.settings");

    // 收拾掉占位的临时目录（TempDir 的 Drop 也会删，这里只是让断言更明确）。
    std::fs::remove_dir(&temp).expect("清掉占位目录");
}

// ---------------------------------------------------------------------------
// ⑮ 外部改文件（另一个窗口 / 手改）：watcher 重载后 get 立刻看到
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_external_edit_is_reloaded_by_the_watcher_and_shows_up_in_get() {
    let fixture = fixture(Some(ENABLED_SETTINGS), Some(MANIFEST)).await;
    // 启动时 load 的那份与磁盘一致：第一次调用只对齐基线，不报变化。
    assert!(
        fixture
            .state
            .device()
            .reload_settings_if_changed()
            .is_none(),
        "内容没变就不该重载"
    );

    // 不必等一拍：判据是文件字节，同一时间戳内的两次写也能分辨（见 docs/pitfalls.md 坑 117）。
    std::fs::write(
        fixture.state.device().data_dir().join("device.json"),
        APPS_ONLY_SETTINGS,
    )
    .expect("外部改设置");

    assert!(
        fixture
            .state
            .device()
            .reload_settings_if_changed()
            .is_some(),
        "字节变了就要重载"
    );
    let data = data_of(&fixture.dispatcher, Command::DeviceSettingsGet).await;
    assert_eq!(data["enabled"], json!(false), "外部把开关关了");
    assert_eq!(data["allowedApps"], json!(["msedge.exe"]));
    assert!(
        fixture
            .state
            .device()
            .reload_settings_if_changed()
            .is_none(),
        "同一份字节只处理一次（坏文件也不许每秒重放事件）"
    );
}
