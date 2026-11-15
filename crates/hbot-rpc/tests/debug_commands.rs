//! 三条 debug 命令的**命令级**行为：`debug_set_breakpoints` / `debug_resume` / `debug_step`。
//!
//! ## 这个文件测什么、不测什么
//!
//! 测的是 `Dispatcher::dispatch → commands::debug → app::debug 登记表` 这条链：
//! bot 存在性判定（-32001 / -32005）、成功时的 wire 形状（`{"type":"ok","data":null}`）、
//! 没有暂停时的 -32003，以及"命令真的把动作送到了停在断点上的那个等待者"。
//!
//! **不测**插桩本体（`hbot-code` 的 `__hbot_trace` / `dispatch_with_debug` 的真实实现，
//! 由另一个并行单元负责）。所以这里没有编译 TypeScript、也没有真的跑 QuickJS：
//! "isolate 停在断点上"用 [`hbot_rpc::app::debug::DebugRegistry::register_pause`] 直接
//! 模拟——它正是 sink 在真实插桩里做的第一件事。这样本文件在插桩未落地时也能全绿。
//!
//! 事件形状（`debug.paused` / `debug.resumed` 的 JSON）由 `tests/events.rs` 的契约测试
//! 钉住，sink 的事件往返由 `src/app/debug/sink.rs` 的单测钉住，这里不重复。
//!
//! ## 为什么要造一个"运行中"的 bot
//!
//! `debug_set_breakpoints` 的成功路径要求 `BotRegistry::is_running()` 为真。集成测试拿不到
//! `#[cfg(test)]` 的 `test_support`，所以这里自带临时目录 + 两个最小 fake（适配器 / 执行器）
//! 直接构造 `Supervisor`，再经 `running_bot()` 登记进 registry——**不启动 supervisor**
//! （不连平台、不收消息），只让 registry 看到"这个 bot 在跑"。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use hbot_adapters::SystemClock;
use hbot_bot::{
    AdapterSubscription, BotAdapter, BotSpec, HandlerExecutor, MessageCtx, OutboundEnvelope,
    Supervisor, SupervisorDeps,
};
use hbot_core::debug::DebugAction;
use hbot_core::{BotId, HbotError, ProjectId, Timestamp};
use hbot_rpc::app::bots::running_bot;
use hbot_rpc::app::debug::PauseTicket;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::{AppState, Command, Dispatcher, Response, StepKind};
use serde_json::{Value, json};

/// 临时目录（`Drop` 递归删除）。刻意不引 `tempfile`，与 `src/app/test_support.rs` 同款。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-debug-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 适配器 fake：supervisor 不启动，所以订阅与发送都不会被真的用到。
struct FakeAdapter;

#[async_trait]
impl BotAdapter for FakeAdapter {
    fn id(&self) -> &'static str {
        "fake"
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        let (sender, subscription) = AdapterSubscription::channel(1);
        // 通道两端都留着会阻止 supervisor 退出；这里刻意丢掉发送端 ⇒ 事件流立即结束。
        drop(sender);
        Ok(subscription)
    }

    async fn send(&self, _envelope: OutboundEnvelope) -> Result<(), HbotError> {
        Ok(())
    }
}

/// 执行器 fake：本文件不投递消息，`on_message` 不会被调用。
struct FakeExecutor;

#[async_trait]
impl HandlerExecutor for FakeExecutor {
    async fn on_message(&self, _ctx: MessageCtx) -> Result<(), HbotError> {
        Ok(())
    }
}

/// 一个装配好的 [`AppState`] + [`Dispatcher`]，数据目录在临时目录里。
struct Harness {
    dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let state = Arc::new(
            AppState::open(
                dir.path().join("data"),
                Arc::new(SystemClock),
                Arc::new(UuidIds),
            )
            .expect("AppState::open"),
        );
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            dir,
            state,
            dispatcher,
        }
    }

    async fn call(&self, command: Command) -> Response {
        self.dispatcher.dispatch(command).await
    }

    /// 在库里写一条 project + bot 行（`debug_set_breakpoints` 用它区分 -32001 / -32005）。
    fn insert_bot_rows(&self, bot_id: BotId, project_id: ProjectId) {
        self.state
            .store()
            .insert_project(&hbot_store::Project {
                id: project_id,
                name: "debug-project".to_owned(),
                path: self
                    .dir
                    .path()
                    .join("project")
                    .to_string_lossy()
                    .into_owned(),
                created_at: 1_700_000_000_000,
            })
            .expect("insert_project");
        self.state
            .store()
            .insert_bot(&hbot_store::Bot {
                id: bot_id,
                project_id,
                name: "debug-bot".to_owned(),
                platform: "fake".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 1_700_000_000_000,
            })
            .expect("insert_bot");
    }

    /// 让 registry 认为这个 bot 正在运行（不启动 supervisor）。
    ///
    /// 需要 tokio runtime（`running_bot` 内部 `tokio::spawn` 收尾任务），所以只能在
    /// `#[tokio::test]` 里调用。
    fn mark_running(&self, bot_id: BotId, project_id: ProjectId) {
        let spec = BotSpec::new(bot_id, project_id, "debug-bot", "fake");
        let deps = SupervisorDeps::new(
            Arc::clone(self.state.store()),
            Arc::clone(self.state.ids()),
            Arc::clone(self.state.clock()),
            Arc::new(FakeAdapter),
            Arc::new(FakeExecutor),
        );
        let supervisor = Arc::new(Supervisor::new(spec, deps));
        let previous = self.state.bots().insert(
            bot_id,
            running_bot(
                "fake",
                Timestamp::from_unix_millis(1_700_000_000_000),
                supervisor,
            ),
        );
        assert!(previous.is_none(), "同一个 bot 只该登记一次");
        assert!(self.state.bots().is_running(&bot_id));
    }
}

