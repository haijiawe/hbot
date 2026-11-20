//! 读循环意外结束 → `onStop(crash)` 的端到端测试。
//!
//! 分两半，各自都有别处覆盖，缺的是它们之间那一段：
//!
//! * `hbot-bot` 的 `read_loop` 如实报告退出原因（`ReadLoopExit`），单测在
//!   `crates/hbot-bot/tests/runtime.rs`；
//! * `hbot-code` 的 `onStop(ctx, reason)` 渲染与 `hbot-core` 的 `StopReason::Crash` 各有单测。
//!
//! 本文件钉的是 **`bot_start` 之后真的有人在盯着读循环**：适配器的订阅被关掉（真实世界里
//! 是平台主动断开且客户端不再重连）时，这个 bot 已经收不到任何消息，但登记表里还是
//! `Running` —— 必须有人把它按 `crash` 停掉、跑 `onStop('crash')`、把状态与日志说清楚。
//!
//! 为什么这里要注入假适配器：真 OneBot 客户端在连接断开后会无限重连（`OneBotClient` 的
//! backoff 循环），订阅通道**不会**关闭，所以用真适配器根本造不出这个场景。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hbot_adapters::SystemClock;
use hbot_bot::{AdapterSubscription, BotAdapter, OutboundEnvelope, PlatformEvent};
use hbot_core::{BotId, HbotError};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::app::runtime::AdapterFactory;
use hbot_rpc::app::runtime::config::BotConfig;
use hbot_rpc::dto::{Command, Response};
use hbot_store::Bot;
use tokio::sync::mpsc;

const BOT_UUID: &str = "66666666-6666-4666-8666-666666666666";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-crashcap-{label}-{}-{seq}",
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

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

// ---------------------------------------------------------------------------
// 可关闭的假适配器：订阅通道的发送端握在测试手里
// ---------------------------------------------------------------------------

/// 订阅通道的发送端。测试把它置 `None` 就等于「平台断开了且不再重连」：
/// `recv()` 立刻返回 `None`，读循环走 `ReadLoopExit::SubscriptionClosed`。
type KillSwitch = Arc<Mutex<Option<mpsc::Sender<PlatformEvent>>>>;

struct ClosableAdapter {
    switch: KillSwitch,
    /// 订阅只能发生一次：第二次 subscribe 必须明确报错，而不是换掉一个通道。
    subscribed: Mutex<bool>,
}

#[async_trait]
impl BotAdapter for ClosableAdapter {
    fn id(&self) -> &'static str {
        "closable"
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        {
            let mut subscribed = self.subscribed.lock().expect("订阅锁");
            if *subscribed {
                return Err(HbotError::conflict("closable adapter 只能订阅一次"));
            }
            *subscribed = true;
        }
        // 容量与真实适配器同源，免得这个夹具在容量维度上与被测链路不同。
        let (tx, rx) = mpsc::channel(hbot_bot::supervisor::ADAPTER_EVENT_CAPACITY);
        *self.switch.lock().expect("开关锁") = Some(tx);
        Ok(AdapterSubscription::new(rx))
    }

    async fn send(&self, _envelope: OutboundEnvelope) -> Result<(), HbotError> {
        // 这个夹具不产生入站事件，所以没有出站消息可发；真被调用说明测试的前提不成立。
        Ok(())
    }
}

#[derive(Clone)]
struct ClosableFactory {
    switch: KillSwitch,
}

impl AdapterFactory for ClosableFactory {
    fn create(&self, _bot: &Bot, _config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError> {
        Ok(Arc::new(ClosableAdapter {
            switch: Arc::clone(&self.switch),
            subscribed: Mutex::new(false),
        }))
    }
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn bots_json() -> String {
    format!("{{\"bots\":[{{\"id\":\"{BOT_UUID}\",\"platform\":\"local\",\"name\":\"崩溃收尾\"}}]}}")
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
    bot: BotId,
    switch: KillSwitch,
}

impl Harness {
    async fn new(label: &str, handler_source: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), handler_source).expect("写 handler");
        std::fs::write(
            project.join("hbot.json"),
            "{\"name\":\"crash-bot\",\"platform\":{\"kind\":\"local\"}}\n",
        )
        .expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");

        let switch: KillSwitch = Arc::new(Mutex::new(None));
        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState")
            .with_adapter_factory(Arc::new(ClosableFactory {
                switch: Arc::clone(&switch),
            }));
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
            bot: bot_id(),
            switch,
        }
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

