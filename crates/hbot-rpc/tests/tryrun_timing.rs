//! S13：用**产品路径**量「新项目模板 → 看到第一条回复」的耗时，并验证同一会话 100 条消息严格保序。
//!
//! 这个文件只做两件事，两件都不绕过产品实现：
//!
//! 1. **A 项（计时）**：`hbot_code::manifest::scaffold`（`Command::ProjectCreate` 的 Phase 1
//!    内部就是它）+ `Command::ProjectOpen` + `Command::BotStart` + `Command::BotInject`，等到
//!    run log 里出现 `[reply] ` 行。平台是 `local`（不联网），适配器是生产默认的
//!    `PlatformRouter`，执行器是真的 `SandboxHandler`（QuickJS）——没有假适配器、没有 mock。
//! 2. **B 项（保序）**：`bot_start` 之后连续快速 `bot_inject` 100 条（文本 `#1`..`#100`），
//!    轮询 `store.list_logs` 直到 100 条 `[reply] ` 或 60s 超时；断言顺序严格是 `#1..#100`。
//!    再做 10 条「每条立刻等回复」的慢速对照，证明快慢两种节奏都不乱序。
//!
//! ## 三条口径，别混
//!
//! * **进程已启动后的耗时**：本文件量的**全部**是这个口径（测试进程已在跑，`AppState` 已在
//!   进程内打开）。这也是 spike 文档里那张表的口径。
//! * **含进程冷启动**：本文件量不到 —— 测试进程里没有"启动一个新的 `hbot` 进程"这回事。
//!   这一段由已存在的 `scripts/headless-startup-probe.ps1` 实测，数字贴在
//!   `docs/spikes/S13-tryrun-timing.md`。
//! * **LLM 生成代码的时间**：本文件**完全不覆盖**。模板自带 `src/main.ts`，整条链路没有一次
//!   模型调用；人类写需求、等模型生成、点 UI 的那段时间本来也不在任何 `cargo test` 里。
//!
//! ## 运行
//!
//! ```text
//! cargo test -p hbot-rpc --test tryrun_timing -- --nocapture
//! ```
//!
//! 必须带 `--nocapture`：分段的中位/最大、100 条的实际序列都打在 stdout 上。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Event, Response};
use serde_json::Value;
use tokio::sync::broadcast;

/// 本地试跑用的固定 bot id（与 `tests/local_runtime.rs` 同一份值，便于对照）。
const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// `local` 平台的试跑会话外部 id（`bot_start` 会幂等建这一行）。
const LOCAL_EXTERNAL_ID: &str = "local:default";

/// run log 里回复行的前缀（真源是 `hbot_adapters` 的 `REPLY_PREFIX`）。
const REPLY_PREFIX: &str = "[reply] ";

/// A 项每个口径跑几轮（每轮一个新的临时目录 + 一个新的 `AppState`）。
const ROUNDS: usize = 5;

/// B 项：快速连发的条数。
const BURST_MESSAGES: usize = 100;

/// B 项：慢速对照的条数。
const SLOW_MESSAGES: usize = 10;

/// 单条回复的等待上限（含首次 `compile_project_debug` 与 QuickJS 冷启动）。
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// 100 条回复全部出现的等待上限（任务书要求 60s）。
const BURST_TIMEOUT: Duration = Duration::from_secs(60);

/// 四个用例互斥执行。
///
/// 计时用的中位数不能被兄弟用例的 CPU / 磁盘争用污染；串行的代价（整份测试多花十几秒）
/// 远小于一次被邻居拖慢后得出的错误结论。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// 临时目录
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-s13-{label}-{}-{seq}", std::process::id()));
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

// ---------------------------------------------------------------------------
// 样本与报表
// ---------------------------------------------------------------------------

struct Samples {
    name: &'static str,
    values: Vec<Duration>,
}

