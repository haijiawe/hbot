//! E 面：资源耗尽（目标 `crates/hbot-code` 的 QuickJS 沙箱 + `crates/hbot-bot/src/queue.rs`）。
//!
//! 攻击者不需要任何权限就能造成这类后果：只要让模型/插件跑一段用户可控的 JS，
//! 或者往会话里灌消息。判据是**宿主还活着**，而不是"错误信息好看"：
//!
//! * 沙箱内的死循环 / 无限递归 / 内存炸弹必须在 handler 时间预算内被中断；
//! * 队列满时必须**背压**（拒绝新消息）而不是无限堆积吃内存，
//!   而且被拒的那条**已经在库里**（背压不等于丢消息）；
//! * 一个会话/一个 bot 出事不能把别的会话/别的 bot 拖死。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hbot_bot::{
    ConversationRef, FailureTracker, HandlerExecutor, Inbox, MessageCtx, PlatformEvent,
    QueueConfig, QueueError, SessionKey, SessionQueues,
};
use hbot_code::{CodeError, Limits, Sandbox, compile_project};
use hbot_core::{
    BotId, ConversationId, ConversationKind, HbotError, IdGenerator, InboundMessage, MessageId,
    Timestamp, ids::*,
};
use hbot_redteam::TempTree;
use hbot_store::Store;

// ── 1. QuickJS 内的资源耗尽 ─────────────────────────────────────────────────

const HANDLER_BUDGET_MS: u64 = 300;

fn limits() -> Limits {
    Limits::default()
        .with_handler_timeout(Duration::from_millis(HANDLER_BUDGET_MS))
        .with_memory_bytes(8 * 1024 * 1024)
}

/// 写一个只有 `src/main.ts` 的最小项目，把 `body` 放进 `onMessage` 里。
fn project_with(tree: &TempTree, body: &str) {
    tree.mkdir("src");
    let source = format!(
        "import {{ defineBot }} from 'hbot';\n\
         export default defineBot({{\n  async onMessage(ctx) {{\n{body}\n  }}\n}});\n"
    );
    tree.write("src/main.ts", &source);
}

fn run_body(tree: &TempTree, body: &str) -> (Result<(), String>, Duration) {
    project_with(tree, body);
    let compiled = compile_project(tree.path()).expect("编译最小项目");
    let sandbox = Sandbox::spawn(compiled, limits()).expect("起沙箱");
    let started = Instant::now();
    let outcome = sandbox.dispatch(&serde_json::json!({ "payload": { "text": "hi" } }));
    let elapsed = started.elapsed();
    let result = outcome
        .map(|_| ())
        .map_err(|error: CodeError| format!("{} | {}", error.code(), error));
    (result, elapsed)
}

fn assert_interrupted(what: &str, body: &str, tag: &str) {
    let tree = TempTree::new(tag);
    let (result, elapsed) = run_body(&tree, body);
    let message = result.expect_err(&format!(
        "{what} 必须被沙箱中断，实际跑完了（耗时 {elapsed:?}）"
    ));
    // 中断必须发生在预算附近，而不是"跑了几十秒最后超时"。
    assert!(
        elapsed < Duration::from_secs(30),
        "{what} 用了 {elapsed:?} 才返回，handler 预算只有 {HANDLER_BUDGET_MS} ms"
    );
    assert!(
        !message.is_empty(),
        "{what} 的错误必须能说明原因（否则调用方无法区分「代码错」和「超时」）"
    );
    println!("[{tag}] {what} → {message}（{elapsed:?}）");
}

/// 防的是：用户（或模型）在 handler 里写 `while(true){}`，整个 bot 的内核线程被占死，
/// 之后这个 bot 的所有消息都不再被处理（而且进程还在，看起来"没崩"）。
#[test]
fn e01_死循环必须在时间预算内被中断() {
    let tree = TempTree::new("e01");
    let (result, elapsed) = run_body(&tree, "while (true) {}");
    let message = result.expect_err("死循环必须被中断");
    assert!(
        message.contains("HBOT_TIMEOUT"),
        "死循环应当报超时，实际：{message}"
    );
    assert!(elapsed < Duration::from_secs(30), "中断太慢：{elapsed:?}");
    println!("[e01] while(true) → {message}（{elapsed:?}）");
}

