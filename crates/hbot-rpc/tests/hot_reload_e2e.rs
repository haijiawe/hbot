//! S12 热重载端到端验收：**走产品路径**量「改一行代码 → 多久生效」，并钉住
//! 「编译失败时继续服务上一版可跑代码」。
//!
//! 这里没有任何 fake / mock：
//!
//! ```text
//! bot_inject → Dispatcher → 真 SandboxHandler → 真 FingerprintCache（生产 TTL）
//!            → 真 hbot_code::compile_project_debug → 真 QuickJS isolate
//!            → LocalAdapter::send → run log 表里的 `[reply] ` 行
//! ```
//!
//! 被小心避开的三个"测试替身"：
//!
//! 1. **不用** `with_fingerprint_ttls` / `with_debounce`（它们都是 `#[cfg(test)]`，只在
//!    `sandbox_handler.rs` 自己的单测里用）。本文件走 `bot_start` → `SandboxHandler::new`，
//!    拿到的是 `DEFAULT_METADATA_TTL`(2s) / `DEFAULT_CONTENT_FLOOR`(5s) / `HOT_RELOAD_DEBOUNCE`(300ms)。
//! 2. **不用** 假时钟：`AppState::open(.., Arc::new(SystemClock), ..)` 与产品一致。
//!    （注意：指纹缓存的 TTL 用的是 `Instant` 而不是注入的 `Clock`，所以假时钟也影响不到它。）
//! 3. **不手写 handler 源码**：项目由 `hbot_code::manifest::scaffold` 生成，之后只改
//!    `src/main.ts` 里那一行 reply 文案 —— 就是用户按 Ctrl+S 的那个动作。
//!
//! ## 断言为什么长这样（重要，别"优化"成更好看的形状）
//!
//! 生效延迟的**下界**是产品实现决定的，没有上界可言（编译时间随项目线性增长），
//! 所以这里不写"延迟 ≤ 某个很大值"这种断言。写的是可推导的那条：
//!
//! - `FingerprintCache::current_at`（`fingerprint_cache.rs:145-204`）只在
//!   `now - signature_at >= metadata_ttl` 时才重新 stat（`:147-153`）；不刷新时直接返回
//!   上一份指纹（`:186-191`），**一个文件内容都不读**。所以「写完文件」到「新版本被编译」
//!   之间至少要等一次 stat，而那次 stat 距上一次 stat 至少 `DEFAULT_METADATA_TTL`。
//! - 每个 trial 先 `bot_stop` + `bot_start`：这会把 `SandboxHandler`（连同它的
//!   `FingerprintCache`）换成全新的，`signature_at = None` 命中 `_ => true` 分支，
//!   所以**预热那一条消息必定做 stat**。记这条消息的开始时刻与回程时刻，就能把
//!   "上一次 stat 的时刻"夹在 `[warm_started, warm_reply_seen]` 里 —— 于是
//!   `latency >= TTL - (warm_turnaround + 写文件前的空隙)` 是可证明的，不是经验值。
//! - 「写文件后的第一条消息仍是旧版本」只在 `距上次 stat 不足 TTL` 时成立，所以那条断言
//!   带一个显式前提检查（前提不成立就报出实测值并失败，而不是默默放过）。
//!
//! ## 观测手段
//!
//! 回复文本带**唯一**的注入序号（`m0`、`m1`…）：日志表按行数截断（`list_logs(limit)`），
//! 靠"第几条回复"数数是不可靠的，所以一律按 `ends_with("：{text}")` 精确匹配。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::fingerprint_cache::{DEFAULT_CONTENT_FLOOR, DEFAULT_METADATA_TTL};
use hbot_rpc::app::runtime::sandbox_handler::HOT_RELOAD_DEBOUNCE;
use hbot_rpc::dto::{Command, Response};

/// 本文件专用的 bot uuid（与 `local_runtime.rs` 的 `2222…` 区分开，便于看日志归属）。
const BOT_UUID: &str = "44444444-4444-4444-8444-444444444444";

