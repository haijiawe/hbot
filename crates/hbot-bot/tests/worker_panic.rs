//! RT-07 回归：`HandlerExecutor::on_message` 的 panic 不许杀死会话 worker。
//!
//! 修复前（`crates/hbot-bot/src/queue.rs` 的 `run_worker` 直接 `await` handler，见
//! `docs/security/red-team-findings.md` 的 RT-07）：展开冲出 worker 任务 → `depth` /
//! `total` 永不归还 → 该会话永久背压、`depth != 0` 所以永远回收不掉，后续消息还照样塞进
//! 无人读取的通道；现场没有一行日志，用户只看到"这个会话的机器人突然一句话都不回"。
//!
//! 四条用例分别钉住：会话继续工作、名额归还、panic 计入失败降级、日志不泄漏正文。
//! 去掉修复（临时把 `catch_unwind` 拿掉）时四条都会红——第 1 条红在 `try_enqueue` 的
//! 断言上（`QueueError::Closed`），第 2/3/4 条红在 `wait_until` 的 5s 超时上。

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use common::wait_until;
use hbot_bot::{
    ConversationRef, FailureTracker, HandlerExecutor, MessageCtx, QueueConfig, SessionKey,
    SessionQueues,
};
use hbot_core::{
    BotId, ConversationId, ConversationKind, HbotError, InboundMessage, MessageId, Timestamp,
};
use uuid::Uuid;

/// 日志抓取缓冲。`tracing` 的全局订阅者是进程级的，只能装一次，所以四条用例共用一份；
/// 断言只做 `contains` / `!contains`，并发执行安全（哨兵只在下面那条用例里出现）。
type LogBuffer = Arc<Mutex<Vec<u8>>>;

fn bot_id() -> BotId {
    BotId::from_uuid(Uuid::from_u128(0xb07))
}

fn conversation_id() -> ConversationId {
    ConversationId::from_uuid(Uuid::from_u128(0xc07))
}

fn session_key() -> SessionKey {
    SessionKey::new(bot_id(), conversation_id())
}

/// 造一条入站：`payload.text = text`（用于"日志不许含正文"的哨兵）。
fn ctx(seq: u128, external_id: &str, text: &str) -> MessageCtx {
    let bot = bot_id();
    let message = InboundMessage {
        id: MessageId::from_uuid(Uuid::from_u128(
            0x7a07_0000_0000_0000_0000_0000_0000_0000 | seq,
        )),
        bot_id: bot,
        conversation_id: conversation_id(),
        external_id: external_id.to_owned(),
        sender_id: "user-1".to_owned(),
        payload: serde_json::json!({ "text": text }),
        received_at: Timestamp::from_unix_millis(1),
        processed_at: None,
    };
    MessageCtx::new(
        bot,
        message,
        ConversationRef::new("rt07", ConversationKind::Group),
    )
}

/// 按**调用序号**（0 起）panic 的 handler；其余调用把 `external_id` 记进 `executed`，
/// 这样"第 2 条真的被执行了"是可断言的，而不是只看任务还活着。
struct ScriptedHandler {
    panic_calls: Vec<usize>,
    calls: AtomicUsize,
    executed: Mutex<Vec<String>>,
}

impl ScriptedHandler {
    fn panicking_on(panic_calls: Vec<usize>) -> Self {
        Self {
            panic_calls,
            calls: AtomicUsize::new(0),
            executed: Mutex::new(Vec::new()),
        }
    }

    /// handler 被调用了几次（含 panic 的那几次——计数在 panic 之前自增）。
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    /// 成功执行完的 `external_id`，按执行顺序。
    fn executed(&self) -> Vec<String> {
        self.executed
            .lock()
            .expect("executed lock poisoned")
            .clone()
    }
}

#[async_trait]
impl HandlerExecutor for ScriptedHandler {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        let call = self.calls.fetch_add(1, Ordering::AcqRel);
        if self.panic_calls.contains(&call) {
            // 故意 panic：模拟插件里一次 `unwrap` 踩空。payload 里不含消息正文。
            panic!("rt07 fixture: handler panicked on purpose (call #{call})");
        }
        self.executed
            .lock()
            .expect("executed lock poisoned")
            .push(ctx.external_id().to_owned());
        Ok(())
    }
}

