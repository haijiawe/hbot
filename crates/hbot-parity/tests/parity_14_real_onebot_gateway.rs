//! parity #14 — **真协议**端到端：手搓的 OneBot 11 正向 WebSocket 网关 → 真
//! `hbot_adapters` `OneBotClient` → 真 `OneBotAdapter` → 真 supervisor + 每会话队列
//! + outbox → 真 QuickJS handler → 真 `send_group_msg` / `send_private_msg` 打回网关。
//!
//! 本套件其它用例（parity_01/02/04/05/09/13）把平台适配器换成 [`support::FakeAdapter`]，
//! 验证的是 supervisor 那一侧；协议那一侧（WS 握手鉴权、echo 配对、心跳、消息段
//! 归一化/反归一化）从来没有被测过 —— 本文件补的就是这条缝。
//!
//! 假货只有「QQ 服务器」：`tests/support/gateway.rs` 按 OneBot 11 的定义手搓了一个
//! 进程内网关（只用 `serde_json` + `tokio-tungstenite`，没有第三方 OneBot 库）。
//!
//! 覆盖的验收点：
//!
//! 1. 三条群消息按序被 handler 看到，回复按序发出（同会话 FIFO）；
//! 2. `send_group_msg` 的 `group_id` 与 `message` 内容正确（`at` → `@qq`、
//!    `image` → `[图片]` 用的是 adapter 现有的有损渲染规则）；
//! 3. 被动回复的引用上下文是否进了出站 payload —— **按实测行为断言**；
//! 4. 同一 `message_id` 投两次 → handler 只执行一次；
//! 5. token 错 → adapter 判失败（`AdapterError`），不是静默成功。
//!
//! 另外几个用例内的边界：事件不带 `message_id`（兜底键是本地合成键，**不能**当引用
//! id 发回平台，出站不带 `reply` 段）、QQ 官方的非数字 `external_id`（`ROBOT1.0_…`）
//! **仍然**要带 `reply` 段、推完一条就主动断开（adapter 重连后继续）。