/// 本地试跑用的固定外部会话 ID（与 `hbot-adapters` 的常量必须一字不差）。
const LOCAL_EXTERNAL_ID: &str = "local:default";

/// 回复前缀（与 `hbot-adapters` 的 `REPLY_PREFIX` 一致）。
const REPLY_PREFIX: &str = "[reply] ";

/// 轮询间隔：只影响"观测粒度"，不影响被测系统。
const POLL: Duration = Duration::from_millis(10);

/// **挂死守卫**，不是产品延迟上限。
///
/// 它的作用只有一个：如果实现把消息丢了 / 卡死，用例在 20s 后带着诊断信息失败，
/// 而不是挂到 CI 超时。20s 这个数字不对任何产品行为做断言。
const HANG_GUARD: Duration = Duration::from_secs(20);

/// 日志表的读取条数上限（与 `local_runtime.rs` 一致）。
const LOG_LIMIT: u32 = 256;

/// 生效延迟的采样次数（任务书要求 ≥5）。
const TRIALS: usize = 5;

/// 语法错误的 `src/main.ts`：`defineBot({` 与 `onMessage` 都没闭合。
///
/// 少了 `},\n});` —— 文件在表达式中间结束，`hbot_code` 的解析阶段就会失败，
/// 于是走 `SandboxHandler` 的"编译失败 → 用 last_good 顶上"分支。
const BROKEN_SOURCE: &str = "\
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`编译不过：${ctx.text}`);
";

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
        let path = std::env::temp_dir().join(format!(
            "hbot-hotreload-{label}-{}-{seq}",
            std::process::id()
        ));
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

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

// ---------------------------------------------------------------------------
// 被改的那一行：handler 源码
// ---------------------------------------------------------------------------

/// 每个版本的标记：`v1····`、`v2········`…
///
/// 长度随版本单调增长是**故意的**：`MetaSignature` 是 `(relpath, mtime_nanos, len)`
/// （`fingerprint_cache.rs:58`），改长度等于保证"元数据层面一定能看出差异"，
/// 于是 5s 内容地板（`:52`，只在 len 与 mtime 都没变时才兜底）不会参与这轮测量。
fn marker(version: u32) -> String {
    format!("v{version}{}", "·".repeat(4 * version as usize))
}

/// 生成 `src/main.ts`：正文只有一行不一样 —— `reply(\`{marker}：${ctx.text}\`)`。
fn handler_source(marker: &str) -> String {
    format!(
        "import {{ defineBot, reply }} from 'hbot';\n\
         \n\
         export default defineBot({{\n\
         \x20 async onMessage(ctx) {{\n\
         \x20   reply(`{marker}：${{ctx.text}}`);\n\
         \x20 }},\n\
         }});\n"
    )
}

/// 回复文本 → 版本标记（回复形如 `v2········：m17`）。
fn marker_of(reply: &str) -> &str {
    reply.rsplit_once('：').map_or(reply, |(head, _rest)| head)
}

// ---------------------------------------------------------------------------
// 夹具：一个 scaffold 出来的真项目 + 真 AppState
// ---------------------------------------------------------------------------

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
    /// `src/main.ts` 的绝对路径 —— 用户按 Ctrl+S 改的就是它。
    main: PathBuf,
    /// 注入文本的唯一序号：回复靠它精确匹配，不靠日志行号。
    text_seq: AtomicU64,
}

