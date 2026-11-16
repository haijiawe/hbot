//! 事件通道**满了**会发生什么：背压（阻塞）还是丢事件？——定性 + 可观测断言。
//!
//! ## 定性（先看结论，再看作证）
//!
//! 适配器的事件通道是**有界**的（容量来自 `OneBotConfig::event_capacity`，`hbot-bot` 侧对应
//! `ADAPTER_EVENT_CAPACITY = 256`，两边都是"有界 + `send().await`"）。满的时候
//! `dispatch_frame` 里的 `events_tx.send(...).await` **挂起读者任务**，于是
//! `OneBotClient::next_event()` 循环不再从 socket 读 —— 这就是背压，**不丢事件**。
//!
//! 为什么不能依赖"容量够大所以不会满"：`hbot-bot/src/adapter.rs` 的
//! `AdapterSubscription::channel` 文档写死了一条契约 ——「适配器写不进去时必须自己施加背压
//! （`docs/02-architecture.md` §10），不得静默丢弃」。所以这里测的是**契约**，不是巧合：
//!
//! | 问题 | 答案 | 本文件的哪条断言作证 |
//! |---|---|---|
//! | 丢事件吗 | 不丢 | 容量 1、12 条连发，消费者读到的正好 12 条且顺序不变 |
//! | 阻塞还是缓冲 | 阻塞（背压一路传导到 socket） | 容量 1、8 MB 灌入、消费者停读 400 ms 时服务器**发不完** |
//! | 会不会重复 | 不会 | 读满之后 300 ms 内没有第 13 条 |
//! | 满了之后会话还在吗 | 在 | 排空后还能 `send_group_msg` 并拿回 echo 响应 |
//! | 容量 0 呢 | 启动时就报错（不静默变成"无限缓冲"） | `start` 返回 `Unsupported`，消息里带 "capacity" |
//!
//! 注意：`hbot-adapters` 不能依赖 `hbot-bot`（层级倒置，`xtask/src/deps.rs` 会拦），所以这里
//! 用 `OneBotConfig::with_capacities` 直接压适配器侧那条通道。`hbot-bot` 的 256 是**同一套
//! 机制**（`AdapterSubscription::channel(256)` + `send().await`），队列满时的背压另有
//! `hbot-bot/tests/runtime.rs::full_queue_backpressures_without_losing_messages` 覆盖。
//!
//! 为什么"丢事件"这件事在这里没有反面用例：实现里根本没有丢的分支 —— 唯一的
//! `send` 失败分支是**接收端已经消失**（`events_tx.send(...).is_err()`），那时客户端会
//! warn `"onebot event receiver dropped; stopping session"` 并结束会话，属于"大声停机"而不是
//! 静默丢弃。接收端只有 `OneBotClient` 自己持有，集成测试无法单独把它丢掉，所以那条分支
//! 由该行代码的日志兜底，不在这里造。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use hbot_adapters::AdapterError;
use hbot_adapters::platforms::onebot::backoff::Backoff;
use hbot_adapters::platforms::onebot::client::{OneBotClient, OneBotConfig};

/// 一条群消息事件；`message_id` 与正文都可控，便于断言顺序与"不重复"。
fn group_event(text: &str, message_id: u64) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1731652800,
        "self_id": 10000,
        "message_id": message_id,
        "user_id": 20001,
        "group_id": 30001,
        "raw_message": text,
        "font": 0,
        "sender": { "user_id": 20001, "nickname": "阿远" },
        "message": [{ "type": "text", "data": { "text": text } }]
    })
    .to_string()
}

/// mock 服务器：**先连发 `count` 条事件**，发完之后转入 echo 循环（对每个动作回响应）。
///
/// 返回 `(url, 已写完的事件条数)`。第二个值就是"服务器有没有被客户端的背压卡住"的探针：
/// 客户端不读 → 事件通道满 → 读者任务停在 `send().await` → 不再读 socket → socket 缓冲满 →
/// 服务器的 `ws.send()` 挂起 → 计数停在某个小于 `count` 的值上。
async fn spawn_flood_server(count: usize, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    let url = format!("ws://{address}");
    let written = Arc::new(AtomicUsize::new(0));
    let written_in_task = Arc::clone(&written);

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws handshake");

        for index in 0..count {
            let text = if body.is_empty() {
                format!("e{index}")
            } else {
                // 填充成 `body`：靠体积把 socket 缓冲撑满，让"服务器被卡住"可观测。
                format!("e{index}:{body}")
            };
            ws.send(WsMessage::text(group_event(&text, index as u64 + 1)))
                .await
                .expect("send event");
            written_in_task.fetch_add(1, Ordering::SeqCst);
        }

        // 发完事件后照常服务动作请求（证明会话没被"通道满"搞坏）。
        while let Some(frame) = ws.next().await {
            let Ok(frame) = frame else { break };
            let Some(text) = frame.to_text().ok() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let Some(echo) = value.get("echo").and_then(Value::as_str) else {
                continue;
            };
            let response = json!({
                "status": "ok",
                "retcode": 0,
                "data": { "message_id": 424242 },
                "echo": echo,
            });
            ws.send(WsMessage::text(response.to_string()))
                .await
                .expect("send response");
        }
    });

    (url, written)
}