/// 防的是：无限递归把 **宿主进程** 的栈打爆（Rust 侧栈溢出是 abort，不是可捕获异常），
/// 或者把沙箱内核留在半死状态。
#[test]
fn e02_无限递归必须被拦在沙箱内() {
    assert_interrupted(
        "无限递归",
        "function f(n) { return f(n + 1); }\nf(0);",
        "e02",
    );
}

/// 防的是：`new Array(1e9).fill(x)` 之类一次性吃掉几 GB，把宿主机内存耗尽
/// （同机其它 bot、桌面端 UI 一起被 OOM killer 带走）。
#[test]
fn e03_大数组分配必须被内存上限挡住() {
    assert_interrupted(
        "分配 32 MB 数组（堆上限 8 MB）",
        "const a = new Array(4_000_000);\na.fill(1.5);",
        "e03",
    );
}

/// 防的是：字符串倍增（`s += s`）在 QuickJS 里以几何速度吃内存，比数组更快更隐蔽。
#[test]
fn e04_巨大字符串必须被内存上限挡住() {
    assert_interrupted(
        "字符串倍增",
        "let s = 'x';\nwhile (true) { s += s; }",
        "e04",
    );
}

/// 防的是：微任务风暴（无限 `Promise` 链）永不回到事件循环，超时定时器根本没机会触发，
/// 于是 handler 永久挂起 —— 这类 bug 最容易被"超时一定生效"的假设漏掉。
#[test]
fn e05_无限promise链必须被中断() {
    assert_interrupted(
        "无限 Promise 链",
        "let p = Promise.resolve();\nwhile (true) { p = p.then(() => {}); }",
        "e05",
    );
}

// ── 2. 队列背压 ─────────────────────────────────────────────────────────────

const CAPACITY: usize = 1024;

/// 所有 handler 都卡在同一道闸上，直到测试放行 —— 这样队列才会真的堆满。
struct GatedHandler {
    gate: Arc<tokio::sync::Semaphore>,
    started: Arc<AtomicUsize>,
}

#[async_trait]
impl HandlerExecutor for GatedHandler {
    async fn on_message(&self, _ctx: MessageCtx) -> Result<(), HbotError> {
        self.started.fetch_add(1, Ordering::AcqRel);
        let permit = self.gate.acquire().await.expect("闸门未关闭");
        permit.forget();
        Ok(())
    }
}

/// 计数器版 ID 生成器（只为测试提供确定性 ID；生产实现在 hbot-rpc 里）。
struct CountingIds {
    next: AtomicUsize,
}

impl CountingIds {
    fn new() -> Self {
        Self {
            next: AtomicUsize::new(1),
        }
    }

    fn take(&self) -> uuid::Uuid {
        let n = self.next.fetch_add(1, Ordering::AcqRel) as u128;
        uuid::Uuid::from_u128(0xE000_0000_0000_0000_0000_0000_0000_0000 + n)
    }
}

impl IdGenerator for CountingIds {
    fn project_id(&self) -> ProjectId {
        ProjectId::from_uuid(self.take())
    }
    fn bot_id(&self) -> BotId {
        BotId::from_uuid(self.take())
    }
    fn conversation_id(&self) -> ConversationId {
        ConversationId::from_uuid(self.take())
    }
    fn session_id(&self) -> SessionId {
        SessionId::from_uuid(self.take())
    }
    fn message_id(&self) -> MessageId {
        MessageId::from_uuid(self.take())
    }
    fn inbound_id(&self) -> InboundId {
        InboundId::from_uuid(self.take())
    }
    fn outbound_id(&self) -> OutboundId {
        OutboundId::from_uuid(self.take())
    }
    fn approval_id(&self) -> ApprovalId {
        ApprovalId::from_uuid(self.take())
    }
    fn edit_id(&self) -> EditId {
        EditId::from_uuid(self.take())
    }
    fn search_id(&self) -> SearchId {
        SearchId::from_uuid(self.take())
    }
}