mod support;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, Stream, StreamExt};
use hbot_adapters::AdapterError;
use hbot_adapters::onebot::{OneBotConfig, probe_connection};
use hbot_code::Limits;
use hbot_core::{BotId, HbotError, ProjectId};
use hbot_rpc::EventBus;
use hbot_rpc::app::runtime::onebot::{OneBotAdapter, PLATFORM};
use hbot_rpc::app::runtime::outbox_source::StorePendingOutbox;
use serde_json::{Value, json};
use support::gateway::{
    Gateway, GatewayConfig, RETCODE_UNSUPPORTED_ACTION, at_segment, group_message, image_segment,
    private_message, text_segment,
};
use support::{
    DEADLINE, ReplyRelay, TempDir, blocking, bot_spec, build_supervisor, group_event, logs_of,
    open_store, sandbox_executor, seed_project, wait_until, wait_until_async, write_bot_script,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use uuid::Uuid;

const BOT_UUID: u128 = 0xB14;
const PROJECT_UUID: u128 = 0x5151_0014;

/// 网关（= 机器人自己）的 QQ 号。adapter 会按它过滤事件。
const SELF_ID: i64 = 10001;
const GROUP_ID: i64 = 123_456;
const GROUP_USER: i64 = 111;
const PRIVATE_USER: i64 = 777;
/// 消息里被 @ 的那个人（与群号、发送者都不同，避免断言串味）。
const AT_QQ: i64 = 222;

const TOKEN: &str = "onebot-11-real-token";

const MSG_ONE: i64 = 9001;
const MSG_TWO: i64 = 9002;
const MSG_THREE: i64 = 9003;
const MSG_PRIVATE: i64 = 9050;
const MSG_DUP: i64 = 9100;
const MSG_AFTER_DUP: i64 = 9300;
const MSG_RECONNECT: i64 = 9200;
const MSG_AFTER_RECONNECT: i64 = 9201;

/// 第一条群消息里有 `at` 和 `image` 段：`@qq` 与 `[图片]` 的渲染必须在真链路上成立。
const AT_MESSAGE_TEXT: &str = "@222 你好[图片]";

/// handler 只是把看到的东西原样顶回去：这样「出站正文」就等于「adapter 渲染出的
/// 入站正文」，断言不需要自己再发明一套渲染规则。
const HANDLER: &str = r#"
import { defineBot } from 'hbot';
export default defineBot({
  onMessage(ctx) {
    ctx.log('收到：' + ctx.text);
    ctx.reply(ctx.text);
  },
});
"#;

#[tokio::test]
async fn real_onebot_gateway_drives_the_whole_chain() {
    if tokio::time::timeout(DEADLINE, real_onebot_case())
        .await
        .is_err()
    {
        panic!("parity_14：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

async fn real_onebot_case() {
    let dir = TempDir::new("14-onebot-real");
    let project_id = ProjectId::from_uuid(Uuid::from_u128(PROJECT_UUID));
    let bot_id = BotId::from_uuid(Uuid::from_u128(BOT_UUID));
    let project_root = dir.join("project");
    write_bot_script(&project_root, HANDLER);

    let gateway = Gateway::start(GatewayConfig::new(SELF_ID).with_token(TOKEN)).await;
    let url = gateway.ws_url();

    // ---------- ⑤ token 错：adapter 必须当场判失败，不是静默成功 ----------
    //
    // `OneBotClient::start` 是永续重连语义（只会在后台重试 + warn），拿不到「确定
    // 失败」；`probe_connection` 才是「连一次、失败就报」的那条路径。
    let refused = probe_connection(&OneBotConfig::new(url.clone()).with_token("definitely-wrong"))
        .await
        .expect_err("错的 token 居然连通了（网关没有拒绝握手）");
    assert!(
        matches!(refused, AdapterError::Transport { .. }),
        "错的 token 期望 AdapterError::Transport，实际是 {refused:?}"
    );
    assert_eq!(
        gateway.rejected_authorizations(),
        vec![Some("Bearer definitely-wrong".to_owned())],
        "网关应当把被拒的 Authorization 头原样记下来"
    );
    assert_eq!(
        gateway.generation(),
        0,
        "被 401 拒掉的握手不该被算成一条连接"
    );

    // 对 token 能过（探针只连一次就关）。
    probe_connection(&OneBotConfig::new(url.clone()).with_token(TOKEN))
        .await
        .expect("对的 token 应当探测成功");
    wait_until("探测连接被网关记账", || gateway.generation() >= 1).await;
    let probe_generation = gateway.generation();
    wait_until("探测连接已经关闭", || !gateway.live()).await;

    // ---------- 真链路上场：真适配器 + 真 supervisor + 真 QuickJS ----------
    let store = open_store(&dir);
    seed_project(&store, project_id, "E:\\hbot-parity-14").await;
    let events = Arc::new(EventBus::new());
    let (handler, replies_rx) = sandbox_executor(
        Arc::clone(&store),
        events,
        bot_id,
        project_root.clone(),
        Limits::new(),
    );

    let adapter = Arc::new(
        OneBotAdapter::connect(
            OneBotConfig::new(url.clone()).with_token(TOKEN),
            Some(SELF_ID),
        )
        .expect("构造 OneBot 适配器"),
    );
    let supervisor = build_supervisor(
        Arc::clone(&store),
        Arc::clone(&adapter),
        handler,
        bot_spec(bot_id, project_id, PLATFORM),
        |deps| {
            deps.pending_outbox = Arc::new(StorePendingOutbox::new(Arc::clone(&deps.store)));
            deps.outbox_config.max_attempts = 1_000;
            deps.outbox_config.base_backoff = Duration::from_millis(20);
            deps.outbox_config.max_backoff = Duration::from_millis(100);
            deps.outbox_config.send_timeout = Duration::from_secs(2);
            deps.shutdown_timeout = Duration::from_secs(2);
        },
    );
    let relay = ReplyRelay::spawn(replies_rx, Arc::clone(&supervisor));
    supervisor.start().await.expect("supervisor 启动");

    // 适配器自己的客户端连上来（探针之后的那一条连接）。
    //
    // 注意这里会出现**两条**连接：`OneBotAdapter::subscribe()` 会先跑一次
    // `probe_connection`（连上即关，只为在 bot_start 之前把配置错误当场报出来），
    // 之后才是真客户端的持续连接。`wait_ready` 等到「最新一条且全部已稳定」的连接，
    // 也就是真客户端那条；探测连接这时候已经关了。
    let ready_generation = gateway
        .wait_ready(Duration::from_secs(10))
        .await
        .expect("真客户端应当连上网关");
    assert!(
        ready_generation > probe_generation,
        "真客户端应当是手工探针之后的新连接（探针={probe_generation}，真连接={ready_generation}）"
    );

    // ---------- ① 三条群消息：handler 顺序 + 回复顺序 ----------
    gateway
        .push(group_message(
            SELF_ID,
            Some(MSG_ONE),
            GROUP_ID,
            GROUP_USER,
            json!([
                at_segment(AT_QQ),
                text_segment(" 你好"),
                image_segment("cat.png")
            ]),
            AT_MESSAGE_TEXT,
        ))
        .await
        .expect("推第一条群消息");
    gateway
        .push(group_message(
            SELF_ID,
            Some(MSG_TWO),
            GROUP_ID,
            GROUP_USER,
            json!("第二条"),
            "第二条",
        ))
        .await
        .expect("推第二条群消息");
    gateway
        .push(group_message(
            SELF_ID,
            Some(MSG_THREE),
            GROUP_ID,
            GROUP_USER,
            json!([text_segment("第三条")]),
            "第三条",
        ))
        .await
        .expect("推第三条群消息");

    wait_until("三条群消息的回复都发出来了", || {
        gateway.actions_named("send_group_msg").len() == 3
    })
    .await;
    wait_until_async("入站全部被标记为已处理", || {
        let store = Arc::clone(&store);
        async move {
            blocking(move || {
                store
                    .list_unprocessed_inbound(10)
                    .map(|rows| rows.is_empty())
            })
            .await
            .expect("读未处理入站")
        }
    })
    .await;
    wait_until("队列跑空", || supervisor.queue_depth() == 0).await;

    // handler 真的按序看到了三条（同会话 FIFO）。
    let logs = logs_of(&store, bot_id).await;
    let seen: Vec<String> = logs
        .iter()
        .map(|row| row.line.clone())
        .filter(|line| line.contains("收到："))
        .collect();
    assert_eq!(
        seen,
        vec![
            "收到：@222 你好[图片]".to_owned(),
            "收到：第二条".to_owned(),
            "收到：第三条".to_owned(),
        ],
        "handler 看到的顺序/正文不对（同会话 FIFO 或消息段渲染被破坏）"
    );

    // handler 产出的 OutboundRequest 也按序（handler → outbox 这一段）。
    let relayed = relay.requests();
    assert_eq!(relayed.len(), 3, "handler 应当产出三条回复：{relayed:?}");
    assert_eq!(
        relayed
            .iter()
            .map(|request| request.reply_to.clone())
            .collect::<Vec<_>>(),
        vec![
            Some(MSG_ONE.to_string()),
            Some(MSG_TWO.to_string()),
            Some(MSG_THREE.to_string()),
        ],
        "handler 的被动回复没有把平台消息 ID 绑成 reply_to"
    );

    // ---------- ② / ③ 出站 payload：群号、正文渲染、引用段 ----------
    let group_sends = gateway.actions_named("send_group_msg");
    assert_eq!(group_sends.len(), 3, "群消息回复条数不对：{group_sends:?}");
    for (recorded, (message_id, text)) in group_sends.iter().zip([
        (MSG_ONE, AT_MESSAGE_TEXT),
        (MSG_TWO, "第二条"),
        (MSG_THREE, "第三条"),
    ]) {
        assert_eq!(
            recorded.params["group_id"],
            json!(GROUP_ID),
            "send_group_msg 的 group_id 不对：{recorded:?}"
        );
        assert_eq!(
            recorded.params["message"],
            json!([
                { "type": "reply", "data": { "id": message_id.to_string() } },
                { "type": "text", "data": { "text": text } },
            ]),
            "出站 message 的段结构不对（引用段或被动了渲染规则）：{recorded:?}"
        );
    }

    // ---------- 私聊：走 send_private_msg，payload 里不能混进 group_id ----------
    gateway
        .push(private_message(
            SELF_ID,
            Some(MSG_PRIVATE),
            PRIVATE_USER,
            json!([text_segment("悄悄话")]),
            "悄悄话",
        ))
        .await
        .expect("推私聊消息");
    wait_until("私聊回复发出", || {
        gateway.actions_named("send_private_msg").len() == 1
    })
    .await;
    let private_sends = gateway.actions_named("send_private_msg");
    assert_eq!(
        private_sends[0].params["user_id"],
        json!(PRIVATE_USER),
        "send_private_msg 的 user_id 不对：{:?}",
        private_sends[0]
    );
    assert_eq!(
        private_sends[0].params["message"],
        json!([
            { "type": "reply", "data": { "id": MSG_PRIVATE.to_string() } },
            { "type": "text", "data": { "text": "悄悄话" } },
        ]),
        "私聊出站 message 不对：{:?}",
        private_sends[0]
    );
    assert!(
        private_sends[0].params.get("group_id").is_none(),
        "私聊 payload 里混进了 group_id：{:?}",
        private_sends[0]
    );

    // ---------- ④ 同一 message_id 投两次：handler 只执行一次 ----------
    let duplicate = group_message(
        SELF_ID,
        Some(MSG_DUP),
        GROUP_ID,
        GROUP_USER,
        json!("重复投递"),
        "重复投递",
    );
    gateway.push(duplicate.clone()).await.expect("第一次投递");
    gateway.push(duplicate).await.expect("第二次投递");
    // 紧跟一条「哨兵」消息：它排在重复投递之后，它的回复到了就说明前两条都已经
    // 走完了 admit（入队或判重），此时再断言条数不会和「还没到」混淆。
    gateway
        .push(group_message(
            SELF_ID,
            Some(MSG_AFTER_DUP),
            GROUP_ID,
            GROUP_USER,
            json!("去重之后"),
            "去重之后",
        ))
        .await
        .expect("推哨兵消息");
    wait_until("重复投递之后的哨兵回复发出", || {
        gateway.actions_named("send_group_msg").len() == 5
    })
    .await;
    let after_dup = gateway.actions_named("send_group_msg");
    assert_eq!(
        after_dup.len(),
        5,
        "同一 message_id 投两次，出站回复条数不对（判重没生效或多发了）：{after_dup:?}"
    );
    assert_eq!(
        after_dup[3].params["message"],
        json!([
            { "type": "reply", "data": { "id": MSG_DUP.to_string() } },
            { "type": "text", "data": { "text": "重复投递" } },
        ]),
        "重复投递的回复内容不对：{:?}",
        after_dup[3]
    );
    assert_eq!(
        after_dup[4].params["message"],
        json!([
            { "type": "reply", "data": { "id": MSG_AFTER_DUP.to_string() } },
            { "type": "text", "data": { "text": "去重之后" } },
        ]),
        "哨兵消息的回复内容/顺序不对：{:?}",
        after_dup[4]
    );
    let logs = logs_of(&store, bot_id).await;
    let dup_runs = logs
        .iter()
        .filter(|row| row.line.contains("收到：重复投递"))
        .count();
    assert_eq!(
        dup_runs, 1,
        "同一 message_id 投两次，handler 却跑了 {dup_runs} 次（判重被绕过）"
    );

    // ---------- 事件不带 message_id：兜底键是本地合成键，不能当引用 id 发出去 ----------
    gateway
        .push(group_message(
            SELF_ID,
            None,
            GROUP_ID,
            GROUP_USER,
            json!("没有 message_id"),
            "没有 message_id",
        ))
        .await
        .expect("推无 message_id 的消息");
    wait_until("无 message_id 的消息回复发出", || {
        gateway.actions_named("send_group_msg").len() == 6
    })
    .await;
    let with_fallback = gateway.actions_named("send_group_msg");
    // 没有 `message_id` 时 adapter 用兜底幂等键当 `external_id`（`<user_id>-<time>-<16 位
    // hex>`，见 `hbot_adapters::onebot::fallback_external_id`）。那是**本地合成**的去重键，
    // 平台侧不认，所以 `passive_reply_id` 必须返回 `None`：出站 `message` 由
    // `build_outbound(None)` 退化成纯字符串，整条没有 `reply` 段。
    let fallback_segments = outbound_segments(&with_fallback[5].params["message"]);
    assert!(
        !fallback_segments
            .iter()
            .any(|segment| segment["type"] == json!("reply")),
        "无 message_id 的事件不该带 reply 段（把本地合成的去重键当引用 id 会被真网关拒绝）：{fallback_segments:?}"
    );
    assert_eq!(
        fallback_segments[0]["type"],
        json!("text"),
        "无 message_id 的事件，出站首段应当是正文：{fallback_segments:?}"
    );
    assert_eq!(
        fallback_segments[0]["data"]["text"],
        json!("没有 message_id"),
        "无 message_id 的消息正文不对：{fallback_segments:?}"
    );
    assert_eq!(
        fallback_segments.len(),
        1,
        "无 message_id 的事件只该有正文一段：{fallback_segments:?}"
    );

    // ---------- ③ 推完一条就断：adapter 重连，回复跨连接照样发出来 ----------
    let before_disconnect = gateway
        .wait_ready(Duration::from_secs(10))
        .await
        .expect("应当有稳定连接（要主动断开它）");
    gateway
        .push_then_disconnect(group_message(
            SELF_ID,
            Some(MSG_RECONNECT),
            GROUP_ID,
            GROUP_USER,
            json!("断开前"),
            "断开前",
        ))
        .await
        .expect("推完就断");
    // 必须等「比被断掉那条更新」的连接：`Close` 刚入队时旧连接还在注册表里，
    // 写任务可能还没被调度，只等「有稳定连接」会立刻拿回旧的那条。
    let reconnected = gateway
        .wait_ready_after(before_disconnect, Duration::from_secs(10))
        .await
        .expect("adapter 应当重连上网关");
    assert!(
        reconnected > before_disconnect,
        "重连应当是新的连接（断开前={before_disconnect}，重连后={reconnected}）"
    );
    wait_until("断开前那条的回复在重连后发出", || {
        gateway.actions_named("send_group_msg").len() == 7
    })
    .await;
    gateway
        .push(group_message(
            SELF_ID,
            Some(MSG_AFTER_RECONNECT),
            GROUP_ID,
            GROUP_USER,
            json!("重连之后"),
            "重连之后",
        ))
        .await
        .expect("推重连后的消息");
    wait_until("重连后的消息也有回复", || {
        gateway.actions_named("send_group_msg").len() == 8
    })
    .await;

    let all_group_sends = gateway.actions_named("send_group_msg");
    assert_eq!(
        all_group_sends[6].params["message"],
        json!([
            { "type": "reply", "data": { "id": MSG_RECONNECT.to_string() } },
            { "type": "text", "data": { "text": "断开前" } },
        ]),
        "断开前那条消息的回复不对（跨重连的重试把它弄丢或弄错了）：{:?}",
        all_group_sends[6]
    );
    assert_eq!(
        all_group_sends[7].params["message"],
        json!([
            { "type": "reply", "data": { "id": MSG_AFTER_RECONNECT.to_string() } },
            { "type": "text", "data": { "text": "重连之后" } },
        ]),
        "重连后的回复不对：{:?}",
        all_group_sends[7]
    );
    // 每条消息只该有一条回复（重连不该把 outbox 里的行再发一遍）。
    let mut reply_ids: Vec<String> = all_group_sends
        .iter()
        .filter_map(|recorded| recorded.params["message"][0]["data"]["id"].as_str())
        .map(str::to_owned)
        .collect();
    let reply_ids_before = reply_ids.len();
    reply_ids.sort_unstable();
    reply_ids.dedup();
    assert_eq!(
        reply_ids.len(),
        reply_ids_before,
        "同一条入站消息出现了两条出站回复（重连重发）：{all_group_sends:?}"
    );

    // ---------- QQ 官方的非数字 external_id（`ROBOT1.0_…`）仍然要带引用段 ----------
    // 「是不是合成键」的判据必须是**形状识别**，不能退化成「必须纯数字」：OneBot 的
    // 真 message_id 是 i64（纯数字），但 QQ 官方的 `d.id` 是 `ROBOT1.0_…` 这种非数字
    // 串，按纯数字判会把 QQ 的被动回复全打死。这里注入一条 external_id 是 QQ 形态的
    // 入站事件（payload 不带 `injected` 标记，所以走的是真被动回复路径）。
    let qq_external_id = "ROBOT1.0_qq-form-external-id";
    supervisor
        .inject(group_event(
            GROUP_ID.to_string().as_str(),
            qq_external_id,
            "QQ 形态",
        ))
        .await
        .expect("注入 QQ 形态的入站事件");
    wait_until("QQ 形态事件的回复发出", || {
        gateway.actions_named("send_group_msg").len() == 9
    })
    .await;
    let with_qq = gateway.actions_named("send_group_msg");
    let qq_segments = outbound_segments(&with_qq[8].params["message"]);
    assert_eq!(
        qq_segments[0],
        json!({ "type": "reply", "data": { "id": qq_external_id } }),
        "QQ 官方的 external_id（`d.id`，非纯数字）必须仍然当引用 id：判据不能退化成“必须纯数字”"
    );
    assert_eq!(
        qq_segments[1]["data"]["text"],
        json!("QQ 形态"),
        "QQ 形态事件的正文不对：{qq_segments:?}"
    );

    // 收尾：真 supervisor 停干净，且没有遗留 pending。
    match supervisor.stop().await {
        Ok(()) => {}
        Err(HbotError::Timeout { ref operation, .. }) if operation == "flush outbox" => {}
        Err(other) => panic!("supervisor 停止出现意外错误：{other}"),
    }
    let pending = blocking({
        let store = Arc::clone(&store);
        move || store.list_pending_outbound_for_bot(&bot_id, 10)
    })
    .await
    .expect("读 pending outbound");
    assert!(
        pending.is_empty(),
        "成功发完之后不该还有 pending 行：{pending:?}"
    );
    drop(relay);
    drop(supervisor);
    drop(adapter);
    drop(store);
    drop(dir);
    drop(gateway);
}

#[tokio::test]
async fn gateway_itself_speaks_onebot_11() {
    if tokio::time::timeout(DEADLINE, gateway_case())
        .await
        .is_err()
    {
        panic!("parity_14 网关自检：{DEADLINE:?} 内没有跑完（有东西挂住了）");
    }
}

/// 网关自己的协议自检（不经过 adapter）：握手鉴权、lifecycle/heartbeat、action 响应。
///
/// 这一条是为了让 `tests/support/gateway.rs` 的实现也被测住 —— 否则「端到端过了」
/// 只说明两边凑得上，不说明网关真的按 OneBot 11 说话。
async fn gateway_case() {
    let gateway = Gateway::start(GatewayConfig::new(SELF_ID).with_token(TOKEN)).await;

    // ① 错的 token：握手被拒，且是 HTTP 401（不是连上再断）。
    let mut wrong = gateway
        .ws_url()
        .into_client_request()
        .expect("构造 WS 请求");
    wrong.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer wrong-token"),
    );
    match tokio_tungstenite::connect_async(wrong)
        .await
        .expect_err("错的 token 不该握手成功")
    {
        WsError::Http(response) => assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "拒绝握手必须是 401"
        ),
        other => panic!("错的 token 期望被 401 拒绝，实际是 {other:?}"),
    }
    assert_eq!(
        gateway.rejected_authorizations(),
        vec![Some("Bearer wrong-token".to_owned())]
    );

    // ② 对的 token：连上第一帧就是 lifecycle(connect)。
    let mut request = gateway
        .ws_url()
        .into_client_request()
        .expect("构造 WS 请求");
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).expect("Bearer 头是 ASCII"),
    );
    let (ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("对的 token 应当握手成功");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let (mut sink, mut source) = ws.split();

    let lifecycle = next_meta(&mut source, "lifecycle").await;
    assert_eq!(lifecycle["post_type"], "meta_event");
    assert_eq!(lifecycle["sub_type"], "connect");
    assert_eq!(lifecycle["self_id"], json!(SELF_ID));

    // ③ 心跳：节拍内来一帧，间隔字段与配置一致。
    let heartbeat = next_meta(&mut source, "heartbeat").await;
    assert_eq!(heartbeat["status"]["online"], json!(true));
    assert_eq!(heartbeat["interval"], json!(1_000));

    // ④ `get_status` 回 status=ok / retcode=0，echo 原样回显。
    sink.send(Message::Text(
        json!({ "action": "get_status", "params": {}, "echo": "status-1" }).to_string(),
    ))
    .await
    .expect("发 get_status");
    let status = next_response(&mut source).await;
    assert_eq!(status["status"], "ok");
    assert_eq!(status["retcode"], json!(0));
    assert_eq!(status["data"]["online"], json!(true));
    assert_eq!(status["echo"], "status-1");

    // ⑤ `send_group_msg` 回 message_id，params 原样记下来。
    sink.send(Message::Text(
        json!({
            "action": "send_group_msg",
            "params": { "group_id": GROUP_ID, "message": "自检" },
            "echo": "send-1",
        })
        .to_string(),
    ))
    .await
    .expect("发 send_group_msg");
    let send = next_response(&mut source).await;
    assert_eq!(send["status"], "ok");
    assert_eq!(send["retcode"], json!(0));
    assert!(
        send["data"]["message_id"].is_i64(),
        "message_id 应当是整数：{send}"
    );

    // ⑥ 未知 action：failed + retcode 1404，echo 照样原样回。
    sink.send(Message::Text(
        json!({ "action": "set_group_ban", "params": {}, "echo": 42 }).to_string(),
    ))
    .await
    .expect("发未知 action");
    let unsupported = next_response(&mut source).await;
    assert_eq!(unsupported["status"], "failed");
    assert_eq!(unsupported["retcode"], json!(RETCODE_UNSUPPORTED_ACTION));
    assert_eq!(unsupported["echo"], json!(42));

    // ⑦ 出站 action 都被记下来了（顺序、params、echo 原样）。
    let actions = gateway.actions();
    assert_eq!(actions.len(), 3, "应当记下三条 action：{actions:?}");
    assert_eq!(actions[0].action, "get_status");
    assert_eq!(actions[1].action, "send_group_msg");
    assert_eq!(
        actions[1].params,
        json!({ "group_id": GROUP_ID, "message": "自检" })
    );
    assert_eq!(actions[2].action, "set_group_ban");
    assert_eq!(actions[2].echo, json!(42));
    assert!(
        actions.iter().all(|recorded| recorded.connection == 1),
        "三条 action 应当都来自同一条连接：{actions:?}"
    );

    // ⑧ 没有连接时推动事件是明确的错误（不是静默丢掉）。
    drop(sink);
    drop(source);
    wait_until("网关把关闭的连接清掉", || !gateway.live()).await;
    let error = gateway
        .push(json!({ "post_type": "meta_event" }))
        .await
        .expect_err("没有连接时不该推得出去");
    assert!(
        error.contains("没有活着的连接"),
        "没有连接时的错误信息不对：{error}"
    );
}

/// 读下一帧 JSON 文本（跳过 ping/pong/binary）。
async fn next_json<S>(source: &mut S) -> Option<Value>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    while let Some(frame) = source.next().await {
        if let Ok(Message::Text(text)) = frame {
            return serde_json::from_str(&text).ok();
        }
    }
    None
}

