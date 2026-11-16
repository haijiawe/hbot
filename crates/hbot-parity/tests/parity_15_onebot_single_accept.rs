//! parity #15：一次 `bot_start` 对 OneBot 平台只开**一条** WS 连接（probe → handoff）。
//!
//! 缺陷背景：`OneBotAdapter::subscribe()` 过去先跑一次 `probe_connection()`（连上即断，
//! 只为在启动前把配错的地址/token/证书当场报出来），**再** `OneBotClient::start()` 开
//! 常驻连接 —— 平台侧一次启动看到**两次** accept。修复后探测拿到的连接直接交给常驻
//! 客户端（probe → handoff），一次启动只有一条连接。
//!
//! 本文件把「连了几次」变成可断言的数字：假网关自己数 accept。
//!
//! 1. 启动成功 → accept **1**（修复前是 2：探测 + 真客户端）；
//! 2. 探测失败（网关 accept 之后立刻关掉，WS 升级失败）→ `bot_start` 返回 `-32002`，
//!    且 accept 仍然是 **1** —— 失败也只连一次，不再退化成后台反复重连。
//!
//! 假网关必须**循环 accept**：只 accept 一次的话，多出来的那条连接永远排在 backlog 里，
//! 最后以 RST 收场（`os error 10054`），于是「连了几次」会被判错成一次 —— 这是
//! `onebot_probe.rs` 注释里记过的坑。

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hbot_adapters::SystemClock;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};
use serde_json::json;
use support::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

/// 本文件自己的 bot 标识（与 `parity_14` 的夹具区分开，两套用例互不串味）。
const BOT_UUID: &str = "55555555-5555-4555-8555-555555555515";
const SELF_ID: i64 = 10_015;

/// 「不该出现的第二条连接」的观察窗口：修复前那条多出来的连接是**稍后一点**才到
/// （真客户端的 supervise 任务和探测并发），读太早会漏。
const SETTLE: Duration = Duration::from_millis(300);

const HANDLER: &str = r#"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('pong');
  },
});
"#;

// ---------------------------------------------------------------------------
// 夹具：已打开项目 + bots.json 的 AppState（**真的** OneBot 适配器工厂）
// ---------------------------------------------------------------------------

struct Harness {
    /// 只是持有到用例结束（`Drop` 时删目录）。
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str, ws_url: &str) -> Self {
        let dir = TempDir::new(label);
        let project = dir.join("project");
        support::write_bot_script(&project, HANDLER);
        let bots = json!({ "bots": [{ "id": BOT_UUID, "url": ws_url }] });
        dir.write(
            "project/bots.json",
            &serde_json::to_string_pretty(&bots).expect("序列化 bots.json"),
        );

        let state = Arc::new(
            AppState::open(dir.join("data"), Arc::new(SystemClock), Arc::new(UuidIds))
                .expect("AppState::open"),
        );
        state.open_project(&project).await.expect("open_project");
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
        }
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: bot_id() })
            .await
    }

    async fn stop(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStop { bot_id: bot_id() })
            .await
    }

    /// 库里记录的 bot 状态：启动成功后必须是 `running`（连接真的交到了客户端手里）。
    fn stored_status(&self) -> Option<String> {
        self.state
            .store()
            .get_bot_status(&bot_id())
            .expect("读 bot 状态")
            .map(|row| row.status)
    }
}