fn queues_with(handler: Arc<ScriptedHandler>, capacity: usize, threshold: u32) -> SessionQueues {
    queues_with_tracker(handler, capacity, Arc::new(FailureTracker::new(threshold)))
}

fn queues_with_tracker(
    handler: Arc<ScriptedHandler>,
    capacity: usize,
    failures: Arc<FailureTracker>,
) -> SessionQueues {
    SessionQueues::new(
        QueueConfig {
            session_capacity: capacity,
            max_in_flight: 2,
        },
        handler,
        failures,
    )
}

#[derive(Clone)]
struct CaptureWriter(LogBuffer);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// 装一个把日志写进内存的全局订阅者（一次性），返回那块缓冲。
fn capture_logs() -> &'static LogBuffer {
    static BUFFER: OnceLock<LogBuffer> = OnceLock::new();
    BUFFER.get_or_init(|| {
        let buffer: LogBuffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(Arc::clone(&buffer)))
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // 只允许装一次；装不上说明环境里已经有全局订阅者，下面"日志里有 handler
        // panicked"的断言会直接失败，不会假绿。
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
}

fn captured(buffer: &LogBuffer) -> String {
    String::from_utf8_lossy(&buffer.lock().expect("log lock poisoned")).into_owned()
}

/// RT-07 要求 1：panic 只吞掉那一条消息，worker 继续处理同一会话的后续消息。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_handler_does_not_kill_the_session() {
    let handler = Arc::new(ScriptedHandler::panicking_on(vec![0]));
    let queues = queues_with(Arc::clone(&handler), 8, 64);

    queues
        .try_enqueue(ctx(1, "evt-boom", "boom"))
        .expect("第一条应当入队成功");
    wait_until("第一条消息进入 handler 并 panic", || {
        handler.calls() >= 1
    })
    .await;

    // 关键：worker 还活着，所以这条不会被 `QueueError::Closed`（sender 与活着的
    // receiver 一一对应）拒绝，也不是被背压拒绝。
    let second = queues.try_enqueue(ctx(2, "evt-ok", "ok"));
    assert!(
        second.is_ok(),
        "panic 之后同一会话应当还能入队（worker 不许死），实际 {second:?}"
    );
    wait_until("第二条消息真的被执行", || {
        handler.executed().len() == 1
    })
    .await;
    assert_eq!(
        handler.executed(),
        vec!["evt-ok".to_owned()],
        "panic 只该吞掉那一条消息，后续消息必须照常执行"
    );
    assert!(
        handler.calls() >= 2,
        "worker 应当继续从通道取消息，实际只调用了 {} 次",
        handler.calls()
    );
    assert_eq!(
        queues.depth(&session_key()),
        0,
        "两条都处理完了，深度必须归零"
    );
    queues
        .shutdown(Duration::from_secs(2))
        .await
        .expect("worker 应当能正常退出");
}

/// RT-07 要求 4：panic 之后 `depth` / `total` 必须回到 0，名额一个不少地还回来。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_handler_returns_the_session_slot() {
    const CAPACITY: usize = 4;
    let handler = Arc::new(ScriptedHandler::panicking_on(vec![0]));
    let queues = queues_with(Arc::clone(&handler), CAPACITY, 64);

    queues
        .try_enqueue(ctx(1, "evt-boom", "boom"))
        .expect("第一条应当入队成功");
    // `depth == 0` 只可能在 panic 被捕获、`InFlightSlot` 的 `Drop` 跑完之后出现；
    // 修复前它永远是 1，这里会 5s 超时。
    wait_until("panic 展开被处理后名额归还", || {
        handler.calls() >= 1 && queues.depth(&session_key()) == 0
    })
    .await;

    assert_eq!(
        queues.depth(&session_key()),
        0,
        "panic 那条的会话名额没有归还（depth 泄漏）"
    );
    assert_eq!(
        queues.total_depth(),
        0,
        "panic 那条的全局未完成计数没有归还（total 泄漏）"
    );

    // 名额必须**一个不少**地还回来：容量 4 的会话现在能连续收 4 条而不背压。
    // 这条断言让"depth 只少减了一部分"也能被抓住。
    for seq in 0..CAPACITY {
        let queued = queues.try_enqueue(ctx(10 + seq as u128, &format!("evt-{seq}"), "work"));
        assert!(
            queued.is_ok(),
            "第 {seq} 条被拒（{queued:?}）：panic 泄漏了会话名额"
        );
    }
    wait_until("容量条消息全部执行完", || {
        handler.executed().len() == CAPACITY
    })
    .await;
    assert_eq!(handler.executed().len(), CAPACITY);
    assert_eq!(queues.depth(&session_key()), 0);
    assert_eq!(queues.total_depth(), 0);
    queues
        .shutdown(Duration::from_secs(2))
        .await
        .expect("worker 应当能正常退出");
}