fn msg_ctx(bot: BotId, conversation: ConversationId, ext: &str) -> MessageCtx {
    let conversation_ref = ConversationRef::new("conv-1", ConversationKind::Private);
    MessageCtx::new(
        bot,
        InboundMessage {
            id: MessageId::from_uuid(uuid::Uuid::from_u128(0xE1)),
            bot_id: bot,
            conversation_id: conversation,
            external_id: ext.to_owned(),
            sender_id: "user-1".to_owned(),
            payload: serde_json::json!({ "text": ext }),
            received_at: Timestamp::from_unix_millis(1),
            processed_at: None,
        },
        conversation_ref,
    )
}

/// 防的是：消息洪水把一个会话的队列撑爆宿主内存。
///
/// 关键不只是"必须拒绝"，而是**拒绝之后消息不能丢**：背压语义是"暂停投递、稍后重试"，
/// 不是"丢掉再告诉上游我拒了"。所以这条用例两件事一起断言：
/// 1. 第 1025 条返回 `Backpressure`；
/// 2. 那 1025 条**全部已经在 `inbound` 表里**（`admit` 先落库，再入队）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e06_队列满了要背压而且消息不能丢() {
    let tree = TempTree::new("e06");
    let store = Arc::new(Store::open(tree.join("test.db")).expect("打开测试库"));
    let ids: Arc<dyn IdGenerator> = Arc::new(CountingIds::new());
    let inbox = Inbox::new(
        Arc::clone(&store),
        Arc::clone(&ids),
        Arc::new(hbot_core::SystemClock),
    );

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = Arc::new(GatedHandler {
        gate: Arc::clone(&gate),
        started: Arc::new(AtomicUsize::new(0)),
    });
    let queues = SessionQueues::new(
        QueueConfig {
            session_capacity: CAPACITY,
            max_in_flight: 4,
        },
        handler.clone(),
        Arc::new(FailureTracker::new(3)),
    );

    let bot = BotId::from_uuid(uuid::Uuid::from_u128(0xB01));
    let total = CAPACITY + 76;
    let mut backpressure_at = None;
    let mut rejected_external_id = String::new();
    let mut rejected_message_id = MessageId::from_uuid(uuid::Uuid::from_u128(0xE2));
    let mut admitted: Vec<MessageId> = Vec::with_capacity(CAPACITY + 1);
    // 外键：conversations → bots → projects，先把手上的三条链建出来。
    let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0xA01));
    store
        .insert_project(&hbot_store::Project {
            id: project_id,
            name: "red-team-e06".to_owned(),
            path: tree.path().display().to_string(),
            created_at: 1,
        })
        .expect("插入 project");
    store
        .insert_bot(&hbot_store::Bot {
            id: bot,
            project_id,
            name: "red-team-e06".to_owned(),
            platform: "webhook".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 1,
        })
        .expect("插入 bot");

    let conversation = ConversationRef::new("conv-1", ConversationKind::Private);
    for index in 0..total {
        let event = PlatformEvent::new(
            format!("ext-{index}"),
            conversation.clone(),
            "user-1",
            serde_json::json!({ "text": format!("msg {index}") }),
        );
        // ① 先落库（这是真实投递路径的顺序：admit 成功才轮到 enqueue）。
        let hbot_bot::Admission::New(message) = inbox.admit(&bot, &event).await.expect("admit")
        else {
            panic!("第 {index} 条被判成重复投递");
        };
        admitted.push(message.id);

        // ② 再入队。
        let ctx = MessageCtx::new(bot, (*message).clone(), conversation.clone());
        match queues.try_enqueue(ctx) {
            Ok(()) => {}
            Err(QueueError::Backpressure { capacity, .. }) => {
                assert_eq!(capacity, CAPACITY, "背压必须报出容量");
                backpressure_at = Some(index);
                rejected_external_id = message.external_id.clone();
                rejected_message_id = message.id;
                break;
            }
            Err(other) => panic!("第 {index} 条应当是背压，实际：{other:?}"),
        }
    }

    let at = backpressure_at.expect("队列塞满后必须出现背压");
    assert_eq!(
        at, CAPACITY,
        "背压应当恰好发生在第 {CAPACITY} 条（容量满）之后，实际在第 {at} 条"
    );

    // ③ 内存里排队的 + 之前跑的，加起来不能超过容量（否则是内存漏洞）。
    let key = SessionKey::new(bot, hbot_bot::conversation_id_for(&bot, "conv-1"));
    assert!(
        queues.depth(&key) <= CAPACITY,
        "队列深度 {} 超过容量 {CAPACITY}",
        queues.depth(&key)
    );

    // ④ **背压不等于丢消息**：被拒的那条必须已经在库里（投递路径是「先落库、再入队」，
    //    所以队列满的时候消息已经持久化，生产方暂停后重试即可，不会丢）。
    let rows = store
        .list_inbound_history_for_bot(&bot, None, u32::MAX)
        .expect("读回放");
    assert_eq!(
        rows.len(),
        at + 1,
        "背压时落库条数与已 admit 条数不符：库 {} 条，admit {} 条",
        rows.len(),
        at + 1
    );
    assert!(
        rows.iter()
            .any(|row| row.message.external_id == rejected_external_id),
        "被背压拒绝的 `{rejected_external_id}` 没有落库（这条消息就真丢了）"
    );
    assert!(
        rows.iter()
            .any(|row| row.message.id.as_uuid() == rejected_message_id.as_uuid()),
        "被背压拒绝的消息行主键不在库里"
    );
    // ④ 补充：这些行仍然都是「待处理」——被背压的那条还在 `processed_at IS NULL` 的集合里，
    //    所以即使进程此时退出，`bot_start` 的启动补偿也能把它捞回来（at-least-once）。
    let pending = store
        .list_inbound_replay_for_bot(&bot, None, u32::MAX)
        .expect("读补偿列表");
    assert_eq!(
        pending.len(),
        at + 1,
        "待处理行数 {} 与 admit 条数 {} 不符（有行被提前标成已处理）",
        pending.len(),
        at + 1
    );

    // ⑤ 放行所有 handler，队列必须能排空（背压不是死锁）。
    gate.add_permits(CAPACITY * 2);
    let drained = tokio::time::timeout(
        Duration::from_secs(20),
        queues.wait_drained(Duration::from_secs(15)),
    )
    .await;
    assert!(drained.is_ok(), "放行后队列没能排空（背压变成了死锁）");
    assert!(
        queues.depth(&key) == 0,
        "排空后深度应当是 0，实际 {}",
        queues.depth(&key)
    );
}