fn bot_id() -> hbot_core::BotId {
    hbot_core::BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

fn error_of(response: &Response) -> (i32, String) {
    match response {
        Response::Error { error } => (error.code, error.message.clone()),
        other => panic!("期望 Error 响应，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 会自己数 accept 的假 OneBot 网关
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Behavior {
    /// 正常 OneBot 11：完成 WS 升级、立刻推 `meta_event/lifecycle`，然后挂着收帧。
    OneBot,
    /// accept 之后立刻关连接：WS 升级必然失败，用来测「失败也只连一次」。
    DropsHandshake,
}

struct CountingGateway {
    addr: SocketAddr,
    accepts: Arc<AtomicU32>,
    task: tokio::task::JoinHandle<()>,
}

impl CountingGateway {
    async fn spawn(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定假网关端口");
        let addr = listener.local_addr().expect("读取假网关端口");
        let accepts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&accepts);
        let task = tokio::spawn(async move {
            // 必须**循环** accept（见文件头）。每条连接各自数一次，探测/重连都算。
            while let Ok((stream, _peer)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(stream, behavior));
            }
        });
        Self {
            addr,
            accepts,
            task,
        }
    }

    /// 到此刻为止 accept 过多少条 TCP 连接。
    fn accepts(&self) -> u32 {
        self.accepts.load(Ordering::SeqCst)
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/onebot/v11/ws", self.addr)
    }
}

impl Drop for CountingGateway {
    fn drop(&mut self) {
        // 监听任务是个死循环，只能 abort；不要在这里 await。
        self.task.abort();
    }
}

async fn serve(stream: TcpStream, behavior: Behavior) {
    match behavior {
        Behavior::DropsHandshake => {
            // 拿起就放：客户端连 HTTP 升级响应都读不到（RST/EOF），探测必然失败。
            drop(stream);
        }
        Behavior::OneBot => {
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            // 真的 OneBot 11 实现连上第一帧就是 `meta_event/lifecycle`（sub_type=connect）。
            let lifecycle = json!({
                "post_type": "meta_event",
                "meta_event_type": "lifecycle",
                "sub_type": "connect",
                "time": 1,
                "self_id": SELF_ID,
            });
            if socket
                .send(Message::Text(lifecycle.to_string()))
                .await
                .is_err()
            {
                return;
            }
            // 挂着收帧：连接保持打开，客户端不会触发重连。
            while let Some(Ok(_)) = socket.next().await {}
        }
    }
}

/// 等过观察窗口再读计数：让「修复前那条稍后到达的第二条连接」有机会被记账。
async fn accepts_after_settle(gateway: &CountingGateway) -> u32 {
    tokio::time::sleep(SETTLE).await;
    gateway.accepts()
}

// ---------------------------------------------------------------------------
// 1. 启动成功：一次 bot_start = 一条连接
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_bot_start_opens_exactly_one_connection() {
    let gateway = CountingGateway::spawn(Behavior::OneBot).await;
    let harness = Harness::new("onebot-single-accept", &gateway.ws_url()).await;

    match harness.start().await {
        Response::Ok { .. } => {}
        other => panic!("协议正常的网关必须启动成功，实际 {other:?}"),
    }

    // 连接已经交到客户端手里（不是"探测完就丢"）：状态必须是 running。
    assert_eq!(
        harness.stored_status().as_deref(),
        Some(hbot_store::BOT_STATUS_RUNNING),
        "启动成功后 bot 状态必须是 running"
    );

    let accepts = accepts_after_settle(&gateway).await;
    eprintln!("[parity_15] 成功启动路径 accept 计数 = {accepts}（期望 1；修复前 2）");
    assert_eq!(
        accepts, 1,
        "一次 bot_start 只能有一条 WS 连接（探测连接已 handoff 给常驻客户端）；\
         修复前这里是 2（一次性探测 + 真客户端）"
    );

    let _ = harness.stop().await;
}

// ---------------------------------------------------------------------------
// 2. 探测失败：-32002，且同样只连一次
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_probe_still_dials_only_once() {
    // 网关照常 accept 但立刻关连接 —— 探测的 WS 升级必然失败，而 accept 计数照记，
    // 于是「失败时连了几次」也变成可断言的数字。
    let gateway = CountingGateway::spawn(Behavior::DropsHandshake).await;
    let harness = Harness::new("onebot-failed-probe", &gateway.ws_url()).await;

    let (code, message) = error_of(&harness.start().await);
    assert_eq!(code, -32002, "探测失败是配置类失败（invalid）：{message}");
    assert!(
        message.contains(&gateway.addr.to_string()),
        "原因里必须带上用户填的地址，实际：{message}"
    );
    assert!(
        message.contains("ms"),
        "原因里要写清超时上限，实际：{message}"
    );

    let accepts = accepts_after_settle(&gateway).await;
    eprintln!("[parity_15] 探测失败路径 accept 计数 = {accepts}（期望 1；失败也只拨一次）");
    assert_eq!(
        accepts, 1,
        "探测失败时也只能连一次：探测拿到的那条连接失败就报错，\
         不许再有客户端在后台重连（修复前这里是 2 起）"
    );

    // 失败之后不许留下"运行中"的僵尸。
    assert_eq!(
        harness.stored_status().as_deref(),
        Some(hbot_store::BOT_STATUS_ERROR),
        "探测失败要把 bot 状态标成 error"
    );
}