/// 命令成功且 `data` 是 `null`（三条 debug 命令共同的返回形状）。
fn assert_ok_null(response: &Response) {
    match response {
        Response::Ok { data } => assert_eq!(data, &None, "debug 命令成功时 data 必须是 null"),
        Response::Error { error } => {
            panic!(
                "期望成功（data:null），实际 -{}：{}",
                error.code, error.message
            )
        }
    }
    // 同时钉住 wire 形状：UI/文档直接抄这个。
    let wire = serde_json::to_value(response).expect("序列化 Response");
    assert_eq!(
        wire,
        json!({"type": "ok", "data": null}),
        "debug 命令成功的 wire 形状必须是 {{type:ok,data:null}}"
    );
}

/// 命令失败且错误码是 `expected`。
fn assert_error_code(response: &Response, expected: i32) {
    match response {
        Response::Error { error } => assert_eq!(
            error.code, expected,
            "错误码不符；实际消息：{}",
            error.message
        ),
        Response::Ok { data } => panic!("期望 -{expected}，实际成功（data:{data:?}）"),
    }
}

fn bot() -> BotId {
    BotId::from_uuid(uuid::Uuid::new_v4())
}

fn project() -> ProjectId {
    ProjectId::from_uuid(uuid::Uuid::new_v4())
}

fn set_breakpoints(bot_id: BotId, file: &str, lines: &[u32]) -> Command {
    Command::DebugSetBreakpoints {
        bot_id,
        file: file.to_owned(),
        lines: lines.to_vec(),
    }
}

/// 模拟"isolate 已经停在断点上"：sink 在真实插桩里做的第一件事就是登记等待位。
fn fake_pause(harness: &Harness, bot_id: BotId, file: &str, line: u32) -> PauseTicket {
    let ticket = harness.state.debug().register_pause(bot_id, file, line);
    assert!(harness.state.debug().is_paused(&bot_id));
    ticket
}

fn recv_action(ticket: &PauseTicket) -> DebugAction {
    ticket
        .actions
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("命令应当把动作送到暂停中的等待者")
}