/// RT-07 要求 3：panic 计入失败，连续失败到阈值时触发既有的降级（与 `Err` 同口径）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_handler_counts_as_a_failure() {
    const THRESHOLD: u32 = 3;
    let logs = capture_logs();
    let tracker = Arc::new(FailureTracker::new(THRESHOLD));
    let handler = Arc::new(ScriptedHandler::panicking_on(vec![0, 1, 2]));
    let queues = queues_with_tracker(Arc::clone(&handler), 8, Arc::clone(&tracker));

    for seq in 0..THRESHOLD {
        queues
            .try_enqueue(ctx(u128::from(seq), &format!("evt-{seq}"), "boom"))
            .expect("入队应当成功");
    }
    wait_until("连续 panic 达到阈值并降级", || {
        tracker.is_degraded()
    })
    .await;

    assert_eq!(
        tracker.consecutive(),
        THRESHOLD,
        "panic 必须走 `record_failure` 累计连续失败"
    );
    assert!(tracker.is_degraded(), "达到阈值后必须处于降级状态");
    assert_eq!(
        tracker.threshold(),
        THRESHOLD,
        "降级阈值不该被 panic 路径改写"
    );
    wait_until("degrade 日志已写出", || {
        captured(logs).contains("bot degraded")
    })
    .await;
    // 降级不等于计数泄漏：worker 仍然要归还名额。
    assert_eq!(queues.depth(&session_key()), 0);
    assert_eq!(queues.total_depth(), 0);
    queues
        .shutdown(Duration::from_secs(2))
        .await
        .expect("worker 应当能正常退出");
}

/// RT-07 要求 2 + 5：panic 必须留一条带 `bot` / `conversation` 的 error 日志、带上
/// panic 的说明，但**不许**把消息正文写进去（入站原文可能含用户隐私）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_does_not_log_message_content() {
    const SECRET_SENTINEL: &str = "SECRET-SENTINEL-12345";
    let logs = capture_logs();
    let handler = Arc::new(ScriptedHandler::panicking_on(vec![0]));
    let queues = queues_with(Arc::clone(&handler), 8, 64);

    let body = format!("正文里带哨兵 {SECRET_SENTINEL}，日志不许出现它");
    queues
        .try_enqueue(ctx(1, "evt-secret", &body))
        .expect("入队应当成功");
    wait_until("panic 的 error 日志已写出", || {
        captured(logs).contains("handler panicked")
    })
    .await;

    let logs = captured(logs);
    assert!(
        logs.contains("handler panicked"),
        "panic 必须留一条 error 日志（修复前的现场是'一行都没有'）：\n{logs}"
    );
    assert!(
        logs.contains("bot="),
        "日志必须带 `bot` 字段，运维才定位得到是哪个 bot：\n{logs}"
    );
    assert!(
        logs.contains("conversation="),
        "日志必须带 `conversation` 字段：\n{logs}"
    );
    assert!(
        logs.contains("rt07 fixture: handler panicked on purpose"),
        "panic 的说明不许被吞掉（downcast 成 &str/String 就该打印）：\n{logs}"
    );
    assert!(
        !logs.contains(SECRET_SENTINEL),
        "日志里出现了消息正文（入站原文可能含用户隐私）：\n{logs}"
    );
    queues
        .shutdown(Duration::from_secs(2))
        .await
        .expect("worker 应当能正常退出");
}