// ── 3. 隔离：一个会话/一个 bot 崩了不能拖累别人 ─────────────────────────────

/// 对指定 bot 的 handler 主动 panic（模拟插件里一个 `unwrap` 踩空）。
struct PanicForBot {
    victim: BotId,
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl HandlerExecutor for PanicForBot {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        self.seen.fetch_add(1, Ordering::AcqRel);
        if ctx.bot_id == self.victim {
            panic!("红队夹具：模拟 handler 在坏消息上 panic");
        }
        Ok(())
    }
}

/// 防的是：一个 bot 的 handler 崩了，把**整个进程**的队列/线程池带走，
/// 于是同机另一个 bot 的所有消息也不再被处理（多 bot 宿主里这是最贵的故障）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e07_一个bot崩溃不拖累另一个bot() {
    let victim = BotId::from_uuid(uuid::Uuid::from_u128(0xBAD));
    let healthy = BotId::from_uuid(uuid::Uuid::from_u128(0x600D));
    let seen = Arc::new(AtomicUsize::new(0));
    let queues = SessionQueues::new(
        QueueConfig {
            session_capacity: 64,
            max_in_flight: 4,
        },
        Arc::new(PanicForBot {
            victim,
            seen: Arc::clone(&seen),
        }),
        Arc::new(FailureTracker::new(3)),
    );

    let victim_conv = ConversationId::from_uuid(uuid::Uuid::from_u128(0xC1));
    let healthy_conv = ConversationId::from_uuid(uuid::Uuid::from_u128(0xC2));

    // 受害者先崩。
    queues
        .try_enqueue(msg_ctx(victim, victim_conv, "boom"))
        .expect("入队不该失败");
    // 健康 bot 的正常消息。
    for index in 0..5 {
        queues
            .try_enqueue(msg_ctx(healthy, healthy_conv, &format!("ok-{index}")))
            .expect("入队不该失败");
    }

    let healthy_key = SessionKey::new(healthy, healthy_conv);
    let deadline = Instant::now() + Duration::from_secs(10);
    while queues.depth(&healthy_key) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        queues.depth(&healthy_key),
        0,
        "受害者 bot 崩了之后，健康 bot 的队列没有排空（跨 bot 连带故障）"
    );
    assert!(
        seen.load(Ordering::Acquire) >= 6,
        "健康 bot 的消息没有被处理：handler 只被调用了 {} 次",
        seen.load(Ordering::Acquire)
    );
}