/// 被测配置：心跳关掉（读者任务被背压挂起时不该被"心跳超时"抢先拆掉连接）。
fn saturated_config(url: String, event_capacity: usize) -> OneBotConfig {
    OneBotConfig::new(url)
        .with_heartbeat_timeout(None)
        .with_backoff(Backoff::new(
            Duration::from_millis(5),
            Duration::from_millis(20),
        ))
        .with_capacities(event_capacity, 8)
}

/// 容量 1 + 12 条连发 + 慢消费者：一条都不丢、顺序不变、会话照常可用。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_event_channel_loses_nothing_and_keeps_the_session_alive() {
    const COUNT: usize = 12;
    let (url, written) = spawn_flood_server(COUNT, "").await;
    let mut client = OneBotClient::start(saturated_config(url, 1)).expect("client starts");

    let mut texts = Vec::with_capacity(COUNT);
    for _ in 0..COUNT {
        // 每读一条停 20 ms：读者任务会实实在在地撞满容量 1 的通道。
        tokio::time::sleep(Duration::from_millis(20)).await;
        let event = tokio::time::timeout(Duration::from_secs(10), client.next_event())
            .await
            .expect("event arrives in time")
            .expect("channel must stay open");
        texts.push(event.text);
    }

    let expected: Vec<String> = (0..COUNT).map(|index| format!("e{index}")).collect();
    assert_eq!(texts, expected, "容量满必须只造成阻塞，不许丢事件或乱序");
    assert_eq!(
        written.load(Ordering::SeqCst),
        COUNT,
        "服务器最终确实把 COUNT 条都写完了"
    );

    // 不重复：读满之后不该冒出第 13 条。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), client.next_event())
            .await
            .is_err(),
        "通道里不该残留重复事件"
    );

    // 会话还在：饱和之后照常发动作并拿回响应。
    let data = client
        .send_group_msg(30001, "通道满过之后还在", Some("1"))
        .await
        .expect("session survives a saturated event channel");
    assert_eq!(data.get("message_id").and_then(Value::as_i64), Some(424242));

    client.shutdown().await;
}

/// 背压确实传导到了 socket：消费者停读时，服务器**发不完**这 8 MB。
///
/// 这一条是"阻塞"与"无限缓冲"的分水岭：如果实现是无界缓冲（或满了丢事件且丢掉后继续读），
/// 服务器会在消费者完全不读的情况下把 500 条全部写完，`written == COUNT`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_stalls_the_socket_instead_of_buffering_without_bound() {
    const COUNT: usize = 500;
    // 16 KB/条 × 500 ≈ 8 MB：远大于回环 socket 缓冲 + 客户端通道容量 1。
    let body: &'static str = Box::leak("x".repeat(16 * 1024).into_boxed_str());
    let (url, written) = spawn_flood_server(COUNT, body).await;
    let mut client = OneBotClient::start(saturated_config(url, 1)).expect("client starts");

    // 完全停读 400 ms。
    tokio::time::sleep(Duration::from_millis(400)).await;
    let stalled = written.load(Ordering::SeqCst);
    assert!(
        stalled < COUNT,
        "消费者停读时服务器必须被卡住（说明背压传导到了 socket），实际写完 {stalled}/{COUNT}"
    );

    // 恢复读取：剩下的照常全部到达，顺序不变（抽首尾与计数，避免 8 MB 全量比较）。
    let mut seen = 0_usize;
    let mut first = None;
    let mut last = String::new();
    while seen < COUNT {
        let event = tokio::time::timeout(Duration::from_secs(20), client.next_event())
            .await
            .expect("event arrives after draining")
            .expect("channel must stay open");
        if first.is_none() {
            first = Some(event.text.clone());
        }
        last = event.text;
        seen += 1;
    }
    assert_eq!(seen, COUNT, "背压期间不许丢事件");
    assert!(
        first.as_deref().is_some_and(|text| text.starts_with("e0:")),
        "首条必须是 e0"
    );
    assert!(
        last.starts_with("e499:"),
        "末条必须是 e499，实际：{}",
        &last[..last.len().min(16)]
    );
    // 服务器计数在 `send().await` 返回后才 +1，客户端拿到最后一条可能比它早一两微秒；
    // 所以这里**轮询到相等**而不是立刻断言（立刻断言在高负载机器上会偶发红，实测过）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while written.load(Ordering::SeqCst) < COUNT && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        written.load(Ordering::SeqCst),
        COUNT,
        "排空后服务器写完全部事件"
    );

    client.shutdown().await;
}

/// 容量 0 不是"无限缓冲"，而是启动就报错（有界契约不能被配置绕开）。
#[tokio::test]
async fn zero_event_capacity_is_rejected_with_a_reason() {
    let error = OneBotClient::start(OneBotConfig::new("ws://127.0.0.1:1").with_capacities(0, 1))
        .expect_err("容量 0 必须被拒绝");
    assert!(
        matches!(error, AdapterError::Unsupported { .. }),
        "容量 0 属于〔不支持〕而不是〔传输失败〕：{error:?}"
    );
    assert!(
        error.to_string().contains("capacity"),
        "错误信息要指出是 capacity 的问题：{error}"
    );
}