impl Harness {
    /// scaffold 一个真项目 → 把 `hbot.json` 改成 `local` → 补 `bots.json` → 打开项目。
    async fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("hot-bot");
        hbot_code::manifest::scaffold(&project, "hot-bot").expect("scaffold 一个真项目");
        // scaffold 的 hbot.json 写的是 onebot11 + ws://127.0.0.1:3001（要外接协议端）。
        // 这里只换平台声明，不动其他字段 —— 与"用户在 IDE 里把平台切成 local"等价。
        std::fs::write(
            project.join("hbot.json"),
            "{\"name\":\"hot-bot\",\"platform\":{\"kind\":\"local\"}}\n",
        )
        .expect("写 hbot.json");
        // scaffold 不写 bots.json（部署配置，不是项目源码）：本地试跑自己补一份。
        std::fs::write(
            project.join("bots.json"),
            format!(r#"{{"bots":[{{"id":"{BOT_UUID}","platform":"local","name":"热重载"}}]}}"#),
        )
        .expect("写 bots.json");

        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot: bot_id(),
            main: project.join("src").join("main.ts"),
            text_seq: AtomicU64::new(0),
        }
    }

    fn store(&self) -> &Arc<hbot_store::Store> {
        self.state.store()
    }

    /// 写 `src/main.ts`，返回**写完成**的时刻（fs::write 返回即文件已关闭）。
    fn write_main(&self, source: &str) -> Instant {
        std::fs::write(&self.main, source).expect("写 src/main.ts");
        Instant::now()
    }

    /// 写新版本并使用它的标记。
    fn write_version(&self, version: u32) -> Instant {
        self.write_main(&handler_source(&marker(version)))
    }

    /// `src/main.ts` 的元数据指纹 `(len, mtime_nanos)` —— 与 `MetaSignature` 同源的两项，
    /// 用来证明"这次改动在元数据层面可见"，顺带留下延迟归因的物证。
    fn main_meta(&self) -> (u64, u128) {
        let meta = std::fs::metadata(&self.main).expect("读 src/main.ts 元数据");
        let mtime = meta
            .modified()
            .expect("读 mtime")
            .duration_since(UNIX_EPOCH)
            .expect("mtime 晚于 epoch");
        (meta.len(), mtime.as_nanos())
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: self.bot })
            .await
    }

    async fn stop(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: self.bot })
            .await
    }

    async fn start_ok(&self) {
        assert_ok(&self.start().await, "bot_start");
    }

    async fn stop_ok(&self) {
        assert_ok(&self.stop().await, "bot_stop");
    }

    /// 重启 bot：`SandboxHandler`（连同 `FingerprintCache`）会被整体换新，
    /// 于是下一条消息必定做一次 stat —— 这是 TTL 下界断言的能量来源。
    async fn restart_ok(&self) {
        self.stop_ok().await;
        self.start_ok().await;
    }

    fn conversation_id(&self) -> ConversationId {
        self.store()
            .get_conversation_by_external(&self.bot, LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .unwrap_or_else(|| {
                panic!("bot_start 之后应存在 external_id = {LOCAL_EXTERNAL_ID} 的会话行")
            })
            .id
    }

    async fn inject(&self, text: &str) -> Response {
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: self.bot,
                conversation_id: self.conversation_id(),
                text: text.to_owned(),
            })
            .await
    }

    fn logs(&self) -> Vec<hbot_store::LogRow> {
        self.store()
            .list_logs(Some(&self.bot), LOG_LIMIT)
            .expect("读日志")
    }

    /// run log 里所有去掉 `[reply] ` 前缀的行。
    fn replies(&self) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    /// `level == "error"` 且包含 `needle` 的日志行（编译失败落在这一档）。
    fn error_lines(&self, needle: &str) -> Vec<String> {
        self.logs()
            .into_iter()
            .filter(|row| row.level == "error" && row.line.contains(needle))
            .map(|row| row.line)
            .collect()
    }

    fn next_text(&self) -> String {
        let seq = self.text_seq.fetch_add(1, Ordering::Relaxed);
        format!("m{seq}")
    }

    /// 注入一条消息，等到它的回复出现，返回回复文本。
    ///
    /// 用唯一文本精确匹配：日志表滚动了也不影响判读。
    async fn inject_and_wait(&self, text: &str) -> String {
        let suffix = format!("：{text}");
        assert_ok(&self.inject(text).await, &format!("bot_inject({text})"));
        let started = Instant::now();
        loop {
            if let Some(reply) = self
                .replies()
                .into_iter()
                .rev()
                .find(|line| line.ends_with(&suffix))
            {
                return reply;
            }
            assert!(
                started.elapsed() < HANG_GUARD,
                "挂死守卫 {HANG_GUARD:?}：注入 `{text}` 的回复没有出现在 run log 里。\
                 最近 8 条日志：{:?}",
                self.logs()
                    .into_iter()
                    .rev()
                    .take(8)
                    .map(|row| format!("[{}] {}", row.level, row.line))
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// 注入一条消息并断言它由 `version` 这一版服务。
    async fn inject_expecting(&self, version: u32) -> String {
        let text = self.next_text();
        let reply = self.inject_and_wait(&text).await;
        assert_eq!(
            marker_of(&reply),
            marker(version),
            "期望 {version} 版服务 `{text}`，实际回复 {reply:?}"
        );
        reply
    }
}

// ---------------------------------------------------------------------------
// 断言助手
// ---------------------------------------------------------------------------

fn assert_ok(response: &Response, what: &str) {
    if let Response::Error { error } = response {
        panic!(
            "{what} 期望成功，实际失败 {}：{}",
            error.code, error.message
        );
    }
}

fn median_ms(latencies: &[Duration]) -> f64 {
    let mut values: Vec<f64> = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    values.sort_by(f64::total_cmp);
    // n = TRIALS 是奇数，取正中间那个就是中位数。
    values[values.len() / 2]
}

// ---------------------------------------------------------------------------
// 用例 1：真实生效延迟（n = 5）
// ---------------------------------------------------------------------------

/// 量「`src/main.ts` 写完成 → 新文案出现在 run log」的耗时，n = 5，报中位与最大。
///
/// 每个 trial 都从"全新指纹缓存"起步（`restart_ok`），否则上一轮 trial 的
/// stat 时刻不可知，下界断言就退化成了经验值。
#[tokio::test]
async fn hot_reload_latency_after_saving_main_ts() {
    println!(
        "生产门槛（未做任何测试期覆盖）：metadata TTL = {DEFAULT_METADATA_TTL:?} \
         (fingerprint_cache.rs:47)、content floor = {DEFAULT_CONTENT_FLOOR:?} (:52)、\
         去抖窗口 = {HOT_RELOAD_DEBOUNCE:?} (sandbox_handler.rs:73)"
    );

    let harness = Harness::new("latency").await;
    // v1 先落地并跑通一条，之后每个 trial 在前一版基础上写下一版。
    harness.write_version(1);
    harness.start_ok().await;
    let first = harness.inject_expecting(1).await;
    println!("基线：{first:?}");
    harness.stop_ok().await;

    let mut latencies: Vec<Duration> = Vec::with_capacity(TRIALS);
    let mut stale_counts: Vec<usize> = Vec::with_capacity(TRIALS);

    for trial in 1..=TRIALS {
        let previous = trial as u32;
        let next = previous + 1;

        harness.restart_ok().await;

        // 预热：全新缓存 ⇒ 这条消息必定 stat（`signature_at = None` → `_ => true`），
        // 它的回程时刻把"上一次 stat 的时刻"夹住。
        let warm_started = Instant::now();
        let warm_reply = harness.inject_expecting(previous).await;
        let warm_turnaround = warm_started.elapsed();
        println!("  trial {trial}: 预热回程 {warm_turnaround:?}（{warm_reply:?}）");

        // Ctrl+S：这是被测动作的起点。
        let meta_before = harness.main_meta();
        let written = harness.write_version(next);
        let meta_after = harness.main_meta();
        assert_ne!(
            meta_before, meta_after,
            "写入必须改变 src/main.ts 的 (len, mtime)：否则元数据签名不变，5s 内容地板才会兜底，\
             这一轮的归因就变了。before = {meta_before:?}，after = {meta_after:?}"
        );

        // 一直注入，直到某条消息拿到新版本。期间每条回复都必须是上一版。
        // 顺便记每条消息的"派发→看到回复"回程：切换那一条的回程里含着
        // 去抖等待 + 编译，缓存命中的那些只有一次 dispatch —— 这是把 2s 归因到
        // 元数据 TTL、把多出来的几百毫秒归因到去抖窗口的实测依据。
        let mut stale = 0usize;
        let mut stale_round_trips: Vec<Duration> = Vec::new();
        let mut switched: Option<(Instant, String, Duration)> = None;
        while written.elapsed() < HANG_GUARD {
            let text = harness.next_text();
            let msg_started = Instant::now();
            let reply = harness.inject_and_wait(&text).await;
            let round_trip = msg_started.elapsed();
            if marker_of(&reply) == marker(next) {
                switched = Some((Instant::now(), reply, round_trip));
                break;
            }
            assert_eq!(
                marker_of(&reply),
                marker(previous),
                "新版本生效之前只应看到 {previous} 版（缓存命中或 last-good），实际 {reply:?}"
            );
            stale += 1;
            stale_round_trips.push(round_trip);
        }
        let (seen_at, reply, switch_round_trip) = switched.unwrap_or_else(|| {
            panic!("挂死守卫 {HANG_GUARD:?}：写完 src/main.ts 后 {TRIALS} 轮内没看到新版本")
        });
        let latency = seen_at.duration_since(written);
        println!(
            "  trial {trial}: 生效 {:.1} ms（stale 注入 {stale} 条，warm {warm_turnaround:?}，\
             新文案 {reply:?}；切换那条回程 {switch_round_trip:?}，\
             stale 回程中位 {:.1} ms）",
            latency.as_secs_f64() * 1000.0,
            median_ms(&stale_round_trips[..stale_round_trips.len().min(64)])
        );

        // 下界推导（见文件头）：新版本需要一次 stat，那次 stat 距上次 stat ≥ TTL，
        // 而上次 stat 就发生在预热消息里。所以
        //   latency ≥ TTL - (warm_turnaround + 写文件前的空隙)
        // 这个下界是产品常数推出来的，不是"看起来差不多"。
        let gap_after_warm = written.saturating_duration_since(warm_started + warm_turnaround);
        let lower_bound = DEFAULT_METADATA_TTL.saturating_sub(warm_turnaround + gap_after_warm);
        assert!(
            latency >= lower_bound,
            "trial {trial}：生效 {latency:?} < 由 metadata TTL 推出的下界 {lower_bound:?}\
             （TTL {DEFAULT_METADATA_TTL:?}，预热回程 {warm_turnaround:?}，空隙 {gap_after_warm:?}）。\
             说明这次改动在元数据 TTL 到期前就被看见了 —— 与 `current_at` 的刷新条件不符。"
        );

        // 「保存后的第一条消息仍是旧版本」只在距上次 stat 不足 TTL 时成立（预热太慢则不可预期）。
        assert!(
            stale > 0 || warm_turnaround + gap_after_warm >= DEFAULT_METADATA_TTL,
            "trial {trial}：写入后第一条消息就拿到了新版本，而它距上次 stat 只有 \
             {:?}（< TTL {DEFAULT_METADATA_TTL:?}）。指纹缓存不该在这个时刻刷新。",
            warm_turnaround + gap_after_warm
        );

        latencies.push(latency);
        stale_counts.push(stale);
    }

    let median = median_ms(&latencies);
    let max = latencies
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .fold(f64::MIN, f64::max);
    println!(
        "n = {TRIALS}：中位 {median:.1} ms，最大 {max:.1} ms，最小 {:.1} ms；\
         stale 注入数 {:?}；门槛 metadata TTL = {} ms + 去抖 {} ms",
        latencies
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(f64::MAX, f64::min),
        stale_counts,
        DEFAULT_METADATA_TTL.as_millis(),
        HOT_RELOAD_DEBOUNCE.as_millis(),
    );
    // 只断言"确实量到了 TRIALS 次"，不对延迟本身设上界：编译耗时随项目增长，
    // 给它套一个数字等于把测试写成实现快照（任务书明确禁止）。
    assert_eq!(latencies.len(), TRIALS);

    harness.restart_ok().await;
    harness.inject_expecting(TRIALS as u32 + 1).await;
    harness.stop_ok().await;
}

// ---------------------------------------------------------------------------
// 用例 2：编译失败 → 继续服务上一版 → 修好 → 恢复
// ---------------------------------------------------------------------------

/// 写入语法错误的 `src/main.ts` 之后：
///
/// - 每条消息**仍然按上一版可跑代码回复**（不是 `Response::Error`，不是空回复）；
/// - 日志里出现 `生成代码编译失败：…` 的错误行，且**恰好一条**（同一份坏指纹不反复编译）；
/// - 没有 `消息 … 处理失败` 行（消息没被丢在半路）；
/// - 修好文件后恢复成新版本，且不新增编译错误行。
#[tokio::test]
async fn compile_error_keeps_serving_last_good_then_recovers() {
    let harness = Harness::new("last-good").await;
    harness.write_version(1);
    harness.start_ok().await;
    let baseline = harness.inject_expecting(1).await;
    println!("基线：{baseline:?}");

    // 1) 写坏文件（Ctrl+S 一个语法错误）
    let broken_at = harness.write_main(BROKEN_SOURCE);
    let broken_meta = harness.main_meta();
    println!("写入语法错误的 src/main.ts：{broken_meta:?}");

    // 2) 一直注入，直到日志里出现编译错误行；期间每条回复都必须是上一版。
    let mut served_while_broken: Vec<String> = Vec::new();
    let (error_seen_at, detecting_round_trip) = loop {
        let text = harness.next_text();
        let msg_started = Instant::now();
        let reply = harness.inject_and_wait(&text).await;
        let round_trip = msg_started.elapsed();
        assert_eq!(
            marker_of(&reply),
            marker(1),
            "编译失败期间必须继续服务上一版可跑代码（实际回复 {reply:?}）"
        );
        assert!(
            !reply.trim().is_empty(),
            "编译失败期间不能给空回复（`{text}`）"
        );
        served_while_broken.push(reply);
        if !harness.error_lines("生成代码编译失败").is_empty() {
            break (Instant::now(), round_trip);
        }
        assert!(
            broken_at.elapsed() < HANG_GUARD,
            "挂死守卫 {HANG_GUARD:?}：坏代码一直没被编译（日志里没有 `生成代码编译失败` 行）"
        );
    };
    println!(
        "编译失败被看见：{:?}（写完成 → 错误行落库），期间 {} 条消息全部由 v1 服务，\
         触发编译的那条回程 {detecting_round_trip:?}（含去抖等待 + 一次失败的编译）",
        error_seen_at.duration_since(broken_at),
        served_while_broken.len()
    );

    // 3) 坏代码窗口的硬断言
    let errors = harness.error_lines("生成代码编译失败");
    for line in &errors {
        println!("  编译错误行：{line}");
    }
    assert_eq!(
        errors.len(),
        1,
        "同一份坏指纹不该被反复编译：期望恰好 1 条编译错误行，实际 {} 条",
        errors.len()
    );
    assert!(
        errors[0].len() > "生成代码编译失败：".len(),
        "编译错误行应在冒号后带上失败原因，实际 {errors:?}"
    );
    assert!(
        harness.error_lines("处理失败").is_empty(),
        "消息不该被丢在半路，却有处理失败行：{:?}",
        harness.error_lines("处理失败")
    );

    // 4) 坏代码还挂着的时候再发 3 条：仍然上一版，且不触发新编译。
    for _ in 0..3 {
        harness.inject_expecting(1).await;
    }
    assert_eq!(
        harness.error_lines("生成代码编译失败").len(),
        1,
        "坏代码没变，不该再编译一次"
    );

    // 5) 修好（v2）→ 等新版本出现
    let fixed_at = harness.write_version(2);
    let mut recovered: Option<(Instant, String)> = None;
    while fixed_at.elapsed() < HANG_GUARD {
        let text = harness.next_text();
        let reply = harness.inject_and_wait(&text).await;
        if marker_of(&reply) == marker(2) {
            recovered = Some((Instant::now(), reply));
            break;
        }
        assert_eq!(
            marker_of(&reply),
            marker(1),
            "修好之后、新版本编译完成之前仍应由 v1 服务，实际 {reply:?}"
        );
    }
    let (recovered_at, reply) = recovered
        .unwrap_or_else(|| panic!("挂死守卫 {HANG_GUARD:?}：修好 src/main.ts 后没恢复成 v2"));
    println!(
        "修复生效：{:?}（写完成 → v2 文案 {reply:?}）",
        recovered_at.duration_since(fixed_at)
    );

    // 6) 恢复之后：稳定在新版本，且编译错误行不新增（历史行留着）。
    harness.inject_expecting(2).await;
    assert_eq!(
        harness.error_lines("生成代码编译失败").len(),
        1,
        "修好的代码不该再产生编译错误行"
    );
    harness.stop_ok().await;
}

// ---------------------------------------------------------------------------
// 用例 3：钉住"保存后立刻发的那一条仍由缓存版本服务"
// ---------------------------------------------------------------------------

/// 这条用例把上面两根断言背后的机制单独钉一次：
///
/// **元数据 TTL 之内**保存之后立刻注入，第一条消息一定还是旧版本 —— 因为
/// `FingerprintCache::current_at` 在 `now - signature_at < metadata_ttl` 时
/// 连 stat 都不做，直接返回缓存指纹（`fingerprint_cache.rs:147-153`、`:186-191`）。
///
/// 前提检查写在断言里：如果这台机器慢到「预热回程 + 写入后立即注入」已经超过 TTL，
/// 用例会带着实测值失败，而不是静默放过（那条断言就没有意义了）。
#[tokio::test]
async fn first_message_right_after_save_is_served_by_the_cached_version() {
    let harness = Harness::new("stale-first").await;
    harness.write_version(1);
    harness.start_ok().await;

    let warm_started = Instant::now();
    harness.inject_expecting(1).await;
    let warm_turnaround = warm_started.elapsed();

    // 写 v2，然后**立刻**注入一条（模拟"保存后马上在试跑框里发一条"）。
    let written = harness.write_version(2);
    let text = harness.next_text();
    let first_started = Instant::now();
    let reply = harness.inject_and_wait(&text).await;
    let first_round_trip = first_started.elapsed();
    let elapsed = written.elapsed();

    assert!(
        warm_turnaround + elapsed < DEFAULT_METADATA_TTL,
        "用例前提不成立：预热回程 {warm_turnaround:?} + 保存后立即注入 {elapsed:?} = {:?} \
         已经超过 metadata TTL {DEFAULT_METADATA_TTL:?}，这条消息本来就该看到新版本，\
         无法用它证明缓存陈旧。",
        warm_turnaround + elapsed
    );
    assert_eq!(
        marker_of(&reply),
        marker(1),
        "保存（{elapsed:?} 前）后 TTL 内的第一条消息应由缓存版本 v1 服务，实际 {reply:?}。\
         指纹缓存只在 `now - signature_at >= metadata_ttl` 时才重新 stat\
         （fingerprint_cache.rs:147-153）。"
    );
    println!(
        "保存后 {elapsed:?}（< TTL {DEFAULT_METADATA_TTL:?}）注入：仍然 {reply:?} \
         —— 陈旧窗口成立（这条消息回程只有 {first_round_trip:?}，没有编译、没有去抖等待：\
         走的是 `current_at` 的缓存返回分支）"
    );

    // 但"晚一点"必须生效，否则就成了"永远不生效"。
    let mut switched = None;
    while written.elapsed() < HANG_GUARD {
        let text = harness.next_text();
        let msg_started = Instant::now();
        let r = harness.inject_and_wait(&text).await;
        let round_trip = msg_started.elapsed();
        if marker_of(&r) == marker(2) {
            switched = Some((Instant::now(), r, round_trip));
            break;
        }
        assert_eq!(marker_of(&r), marker(1), "过渡期内只该看到 v1，实际 {r:?}");
    }
    let (seen_at, r, switch_round_trip) = switched.unwrap_or_else(|| {
        panic!("挂死守卫 {HANG_GUARD:?}：保存后一直没切到 v2，陈旧窗口变成了永不过期")
    });
    println!(
        "同一进程内稍后注入：{:.1} ms 后切到 {r:?}（切换那条回程 {switch_round_trip:?}，\
         含去抖等待 + 一次编译）",
        seen_at.duration_since(written).as_secs_f64() * 1000.0
    );
    harness.stop_ok().await;
}