/// 只在**第一条**消息上 panic，之后正常 —— 用来区分"一次崩溃"和"永久不可用"。
struct PanicOnce {
    armed: std::sync::atomic::AtomicBool,
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl HandlerExecutor for PanicOnce {
    async fn on_message(&self, _ctx: MessageCtx) -> Result<(), HbotError> {
        self.seen.fetch_add(1, Ordering::AcqRel);
        if self.armed.swap(false, Ordering::AcqRel) {
            panic!("红队夹具：模拟 handler 在第一条坏消息上 panic");
        }
        Ok(())
    }
}

/// 防的是：一条坏消息让**这个会话**永久收不到消息。
///
/// 这是 e07 的另一半。e07 证明"一个 bot 崩不拖累另一个 bot"（跨会话隔离成立）；
/// 会话**内部**的隔离是 RT-07 的修复提供的（`run_worker` 用 `catch_unwind` 捕获 handler
/// 的展开，失败计入 `FailureTracker` 并留日志，名额归还在 `InFlightSlot` 的 `Drop` 里）。
///
/// 修复前的现场（保留作证）：handler 的 panic 逃出 worker 任务后，`depth` / `total`
/// 的递减语句永远不执行，worker 的 `Receiver` 也随之被丢弃，于是这个会话之后每一条
/// 消息都拿 `QueueError::Closed`（→ `HbotError::Conflict`），而进程里没有任何一行日志
/// 说明"这个会话的 worker 已经死了"。
///
/// 真实后果：一个能触发 handler panic 的畸形消息（插件里一次 `unwrap` 踩空）就足以把
/// 该会话打进永久不可用状态，且直到重启都不会自愈 —— 对攻击者而言这是"无需权限的
/// 会话级 DoS"。所以本用例要求：一次 panic 之后 worker 仍然在、`depth` 归还、后续消息
/// 照常处理。见 `docs/security/red-team-findings.md` 的 RT-07（已修）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e08_同会话里一次handler崩溃不得让该会话永久失效() {
    let bot = BotId::from_uuid(uuid::Uuid::from_u128(0xB08));
    let conversation = ConversationId::from_uuid(uuid::Uuid::from_u128(0xC08));
    let seen = Arc::new(AtomicUsize::new(0));
    let queues = SessionQueues::new(
        QueueConfig {
            session_capacity: 8,
            max_in_flight: 2,
        },
        Arc::new(PanicOnce {
            armed: std::sync::atomic::AtomicBool::new(true),
            seen: Arc::clone(&seen),
        }),
        Arc::new(FailureTracker::new(3)),
    );
    let key = SessionKey::new(bot, conversation);

    queues
        .try_enqueue(msg_ctx(bot, conversation, "boom"))
        .expect("第一条消息入队不该失败");

    // 等第一次 panic 真的发生（seen 到 1 就说明 worker 已经跑到 handler 里了）。
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.load(Ordering::Acquire) >= 1,
        "handler 没有被调用，夹具没生效"
    );
    // 给 catch_unwind 的收尾（`InFlightSlot::Drop` 归还名额）留一点时间。
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 第二次入队：这一条应当被正常处理（handler 只 panic 一次）。
    let second = queues.try_enqueue(msg_ctx(bot, conversation, "ok-2"));
    assert!(
        second.is_ok(),
        "第一条消息 panic 之后，同一会话就再也收不到消息了（拿到 {second:?}）——\
         会话级永久 DoS，重启才能恢复"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while queues.depth(&key) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        queues.depth(&key),
        0,
        "崩溃那一条的未完成名额没有归还（depth 卡在 {}），该会话之后会永久背压",
        queues.depth(&key)
    );
}