/// 读到一条指定 `meta_event_type` 的帧（中间可能夹着心跳）。
async fn next_meta<S>(source: &mut S, meta_event_type: &str) -> Value
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    loop {
        let frame = next_json(source)
            .await
            .expect("网关在推 meta 事件之前就断开了");
        if frame.get("meta_event_type").and_then(Value::as_str) == Some(meta_event_type) {
            return frame;
        }
    }
}

/// 读到一条 action 响应（带 `echo` 的帧；中间可能夹着心跳）。
async fn next_response<S>(source: &mut S) -> Value
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    loop {
        let frame = next_json(source)
            .await
            .expect("网关在回 action 之前就断开了");
        if frame.get("echo").is_some() {
            return frame;
        }
    }
}

/// 把出站 `message` 归一成「段」列表。
///
/// adapter 在**没有引用**时发的是纯字符串（`build_outbound(None)` →
/// `MessagePayload::Text`），有引用时才是段数组；断言不该关心这个编码差异。
fn outbound_segments(message: &Value) -> Vec<Value> {
    match message {
        Value::String(text) => vec![json!({ "type": "text", "data": { "text": text } })],
        Value::Array(segments) => segments.clone(),
        other => panic!("出站 message 既不是字符串也不是段数组：{other}"),
    }
}