impl Samples {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            values: Vec::new(),
        }
    }

    fn push(&mut self, value: Duration) {
        self.values.push(value);
    }

    /// 中位数：n 为奇数时取正中间那一个（本轮 n = 5，不涉及插值）。
    fn median(&self) -> Duration {
        let mut sorted = self.values.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    fn max(&self) -> Duration {
        *self.values.iter().max().expect("每个分段至少有一个样本")
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// 打一张"分段 / 中位 / 最大 / n"的表。
fn report(title: &str, segments: &[&Samples]) {
    println!("\n========== {title} ==========");
    println!(
        "host = {} / arch = {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "cpu = {:?} 逻辑核",
        std::thread::available_parallelism().map(|n| n.get())
    );
    println!("分段 | 中位(ms) | 最大(ms) | n");
    for segment in segments {
        println!(
            "{} | {:.1} | {:.1} | {}",
            segment.name,
            ms(segment.median()),
            ms(segment.max()),
            segment.values.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 产品路径的调用封装
// ---------------------------------------------------------------------------

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("解析 bot uuid"))
}

/// `local` 平台的最小 `bots.json`：只有 `id` + `platform`，没有 url、没有凭据。
fn local_config() -> String {
    format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"S13 计时"}}]}}"#)
}

/// 打开一个全新的 `AppState`（数据目录给的是本轮临时目录），返回生产用的 `Dispatcher`。
fn open_state(round_dir: &Path) -> (Arc<AppState>, Dispatcher) {
    let state = AppState::open(
        round_dir.join("data"),
        Arc::new(SystemClock),
        Arc::new(UuidIds),
    )
    .expect("打开 AppState");
    let state = Arc::new(state);
    let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
    (state, dispatcher)
}

fn assert_ok(response: &Response) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!("期望成功，实际失败 {}：{}", error.code, error.message)
        }
    }
}

async fn dispatch_ok(dispatcher: &Dispatcher, command: Command) -> Response {
    let response = dispatcher.dispatch(command).await;
    assert_ok(&response);
    response
}

/// 取 `Response::Ok` 的 `data`；失败用例已在 [`dispatch_ok`] 里 panic 过。
fn data_of(response: &Response) -> Value {
    match response {
        Response::Ok { data } => data.clone().unwrap_or(Value::Null),
        Response::Error { error } => {
            panic!("期望成功，实际失败 {}：{}", error.code, error.message)
        }
    }
}

/// `bot_start` 幂等建好的 `local:default` 会话行 id。
fn local_conversation_id(state: &AppState, bot: &BotId) -> ConversationId {
    state
        .store()
        .get_conversation_by_external(bot, LOCAL_EXTERNAL_ID)
        .expect("读 local:default 会话")
        .unwrap_or_else(|| {
            panic!("bot_start 之后应存在 external_id = {LOCAL_EXTERNAL_ID} 的会话行")
        })
        .id
}

/// run log 里所有 `[reply] ` 行，**按写入序**（旧 → 新）。
///
/// `Store::list_logs` 是"最新在前"（按 `logs.id` 降序取前 limit 条），做顺序断言前必须先反转。
fn reply_rows(state: &AppState, bot: &BotId) -> Vec<hbot_store::LogRow> {
    let mut rows = state.store().list_logs(Some(bot), 1000).expect("读日志表");
    rows.reverse();
    rows.into_iter()
        .filter(|row| row.source == "bot" && row.line.starts_with(REPLY_PREFIX))
        .collect()
}

fn reply_texts(state: &AppState, bot: &BotId) -> Vec<String> {
    reply_rows(state, bot)
        .into_iter()
        .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
        .collect()
}

/// 等「下一条 `[reply] ` 事件」。
///
/// 走的是 `EventBus` 上的 `Event::RunLog` —— IDE 日志面板读的同一条流。选它而不是轮询
/// `store.list_logs`，是为了让"从注入到回复可见"这段不被 25ms 的轮询粒度污染；
/// `local` 适配器是先落库再发事件（`record_lines`），所以事件到达时日志行一定已经在了。
async fn next_reply_event(receiver: &mut broadcast::Receiver<Event>, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("等待下一条 [reply] 事件超时（{}s）", timeout.as_secs());
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Err(_) => panic!("等待下一条 [reply] 事件超时（{}s）", timeout.as_secs()),
            // 4096 容量的总线在"两条消息"这个量级上不可能落后；真落后了就是别处的 bug。
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                panic!("事件流落后 {skipped} 条（EventBus 容量不足或消费者停摆）");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => panic!("事件总线已关闭"),
            Ok(Ok(event)) => {
                if let Event::RunLog { line, .. } = event
                    && let Some(text) = line.strip_prefix(REPLY_PREFIX)
                {
                    return text.to_owned();
                }
            }
        }
    }
}