// ---------------------------------------------------------------- set_breakpoints

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_breakpoints_on_unknown_bot_is_not_found() {
    let harness = Harness::new("unknown-bot");
    let response = harness
        .call(set_breakpoints(bot(), "src/main.ts", &[3]))
        .await;
    assert_error_code(&response, -32001);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_breakpoints_on_a_stopped_bot_is_denied() {
    let harness = Harness::new("stopped-bot");
    let (bot_id, project_id) = (bot(), project());
    harness.insert_bot_rows(bot_id, project_id);

    // 库里有这个 bot，但没启动 ⇒ "现在不能调"，-32005 denied（不是不存在）。
    let response = harness
        .call(set_breakpoints(bot_id, "src/main.ts", &[3]))
        .await;
    assert_error_code(&response, -32005);
    // 失败的请求不该把断点记进登记表（否则用户改好状态后会被"幽灵断点"惊到）。
    assert!(
        harness.state.debug().breakpoints_of(&bot_id).is_empty(),
        "被拒绝的设断点不该留下状态"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_breakpoints_on_a_running_bot_returns_null_and_is_remembered() {
    let harness = Harness::new("running-bot");
    let (bot_id, project_id) = (bot(), project());
    harness.insert_bot_rows(bot_id, project_id);
    harness.mark_running(bot_id, project_id);

    let response = harness
        .call(set_breakpoints(bot_id, "src/main.ts", &[3, 9, 0]))
        .await;
    assert_ok_null(&response);

    // 断点落在登记表里（isolate 尚未出生时靠它，出生时 attach 会补推给沙箱）。
    let breakpoints = harness.state.debug().breakpoints_of(&bot_id);
    assert!(breakpoints.hits("src/main.ts", 3));
    assert!(breakpoints.hits("src/main.ts", 9));
    assert!(!breakpoints.hits("src/main.ts", 0), "行号 0 必须被忽略");

    // 空行集合 = 取消该文件的断点。
    let response = harness
        .call(set_breakpoints(bot_id, "src/main.ts", &[]))
        .await;
    assert_ok_null(&response);
    assert!(
        harness
            .state
            .debug()
            .breakpoints_of(&bot_id)
            .lines_of("src/main.ts")
            .is_none(),
        "空行集合应当移除该文件的断点"
    );
}

// ---------------------------------------------------------------- resume / step

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_without_a_pause_is_conflict() {
    let harness = Harness::new("resume-conflict");
    // 连 bot 都不存在也一样：这条命令唯一的失败条件是"没人在等"，不是"bot 不存在"。
    let response = harness.call(Command::DebugResume { bot_id: bot() }).await;
    assert_error_code(&response, -32003);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_step_kind_maps_to_its_debug_action() {
    let harness = Harness::new("step-kinds");
    let cases = [
        (StepKind::Over, DebugAction::StepOver),
        (StepKind::Into, DebugAction::StepInto),
        (StepKind::Out, DebugAction::StepOut),
    ];
    for (index, (kind, expected)) in cases.into_iter().enumerate() {
        let bot_id = bot();
        let ticket = fake_pause(&harness, bot_id, "src/main.ts", 10 + index as u32);

        let response = harness.call(Command::DebugStep { bot_id, kind }).await;
        assert_ok_null(&response);
        assert_eq!(
            recv_action(&ticket),
            expected,
            "step {kind:?} 的动作映射不对"
        );
        assert!(
            !harness.state.debug().is_paused(&bot_id),
            "放行之后不该还留着等待位"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_without_a_pause_is_conflict() {
    let harness = Harness::new("step-conflict");
    let response = harness
        .call(Command::DebugStep {
            bot_id: bot(),
            kind: StepKind::Over,
        })
        .await;
    assert_error_code(&response, -32003);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_wakes_the_isolate_with_continue() {
    let harness = Harness::new("resume");
    let bot_id = bot();
    let ticket = fake_pause(&harness, bot_id, "src/main.ts", 23);

    let response = harness.call(Command::DebugResume { bot_id }).await;
    assert_ok_null(&response);
    assert_eq!(recv_action(&ticket), DebugAction::Continue);

    // 同一次暂停不能被放行两次：第二次必须报 conflict。
    let response = harness.call(Command::DebugResume { bot_id }).await;
    assert_error_code(&response, -32003);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_resume_wakes_every_paused_isolate_of_the_bot() {
    // 一个 bot 最多两个 isolate（`MAX_ISOLATES`），可能同时停住；wire 上没有 pauseId，
    // 所以一条 resume 必须把两个都放行（否则 UI 会出现"点了继续还在停"）。
    let harness = Harness::new("two-isolates");
    let bot_id = bot();
    let first = fake_pause(&harness, bot_id, "src/main.ts", 1);
    let second = fake_pause(&harness, bot_id, "src/lib.ts", 2);
    assert_eq!(harness.state.debug().paused(&bot_id), 2);

    let response = harness.call(Command::DebugResume { bot_id }).await;
    assert_ok_null(&response);
    assert_eq!(recv_action(&first), DebugAction::Continue);
    assert_eq!(recv_action(&second), DebugAction::Continue);
    assert!(!harness.state.debug().is_paused(&bot_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_of_another_bot_is_not_woken() {
    let harness = Harness::new("other-bot");
    let (mine, other) = (bot(), bot());
    let mine_ticket = fake_pause(&harness, mine, "src/main.ts", 4);
    let other_ticket = fake_pause(&harness, other, "src/other.ts", 5);

    let response = harness.call(Command::DebugResume { bot_id: mine }).await;
    assert_ok_null(&response);
    assert_eq!(recv_action(&mine_ticket), DebugAction::Continue);
    assert!(
        other_ticket.actions.try_recv().is_err(),
        "别的 bot 的暂停不该被这条命令放行"
    );
    assert!(harness.state.debug().is_paused(&other));
}

// ---------------------------------------------------------------- wire 形状

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_commands_serialize_to_the_frozen_wire_shape() {
    // 成功：{"type":"ok","data":null}；失败：{"type":"error","error":{code,message,data}}。
    // 这两条直接抄进 docs/02-architecture.md 或 UI 的类型注释都不会错。
    let harness = Harness::new("wire");
    let running = bot();
    let (project_id, unknown) = (project(), bot());
    harness.insert_bot_rows(running, project_id);
    harness.mark_running(running, project_id);

    let ok = harness
        .call(set_breakpoints(running, "src/main.ts", &[1]))
        .await;
    assert_eq!(
        serde_json::to_value(&ok).expect("序列化"),
        json!({"type": "ok", "data": null})
    );

    let missing = harness
        .call(set_breakpoints(unknown, "src/main.ts", &[1]))
        .await;
    let wire: Value = serde_json::to_value(&missing).expect("序列化");
    assert_eq!(wire["type"], json!("error"));
    assert_eq!(wire["error"]["code"], json!(-32001));
    assert!(
        wire["error"]["message"]
            .as_str()
            .expect("message 应当是字符串")
            .contains("bot"),
        "not_found 的消息应当说明是哪个实体：{wire}"
    );
}