    /// 「平台断开且不再重连」：松开发送端，读循环下一次 `recv()` 得到 `None`。
    fn close_subscription(&self) {
        *self.switch.lock().expect("开关锁") = None;
    }

    fn logs(&self) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&self.bot), 1024)
            .expect("读日志")
            .into_iter()
            .map(|row| row.line)
            .collect()
    }

    fn stored_status(&self) -> Option<String> {
        self.state
            .store()
            .get_bot_status(&self.bot)
            .expect("读 bot 状态")
            .map(|row| row.status)
    }
}

fn assert_ok(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!(
                "{what} 期望成功，实际失败 {}：{}",
                error.code, error.message
            );
        }
    }
}

async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

const HANDLER_STOP_LOG: &str = r#"
import { defineBot, log } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    log('message');
  },
  async onStop(ctx, reason) {
    log('stop:' + ctx.phase + ':' + reason + ':' + ctx.reason);
  },
});
"#;

// ---------------------------------------------------------------------------
// ① 订阅被关掉 = 意外结束 = onStop(crash)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_closed_subscription_stops_the_bot_as_a_crash() {
    let harness = Harness::new("crash", HANDLER_STOP_LOG).await;
    assert_ok(&harness.start().await, "bot_start");

    // 先等它真的跑起来：订阅在 `bot_start` 里建立，这里断言的是「开关已经被交出去」。
    wait_for("订阅建立", || {
        harness.switch.lock().expect("开关锁").is_some()
    })
    .await;
    assert_eq!(harness.stored_status().as_deref(), Some("running"));

    harness.close_subscription();

    // `onStop` 必须拿到 crash（第二个参数与 ctx.reason 两处都对），且是第 4 个字段。
    wait_for("onStop(crash) 的日志", || {
        harness.logs().iter().any(|line| line.starts_with("stop:"))
    })
    .await;
    let line = harness
        .logs()
        .into_iter()
        .find(|line| line.starts_with("stop:"))
        .expect("onStop 日志");
    assert_eq!(
        line, "stop:stop:crash:crash",
        "读循环意外结束必须按 crash 交给 onStop，而不是悄悄停在 Running"
    );

    // 面板上要看得到为什么停：一行点名「读循环意外结束」的 error。
    assert!(
        harness
            .logs()
            .iter()
            .any(|line| line.contains("读循环意外结束")),
        "停机原因要落一行日志，实际日志：{:?}",
        harness.logs()
    );

    // 库里也必须是 stopped：登记表、DB、事件三处说法必须一致。
    wait_for("bot 状态变成 stopped", || {
        harness.stored_status().as_deref() == Some("stopped")
    })
    .await;
}

// ---------------------------------------------------------------------------
// ② 正常停机不算意外（对照组）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_stop_is_not_reported_as_a_crash() {
    let harness = Harness::new("plain", HANDLER_STOP_LOG).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_ok(&harness.stop().await, "bot_stop");

    wait_for("onStop(user) 的日志", || {
        harness.logs().iter().any(|line| line.starts_with("stop:"))
    })
    .await;
    let line = harness
        .logs()
        .into_iter()
        .find(|line| line.starts_with("stop:"))
        .expect("onStop 日志");
    assert_eq!(
        line, "stop:stop:user:user",
        "用户自己停的不许冒充崩溃：那会让清理逻辑跑在正常停机的路径上"
    );
    assert!(
        !harness
            .logs()
            .iter()
            .any(|line| line.contains("读循环意外结束")),
        "正常停机不该留崩溃日志，实际日志：{:?}",
        harness.logs()
    );
}