/// 轮询 `store.list_logs`，直到出现至少 `expected` 条 `[reply] ` 行或超时（任务书的 B 项口径）。
async fn wait_for_reply_rows(
    state: &AppState,
    bot: &BotId,
    expected: usize,
    timeout: Duration,
) -> Vec<hbot_store::LogRow> {
    let deadline = Instant::now() + timeout;
    loop {
        let rows = reply_rows(state, bot);
        if rows.len() >= expected {
            return rows;
        }
        if Instant::now() >= deadline {
            let first = rows.first().map_or("<无>", |row| row.line.as_str());
            let last = rows.last().map_or("<无>", |row| row.line.as_str());
            panic!(
                "等待 {expected} 条 [reply] 行超时（{}s）：实际 {} 条，首条 {first:?}，末条 {last:?}",
                timeout.as_secs(),
                rows.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 从回复正文里取出 `#n` 的 n（模板 handler 回显 `收到：#n`）。
fn index_of(text: &str) -> usize {
    let marker = text
        .rfind('#')
        .unwrap_or_else(|| panic!("回复里应带 #n 序号：{text:?}"));
    text[marker + 1..]
        .trim()
        .parse::<usize>()
        .unwrap_or_else(|_| panic!("回复的序号不是整数：{text:?}"))
}

/// `bot_start` → 注入 `#1` 等首条回复 → 注入 `#2` 等热回复 → `bot_stop`。
///
/// 返回 `(bot_start, 首条回复, 热回复)` 三段耗时。两条时间线都从 `BotInject` 之前起表：
/// 这一段包含「入队 → 会话队列 → `compiled()`（首次 debug 编译）→ QuickJS 执行 →
/// outbox → `local` 适配器写 run log」。
async fn start_and_send_two(
    state: &Arc<AppState>,
    dispatcher: &Dispatcher,
) -> (Duration, Duration, Duration) {
    let bot = bot_id();
    let mut events = state.events().subscribe();

    let started = Instant::now();
    dispatch_ok(dispatcher, Command::BotStart { bot_id: bot }).await;
    let start = started.elapsed();

    let conversation = local_conversation_id(state, &bot);

    let started = Instant::now();
    dispatch_ok(
        dispatcher,
        Command::BotInject {
            bot_id: bot,
            conversation_id: conversation,
            text: "#1".to_owned(),
        },
    )
    .await;
    let text = next_reply_event(&mut events, REPLY_TIMEOUT).await;
    let first = started.elapsed();
    assert_eq!(text, "收到：#1", "模板 handler 应原样回显注入文本");

    // 事件流看到的那一条必须已经在日志表里（`record_lines` 先落库再发事件）。
    let logged = reply_rows(state, &bot);
    assert_eq!(logged.len(), 1, "第一条消息应恰好产出一行回复");
    assert_eq!(
        logged[0].line.strip_prefix(REPLY_PREFIX),
        Some(text.as_str()),
        "事件里的回复与日志表里的必须是同一条"
    );

    let started = Instant::now();
    dispatch_ok(
        dispatcher,
        Command::BotInject {
            bot_id: bot,
            conversation_id: conversation,
            text: "#2".to_owned(),
        },
    )
    .await;
    let text = next_reply_event(&mut events, REPLY_TIMEOUT).await;
    let warm = started.elapsed();
    assert_eq!(text, "收到：#2", "第二条回复应原样回显注入文本");

    // 收尾：不留运行中的 runtime（否则临时目录删除时 store / isolate 还占着文件）。
    dispatch_ok(dispatcher, Command::BotStop { bot_id: bot }).await;

    (start, first, warm)
}

// ---------------------------------------------------------------------------
// A 项：模板 → 第一条回复（分段计时，5 轮）
// ---------------------------------------------------------------------------

/// A 项主口径：脚手架 + 打开项目 + 编译 + `bot_start` + 注入到第一条回复。
///
/// 每轮都是**新的临时目录 + 新的 `AppState`**，跑 5 轮报中位/最大。
///
/// 关于「首次编译」：`project_open` **不编译**（它只开 workspace + 刷 `hbot.d.ts`）。
/// 真正第一次编译发生在第一条消息的处理路径里（`SandboxHandler::compiled()` →
/// `hbot_code::compile_project_debug`，带 `FingerprintCache`），所以它落在
/// 「注入 → 第一条回复」这一段里；与之对照的「注入 → 第二条回复」是热路径（无编译），
/// 两段的差值就是「首次 debug 编译 + QuickJS 冷启动」的代价。另外单独量一条
/// `Command::Diagnostics`（= `hbot_code::compile_project`，release 口径）作为
/// 「编译一个干净项目本身要多久」的产品命令参考。
#[tokio::test]
async fn template_to_first_reply_timing() {
    let _serial = SERIAL.lock().await;

    let mut scaffold = Samples::new("1 脚手架 manifest::scaffold（= project_create 的 Phase 1）");
    let mut open = Samples::new("2 打开项目 project_open（含 refresh_host_dts）");
    let mut compile = Samples::new("3 编译 diagnostics（release，独立口径，不在试跑热路径上）");
    let mut start = Samples::new("4 bot_start（local 适配器 + supervisor）");
    let mut first_reply = Samples::new("5 注入 → 第一条回复（含首次 debug 编译 + QuickJS 冷启动）");
    let mut warm_reply = Samples::new("6 注入 → 第二条回复（热路径，无编译）");
    let mut steps_total = Samples::new("合计（in-process，上述 1+2+4+5 之和）");
    let mut round_wall = Samples::new("整轮墙钟（含夹具：写 bots.json + AppState::open）");

    for round in 1..=ROUNDS {
        let dir = TempDir::new(&format!("timing-{round}"));
        let project = dir.path().join("template");
        let round_clock = Instant::now();

        let started = Instant::now();
        hbot_code::manifest::scaffold(&project, &format!("s13-{round}"))
            .expect("scaffold 一个新项目模板");
        let scaffold_dur = started.elapsed();
        scaffold.push(scaffold_dur);
        assert!(
            project.join("src").join("main.ts").is_file(),
            "scaffold 应生成入口 src/main.ts"
        );

        // `scaffold` 不写部署配置（它不是项目源码）：本地试跑要补一份 `bots.json`。
        // 这是**夹具**动作，不计入任何分段 —— 真实用户是在 UI 里填表单，那段本来就不在本测试里。
        std::fs::write(project.join("bots.json"), local_config()).expect("写 bots.json");

        // 夹具：每轮一个全新的 AppState（新数据目录）。
        let (state, dispatcher) = open_state(dir.path());

        let started = Instant::now();
        dispatch_ok(
            &dispatcher,
            Command::ProjectOpen {
                path: project.display().to_string(),
            },
        )
        .await;
        let open_dur = started.elapsed();
        open.push(open_dur);

        let (start_dur, first_dur, warm_dur) = start_and_send_two(&state, &dispatcher).await;
        start.push(start_dur);
        first_reply.push(first_dur);
        warm_reply.push(warm_dur);

        // 编译参考口径：产品命令里唯一能单独量"编译一个干净项目"的入口。
        let started = Instant::now();
        let response = dispatch_ok(&dispatcher, Command::Diagnostics { path: None }).await;
        let compile_dur = started.elapsed();
        compile.push(compile_dur);
        let diagnostics = data_of(&response);
        assert_eq!(
            diagnostics.as_array().map(Vec::len),
            Some(0),
            "干净模板不应有诊断错误：{diagnostics}"
        );

        steps_total.push(scaffold_dur + open_dur + start_dur + first_dur);
        round_wall.push(round_clock.elapsed());
        println!(
            "第 {round} 轮：脚手架 {:.1} / 打开 {:.1} / 编译 {:.1} / bot_start {:.1} / 首条回复 {:.1} / 热回复 {:.1} / 整轮 {:.1} (ms)",
            ms(scaffold_dur),
            ms(open_dur),
            ms(compile_dur),
            ms(start_dur),
            ms(first_dur),
            ms(warm_dur),
            ms(round_clock.elapsed())
        );
    }

    report(
        "A 口径：新项目模板 → 看到第一条回复（local 平台，进程已启动后）",
        &[
            &scaffold,
            &open,
            &compile,
            &start,
            &first_reply,
            &warm_reply,
            &steps_total,
            &round_wall,
        ],
    );
}

/// A 项的「产品命令」口径：`Command::ProjectCreate` 一条命令就是脚手架 + 落库 + 打开项目。
///
/// 与上一个用例的差别只有第一步：这里量的是一整条 wire 命令，不拆 Phase 1/3；
/// 后面的 `bot_start` / 首条回复 / 热回复完全同一条链路。
#[tokio::test]
async fn project_create_to_first_reply_timing() {
    let _serial = SERIAL.lock().await;

    let mut create = Samples::new("1 project_create（脚手架 + 落库 + 打开项目，一条命令）");
    let mut start = Samples::new("2 bot_start");
    let mut first_reply = Samples::new("3 注入 → 第一条回复（含首次 debug 编译）");
    let mut warm_reply = Samples::new("4 注入 → 第二条回复（热路径）");
    let mut steps_total = Samples::new("合计（in-process，上述 1+2+3 之和）");

    for round in 1..=ROUNDS {
        let dir = TempDir::new(&format!("create-{round}"));
        let (state, dispatcher) = open_state(dir.path());
        let project = dir.path().join("created");

        let started = Instant::now();
        dispatch_ok(
            &dispatcher,
            Command::ProjectCreate {
                path: project.display().to_string(),
                name: format!("created-{round}"),
            },
        )
        .await;
        let create_dur = started.elapsed();
        create.push(create_dur);
        assert!(
            project.join("src").join("main.ts").is_file(),
            "project_create 应生成入口 src/main.ts"
        );
        assert!(
            state.project_root().is_some(),
            "project_create 成功后新项目应当是已打开状态"
        );

        // 夹具：补 `bots.json`（scaffold 与 project_create 都不写部署配置）。
        std::fs::write(project.join("bots.json"), local_config()).expect("写 bots.json");

        let (start_dur, first_dur, warm_dur) = start_and_send_two(&state, &dispatcher).await;
        start.push(start_dur);
        first_reply.push(first_dur);
        warm_reply.push(warm_dur);

        steps_total.push(create_dur + start_dur + first_dur);
        println!(
            "第 {round} 轮：project_create {:.1} / bot_start {:.1} / 首条回复 {:.1} / 热回复 {:.1} (ms)",
            ms(create_dur),
            ms(start_dur),
            ms(first_dur),
            ms(warm_dur)
        );
    }

    report(
        "A 口径（产品命令版）：Command::ProjectCreate → 第一条回复",
        &[&create, &start, &first_reply, &warm_reply, &steps_total],
    );
}

// ---------------------------------------------------------------------------
// B 项：同一会话 100 条快速注入严格保序
// ---------------------------------------------------------------------------

/// 100 条连续快速注入：全部被接受、全部处理完、回复严格按 `#1..#100` 出现。
///
/// 顺序断言用的是 `logs.id`（`AUTOINCREMENT`，写入序）反转后的序列：
/// 收到的 100 行必须恰好对应 `#1..#100`，既不乱序、也不丢、也不重复。
#[tokio::test]
async fn hundred_rapid_injects_stay_in_order() {
    let _serial = SERIAL.lock().await;

    let dir = TempDir::new("burst");
    let project = dir.path().join("template");
    hbot_code::manifest::scaffold(&project, "burst-bot").expect("scaffold 一个新项目模板");
    std::fs::write(project.join("bots.json"), local_config()).expect("写 bots.json");
    let (state, dispatcher) = open_state(dir.path());
    let bot = bot_id();
    dispatch_ok(
        &dispatcher,
        Command::ProjectOpen {
            path: project.display().to_string(),
        },
    )
    .await;
    dispatch_ok(&dispatcher, Command::BotStart { bot_id: bot }).await;
    let conversation = local_conversation_id(&state, &bot);

    // **不等待、不等回复**地连发：这是任务书的"快速连发"口径。
    let burst_started = Instant::now();
    for index in 1..=BURST_MESSAGES {
        let response = dispatcher
            .dispatch(Command::BotInject {
                bot_id: bot,
                conversation_id: conversation,
                text: format!("#{index}"),
            })
            .await;
        // 连发的 100 条都在会话队列容量（1024）之内：每一条都必须被接受。
        assert_ok(&response);
    }
    let enqueued = burst_started.elapsed();

    let rows = wait_for_reply_rows(&state, &bot, BURST_MESSAGES, BURST_TIMEOUT).await;
    let processed = burst_started.elapsed();

    println!(
        "\n100 条快速注入：{BURST_MESSAGES} 条全部入队用时 {:.1} ms；从第一条注入到第 {BURST_MESSAGES} 条回复可见 {:.1} ms",
        ms(enqueued),
        ms(processed)
    );
    assert_eq!(
        rows.len(),
        BURST_MESSAGES,
        "应恰好收到 {BURST_MESSAGES} 行 [reply]，实际 {} 行",
        rows.len()
    );

    // `list_logs` 是"最新在前"，`reply_rows` 已经反转成写入序；再确认一遍 id 严格递增。
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "反转后的日志 id 必须严格递增（写入序），实际首尾 {:?}..{:?}",
        ids.first(),
        ids.last()
    );

    let texts: Vec<String> = rows
        .iter()
        .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
        .collect();
    let indices: Vec<usize> = texts.iter().map(|text| index_of(text)).collect();
    let expected: Vec<usize> = (1..=BURST_MESSAGES).collect();

    let head = &indices[..5.min(indices.len())];
    let tail = &indices[indices.len().saturating_sub(5)..];
    println!(
        "实际序列首 5：{head:?}；末 5：{tail:?}；全文长度 {}",
        indices.len()
    );
    println!("实际序列前 3 行原文：{:?}", &texts[..3.min(texts.len())]);
    println!(
        "实际序列后 3 行原文：{:?}",
        &texts[texts.len().saturating_sub(3)..]
    );

    assert_eq!(
        indices, expected,
        "回复必须严格是 #1..#{BURST_MESSAGES}（无乱序 / 无丢失 / 无重复）；实际首 5 {head:?}，末 5 {tail:?}"
    );

    dispatch_ok(&dispatcher, Command::BotStop { bot_id: bot }).await;
}

/// 慢速对照：每条注入后**立刻等它的回复**，再做下一条（10 条）。
///
/// 目的是证明"慢节奏不乱序"不是靠快速连发的队列缓冲偶然成立的：两种节奏都必须给出
/// `#1..#10` 的严格递增序列。
#[tokio::test]
async fn slow_injects_stay_in_order() {
    let _serial = SERIAL.lock().await;

    let dir = TempDir::new("slow");
    let project = dir.path().join("template");
    hbot_code::manifest::scaffold(&project, "slow-bot").expect("scaffold 一个新项目模板");
    std::fs::write(project.join("bots.json"), local_config()).expect("写 bots.json");
    let (state, dispatcher) = open_state(dir.path());
    let bot = bot_id();
    dispatch_ok(
        &dispatcher,
        Command::ProjectOpen {
            path: project.display().to_string(),
        },
    )
    .await;
    dispatch_ok(&dispatcher, Command::BotStart { bot_id: bot }).await;
    let conversation = local_conversation_id(&state, &bot);

    let mut latencies = Vec::new();
    for index in 1..=SLOW_MESSAGES {
        let started = Instant::now();
        dispatch_ok(
            &dispatcher,
            Command::BotInject {
                bot_id: bot,
                conversation_id: conversation,
                text: format!("#{index}"),
            },
        )
        .await;
        let rows = wait_for_reply_rows(&state, &bot, index, REPLY_TIMEOUT).await;
        latencies.push(started.elapsed());
        assert_eq!(
            rows.len(),
            index,
            "慢速节奏下第 {index} 条注入完成后应恰好有 {index} 行回复"
        );
    }

    let texts = reply_texts(&state, &bot);
    let indices: Vec<usize> = texts.iter().map(|text| index_of(text)).collect();
    let expected: Vec<usize> = (1..=SLOW_MESSAGES).collect();
    let latency_ms: Vec<f64> = latencies.iter().map(|value| ms(*value)).collect();
    println!("\n慢速对照（每条立刻等回复，n = {SLOW_MESSAGES}）单条耗时(ms)：{latency_ms:.1?}");
    println!("实际序列：{indices:?}");
    assert_eq!(
        indices, expected,
        "慢速节奏也必须是 #1..#{SLOW_MESSAGES} 严格递增、无丢失、无重复"
    );

    dispatch_ok(&dispatcher, Command::BotStop { bot_id: bot }).await;
}
