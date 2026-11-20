//! `bot_start` / `bot_stop` / `bot_inject` / `bot_list` / `logs_tail` 五个命令的实现。
//!
//! ## 组合根就在这里
//!
//! 这几个函数是 daemon 里唯一把"平台适配器 + supervisor + 沙箱执行器 + outbox"接起来的
//! 地方（`crates/hbot-bot` 与 `crates/hbot-adapters` 都不知道彼此的存在）。接线形状见
//! [`crate::app::runtime`] 的模块文档：
//!
//! ```text
//! OneBotAdapter ──events──► Supervisor ──► SessionQueues ──► SandboxHandler(QuickJS)
//!        ▲                      │                                  │
//!        └────── send ────── Outbox ◄── mpsc<OutboundRequest> ◄──── replies
//! ```
//!
//! ## 配置从哪来（M0）
//!
//! 连接参数（ws 地址 / token / selfId / sandbox 限额）来自**项目根的 `bots.json`**，
//! schema 与错误信息见 [`crate::app::runtime::config`]。库里没有 bot 行时会按配置补建
//! （`projects` 行同样按项目根补建）——否则 `bots.project_id` 的外键根本没法满足，
//! M0 也就没有"真机跑通"这条路径。等 IDE 有了建 bot 的界面，这段引导就可以删掉。
//!
//! ## 状态怎么反映出去
//!
//! * 启动成功：库里 `bots.status = running`，并推 [`Event::BotStatus`]。
//! * 启动失败：库里 `bots.status = error`，推 `BotStatus::Stopped` + `error` 文本
//!   （wire DTO 的 `BotStatus` 只有 5 个状态、没有 `Error`，见
//!   `crates/hbot-rpc/src/dto/event.rs`；`error` 字段就是给这个场景留的）。
//! * 停止：幂等，总是返回 ok，库里 `stopped`，推 `BotStatus::Stopped`。

use std::path::Path;
use std::sync::Arc;

use hbot_bot::{
    BotSpec, ConversationRef, Inbox, InjectOutcome, PlatformEvent, Supervisor, SupervisorDeps,
};
use hbot_core::{
    BotId, BotStatus, ConversationId, ConversationKind, EventSink, HbotError, Timestamp,
};
use hbot_store::{
    BOT_STATUS_DEGRADED, BOT_STATUS_ERROR, BOT_STATUS_RUNNING, BOT_STATUS_STOPPED, Bot, Project,
};
use serde_json::json;
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::app::bots::running_bot;
use crate::app::commands::{done, failed, unimplemented};
use crate::app::runtime::config::{BotConfig, read_bot_config};
use crate::app::runtime::files::ProjectFiles;
use crate::app::runtime::jobs::StoreJobQueue;
use crate::app::runtime::local;
use crate::app::runtime::onebot::PLATFORM;
use crate::app::runtime::outbox_source::StorePendingOutbox;
use crate::app::runtime::sandbox_handler::{
    REPLY_CAPACITY, SandboxHandler, SandboxHandlerConfig, TICK_INTERVAL,
};
use crate::app::runtime::send::StoreSendQueue;
use crate::app::runtime::sql::StoreSqlBackend;
use crate::app::runtime::waits::StoreWaitQueue;
use crate::app::runtime::{AdapterPorts, conversation_kind_from_str, conversation_kind_str};
use crate::app::store_failed;
use crate::dto::{
    BotSummary, Command, ConversationSummary, Event, JobView, LogLevel, Response, RunLog, WaitView,
};

/// `bot_inject` 伪造的发送者 id。
///
/// 用固定值而不是随机值：注入消息在库里要一眼能认出来（`inbound.sender_id`），
/// 与真实平台的数字 QQ 号不会撞。
pub const INJECT_SENDER: &str = "inject";

/// `logs_tail` 单次最多返回多少行（防止 UI 传个巨大的 limit 把内存打满）。
pub const MAX_LOG_LIMIT: usize = 1000;

/// `bot_jobs` / `bot_waits` 单次最多回多少条（面板只需要最近若干条，不接一万行）。
///
/// 超了按上限算而不是报错：面板轮询时传什么值都不该让整块 UI 变红。
pub const MAX_VIEW_LIMIT: usize = 200;

/// 按命令分发到各 handler（`dispatch.rs` 是本模块唯一调用方）。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::BotStart { bot_id } => start(state, bot_id).await,
        Command::BotStop { bot_id } => stop(state, bot_id).await,
        Command::BotInject {
            bot_id,
            conversation_id,
            text,
        } => inject(state, bot_id, conversation_id, text).await,
        Command::LogsTail { bot_id, limit } => logs_tail(state, bot_id, limit).await,
        Command::BotList => bot_list(state).await,
        Command::BotConversations { bot_id } => bot_conversations(state, bot_id).await,
        Command::BotJobs { bot_id, limit } => bot_jobs(state, bot_id, limit).await,
        Command::BotWaits { bot_id, limit } => bot_waits(state, bot_id, limit).await,
        other => unimplemented(other.kind()),
    }
}

/// 启动一个 bot：读配置 → 建适配器/执行器 → 起 supervisor → 登记到 registry。
///
/// 重复启动返回 -32003（conflict）。启动失败时释放占位、把失败落到库里并推事件，
/// 然后把真错误返回给调用方（绝不"看起来起来了"）。
pub async fn start(state: &AppState, bot_id: BotId) -> Response {
    // 占位先于一切异步动作：两个并发 bot_start 里只有一个能走到下面，
    // 否则两条路径都会订阅同一个适配器（第二条会撞 "只能订阅一次"）并留下半个 runtime。
    if !state.bots().reserve(bot_id) {
        return failed(HbotError::conflict(format!(
            "bot {bot_id} 已在运行或正在启动：拒绝重复启动（-32003）"
        )));
    }

    match start_inner(state, bot_id).await {
        Ok(()) => done(),
        Err(error) => {
            state.bots().release(&bot_id);
            report_start_failure(state, bot_id, &error.to_string()).await;
            failed(error)
        }
    }
}

/// [`start`] 的实际接线（失败时由调用方统一收尾）。
async fn start_inner(state: &AppState, bot_id: BotId) -> Result<(), HbotError> {
    let root = state.project_root().ok_or_else(|| {
        HbotError::invalid(
            "没有打开的项目：先发 project_open，再 bot_start（bots.json 位于项目根）",
        )
    })?;

    // 配置先读：缺配置要报 -32002，而不是"先建了一堆行再告诉你没地址"。
    let config_root = root.clone();
    let config = state
        .blocking(move || read_bot_config(&config_root, &bot_id))
        .await?;

    let bot = match load_bot(state, bot_id).await? {
        Some(bot) => {
            if !bot.enabled {
                return Err(HbotError::denied(format!(
                    "bot {bot_id} 在库里是禁用状态（enabled=0）：先在设置里启用它"
                )));
            }
            bot
        }
        None => bootstrap_bot(state, &root, &config, bot_id).await?,
    };

    // 平台适配器：OneBot 走 `connect`（内含 `wss://` 的显式拒绝）。
    // 走 `create_with` 而不是 `create`：`local` 平台要把回复写进 run log，需要
    // store / events / clock 三个端口；不关心端口的平台用 trait 的默认实现原样转发。
    let ports = AdapterPorts {
        store: Arc::clone(state.store()),
        events: Arc::clone(state.events()),
        clock: Arc::clone(state.clock()),
    };
    let adapter = state.adapter_factory().create_with(&bot, &config, &ports)?;

    // 沙箱执行器：回复先落在有界 channel 上，再由下面的搬运任务转进 outbox。
    let (replies_tx, mut replies_rx) = mpsc::channel(REPLY_CAPACITY);
    let executor = Arc::new(
        SandboxHandler::new(SandboxHandlerConfig {
            bot_id,
            project_root: root.clone(),
            limits: config.limits,
            store: Arc::clone(state.store()),
            events: Arc::clone(state.events()),
            clock: Arc::clone(state.clock()),
            // 出站端口要复用同一条通道（`ctx.send` 的主动消息与 handler 返回值排在同一个队列里），
            // 所以这里留一份给下面的 `with_send`。
            replies: replies_tx.clone(),
            // 状态能力：声明闸在 `StoreBotState` 里，工厂每次起 isolate 现读清单。
            state: Some(Arc::new(crate::app::state::StoreStateSource::new(
                Arc::clone(state.store()),
                Arc::clone(state.clock()),
            ))),
            // 智能能力：声明闸在 `ProviderAiClient` 里（每次调用重读），工厂每次起
            // isolate 现读清单与 `agent.json`；没配模型时沙箱侧报 `HBOT_AI_UNAVAILABLE`。
            ai: Some(Arc::new(crate::app::ai::HostAiSource::new(
                state.data_dir(),
            ))),
            // 作业能力：入队端口落 `bot_jobs` 表（声明闸在沙箱侧现读清单，见
            // `SandboxHandler::worker`）。不装配 = `ctx.jobs.enqueue` 报
            // `HBOT_JOBS_UNAVAILABLE`，明确失败，不是把作业收进内存。
            // SQL 能力：每 bot 一个库文件（隔离靠文件，不靠表名前缀）。
            // 迁移在 `start_bot` 的后面执行一次（见 `apply_sql_migrations`）。
            sql: Some(Arc::new(StoreSqlBackend::new(
                bot_id,
                root.clone(),
                Arc::clone(state.bot_sql()),
            ))),
            // 文件能力：路径钉在项目根里（符号链接也要落在根内），读 / 写分开声明。
            // 不装配 = `ctx.files.*` 报 `HBOT_FILE_UNAVAILABLE`，明确失败。
            files: Some(Arc::new(ProjectFiles::new(root.clone()).map_err(
                |error| HbotError::internal(format!("项目根不可用，无法装配文件能力：{error}")),
            )?)),
            jobs: Some(Arc::new(StoreJobQueue::new(
                bot_id,
                root.clone(),
                Arc::clone(state.store()),
                Arc::clone(state.ids()),
                Arc::clone(state.clock()),
            ))),
            // 会话等待：`ctx.session.prompt` 落 `bot_waits`；声明闸在沙箱侧现读清单。
            waits: Some(Arc::new(StoreWaitQueue::new(
                bot_id,
                Arc::clone(state.store()),
                Arc::clone(state.ids()),
                Arc::clone(state.clock()),
            ))),
        })
        // 设备能力：闸是 `AppState` 那一份（唯一判据），后端是当前平台的那一个。
        // 生产装配只有这一处 —— 别的路径（测试/bench）不装，`ctx.device.*` 会明确报
        // `HBOT_DEVICE_UNAVAILABLE`，不会静默成功。
        .with_device(
            state.device().gate(),
            Arc::new(hbot_device::platform_backend()),
        )
        // 出站能力：`ctx.send` / `ctx.notify` 走 supervisor 的回复通道（`reply_to: None` = 主动消息）。
        // 生产装配只有这一处 —— 不装配时 `ctx.send` 明确报 `HBOT_SEND_UNAVAILABLE`，不是静默丢消息。
        .with_send(Arc::new(StoreSendQueue::new(
            bot_id,
            Arc::clone(state.store()),
            replies_tx,
            Arc::clone(state.clock()),
        )) as Arc<dyn hbot_code::send::SendQueue>),
    );

    // 执行器要给两处：supervisor（跑 handler）和下面的定时器 tick 循环。
    // 这里显式把具体类型转成 trait object —— 直接写 `Arc::clone(&executor)` 会得到
    // `Arc<SandboxHandler>`，而 `SupervisorDeps::new` 要的是 `Arc<dyn HandlerExecutor>`。
    let handler: Arc<dyn hbot_bot::HandlerExecutor> = executor.clone();

    let mut deps = SupervisorDeps::new(
        Arc::clone(state.store()),
        Arc::clone(state.ids()),
        Arc::clone(state.clock()),
        adapter,
        handler,
    );
    // 续发来源：上次崩溃/关停留下的 pending outbound（按 bot 过滤，见 outbox_source）。
    deps.pending_outbox = Arc::new(StorePendingOutbox::new(Arc::clone(state.store())));

    let spec = BotSpec {
        bot_id,
        project_id: bot.project_id,
        name: bot.name.clone(),
        platform: bot.platform.clone(),
        // token 绝不进库：config_json 只留非敏感字段（库文件会被备份/上传）。
        config_json: json!({ "wsUrl": config.ws_url }).to_string(),
    };

    let supervisor = Arc::new(Supervisor::new(spec, deps));
    supervisor.start().await?;

    // `onStart`：**失败就是启动失败**。放在 supervisor.start 之后、登记之前 ——
    // 此刻还没有任何消息被消费（队列是空的），所以「初始化没成功就别让它开始接活」
    // 这句话是成立的；失败时的收尾与 ensure_local_conversation 那条路一致（先停 supervisor）。
    // 项目自带的 `migrations/*.sql` 在 `onStart` **之前**跑：`onStart` 里可能就要读表。
    // 失败**不阻断启动**（与编译失败同一纪律）：迁移是用户代码的一部分，一个写错的
    // `CREATE TABLE` 不该让 bot 永远起不来 —— 落一行 error，让它起着、改好再重启。
    match executor.apply_sql_migrations() {
        Ok(0) => {}
        Ok(applied) => {
            tracing::info!(bot = %bot_id, applied, "SQL 迁移已应用");
        }
        Err(error) => {
            tracing::warn!(bot = %bot_id, error = %error, "SQL 迁移失败（不阻断启动）");
            local::record_lines(
                state.store(),
                state.events(),
                state.clock(),
                bot_id,
                LogLevel::Error,
                vec![format!("SQL 迁移失败：{error}")],
            )
            .await;
        }
    }
    if let Err(error) = executor
        .run_lifecycle(hbot_core::LifecyclePhase::Start, None)
        .await
    {
        if let Err(stop_error) = supervisor.stop().await {
            tracing::warn!(bot = %bot_id, error = %stop_error, "onStart 失败后停止 supervisor 也失败");
        }
        return Err(error);
    }

    // `local` 平台的会话行：它不订阅任何真实事件，所以没有人会替它建 conversation 行，
    // 而 `bot_inject` 要求这一行存在（`inject_inner` 会 `get_conversation`，拿不到 -32001）。
    // 失败要收尾：supervisor 已经在跑，直接返回 Err 会留下一个没人登记的 runtime。
    if let Err(error) = ensure_local_conversation(state, &bot).await {
        if let Err(stop_error) = supervisor.stop().await {
            tracing::warn!(bot = %bot_id, error = %stop_error, "启动收尾时停止 supervisor 失败");
        }
        return Err(error);
    }

    // 回复搬运：handler 产出 → supervisor.send() → outbox 行 → 适配器发送。
    // supervisor 自己不会调 handler，这条单行道保证"回复必须经过 outbox"。
    let replies_supervisor = Arc::clone(&supervisor);
    tokio::spawn(async move {
        while let Some(request) = replies_rx.recv().await {
            if let Err(error) = replies_supervisor.send(request).await {
                // 到这个点回复已经产出，但没进 outbox：留痕，不静默吞。
                tracing::warn!(
                    bot = %replies_supervisor.bot_id(),
                    error = %error,
                    "回复未能进入 outbox：这条回复丢了"
                );
            }
        }
    });

    if let Err(error) = persist_status(state, bot_id, BOT_STATUS_RUNNING).await {
        // 状态写不进去就别假装起来了：先停掉 supervisor，再报错。
        if let Err(stop_error) = supervisor.stop().await {
            tracing::warn!(bot = %bot_id, error = %stop_error, "启动收尾时停止 supervisor 失败");
        }
        return Err(error);
    }

    // webhook 的派发入口：登记的**执行器**（不是 supervisor）—— 外部请求没有会话，
    // 不走消息那条「落库 → 入队 → 去重」的管道。
    state.webhooks().register(bot_id, Arc::clone(&executor));

    let started_at = state.clock().now();
    state.bots().insert(
        bot_id,
        running_bot(bot.platform.clone(), started_at, supervisor),
    );

    // 定时器 tick：每 `TICK_INTERVAL` 问执行器一次"有没有计划到点"。
    //
    // 只有在清单里声明了计划时才会真编译、真起 isolate —— 没声明时 `tick` 第一句就返回，
    // 所以这个循环对不声明 timer 的项目只是一秒一次的空转（一次读清单 + 一个长度判断）。
    //
    // 循环必须能被 `bot_stop` 停住：句柄登记进 `BotRegistry` 的 `aux`，`stop()` 会 abort。
    // 登记失败说明这个 bot 已经不在表里（停止与登记赛跑），此时立刻 abort 自己。
    // 定时器与作业共用同一拍：两者都是"声明了才有"的能力，各自第一句都会先读清单
    // 决定要不要干活。分成两个函数而不是一个，是因为 `tick` 的早退条件是"没有计划"——
    // 复用给作业会让只声明 jobs 的项目永远派发不到。
    let ticker_handler = Arc::clone(&executor);
    let ticker_clock = Arc::clone(state.clock());
    let ticker = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        // 丢弃错过的节拍：休眠后醒来不该补发一串 tick（补发的次数由计划表自己决定）。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now = ticker_clock.now();
            if let Err(error) = ticker_handler.tick(now).await {
                // 一秒一次的错误不刷屏：记一行 warn，下一拍再试。
                tracing::warn!(bot = %bot_id, error = %error, "定时器 tick 失败");
            }
            if let Err(error) = ticker_handler.tick_jobs(now).await {
                tracing::warn!(bot = %bot_id, error = %error, "作业 tick 失败");
            }
            if let Err(error) = ticker_handler.tick_waits(now).await {
                tracing::warn!(bot = %bot_id, error = %error, "等待 tick 失败");
            }
        }
    });
    if !state.bots().attach_aux(&bot_id, ticker) {
        tracing::warn!(bot = %bot_id, "bot 已不在登记表里，定时器循环立即停止");
    }

    state.events().emit(Event::BotStatus {
        bot_id,
        status: BotStatus::Running,
        error: None,
    });
    tracing::info!(
        bot = %bot_id,
        platform = %bot.platform,
        project = %bot.project_id,
        "bot 已启动"
    );
    Ok(())
}

/// `local` 平台在启动时幂等建好试跑用的会话行（其它平台什么都不做）。
///
/// ## 为什么必须在这里建
///
/// `local` 适配器不产生任何入站事件（那是它的定义），而真实平台的 conversation 行是由
/// supervisor 的 [`Inbox`] 在处理第一条事件时补建的 —— `local` 没有"第一条事件"，
/// 于是也就没人建。`bot_inject` 要求这一行存在（`inject_inner` 先 `get_conversation`），
/// 所以不给它一行，本地试跑根本用不了（注入直接 -32001）。
///
/// ## 为什么用 `local:default` 这个固定值
///
/// 会话 ID 由 `(bot_id, external_id)` 确定性派生（[`hbot_bot::conversation_id_for`]），
/// 外部 ID 固定 → 本地 ID 也固定：重复 `bot_start`、进程重启之后仍然是同一个会话，
/// 不会每启动一次就在 `conversations` 表里多一行孤儿。
///
/// 这里复用 [`Inbox::ensure_conversation`] 而不是自己"先查再插"：它已经把
/// "唯一约束撞了 = 已经存在"这条判据写好了（见 `crates/hbot-bot/src/dedupe.rs`），
/// 两条路径各写一份就会有一份漏掉冲突处理。
///
/// 返回的会话 ID 会落一行 run log —— 不是装饰：wire DTO 目前**没有**把会话 id 交给 UI
/// 的通道（`BotStart` 返回 `null`、`BotSummary` 没有这个字段），日志是眼下唯一能让
/// 人/脚本拿到它的地方（`logs_tail` 可读）。
async fn ensure_local_conversation(state: &AppState, bot: &Bot) -> Result<(), HbotError> {
    if bot.platform != local::PLATFORM {
        return Ok(());
    }
    let conversation = ConversationRef::new(
        local::DEFAULT_CONVERSATION_EXTERNAL_ID.to_owned(),
        ConversationKind::Private,
    );
    let inbox = Inbox::new(
        Arc::clone(state.store()),
        Arc::clone(state.ids()),
        Arc::clone(state.clock()),
    );
    let conversation_id = inbox.ensure_conversation(&bot.id, &conversation).await?;
    // 落一行可读日志：UI 侧要靠它拿到试跑要用的 conversationId（见上面的说明）。
    local::record_lines(
        state.store(),
        state.events(),
        state.clock(),
        bot.id,
        LogLevel::Info,
        vec![format!(
            "local 试跑会话已就绪：conversationId = {conversation_id}，externalId = \
             {}（bot_inject 用这个 conversationId）",
            local::DEFAULT_CONVERSATION_EXTERNAL_ID
        )],
    )
    .await;
    tracing::info!(
        bot = %bot.id,
        conversation = %conversation_id,
        external_id = local::DEFAULT_CONVERSATION_EXTERNAL_ID,
        "local 试跑会话已就绪"
    );
    Ok(())
}

/// 停止一个 bot（幂等：没在跑也返回 ok）。
///
/// 用户点停止（`bot_stop`）走这里，`onStop` 收到的原因就是 `user`。
pub async fn stop(state: &AppState, bot_id: BotId) -> Response {
    stop_with_reason(state, bot_id, hbot_core::StopReason::User).await
}

/// 停止一个 bot，并把 `reason` 交给 `onStop`。
///
/// 三个调用点各有自己的原因：用户点停止是 `user`，切项目与进程退出是 `shutdown`。
/// 原因不是装饰 —— handler 要靠它区分「存档并退出」与「用户只是重启一下」，
/// 猜错会让下游逻辑读到不可信状态（`StopReason::parse` 不认识的字符串直接报错，
/// 不默认成 `User` 也是这个理由）。
pub async fn stop_with_reason(
    state: &AppState,
    bot_id: BotId,
    reason: hbot_core::StopReason,
) -> Response {
    // `onStop`：在拆掉执行器之前跑，给它 5 秒（`hbot_core::STOP_BUDGET`）。
    // 失败只落日志 —— 停机路径上没有任何人在等它的结果。
    if let Some(executor) = state.webhooks().get(&bot_id) {
        if let Err(error) = executor
            .run_lifecycle(hbot_core::LifecyclePhase::Stop, Some(reason))
            .await
        {
            tracing::warn!(bot = %bot_id, error = %error, "onStop 执行失败");
        }
    }
    // 先注销 webhook：停下来的 bot 不该再被外部系统打到（下一步的请求立刻 404），
    // 而不是等 supervisor 排空完了才发现执行器已经不可用。
    state.webhooks().unregister(&bot_id);
    let was_running = state.bots().stop(&bot_id).await;
    if !was_running {
        tracing::debug!(bot = %bot_id, "bot_stop 作用于未运行的 bot（幂等）");
    }

    // 未完成的作业跟着停：`cancelled` 是终态，重启的 `resume_interrupted_jobs` 不会
    // 再把它们捡回来跑（那是给"进程崩在运行中间"用的，不是给"用户按了停止"用的）。
    // 已经 `done` / `failed` 的行不动 —— 把成功记录改写成"取消"会抹掉真实结果。
    let store = Arc::clone(state.store());
    let at = state.clock().now().unix_millis();
    match tokio::task::spawn_blocking(move || store.cancel_jobs_for_bot(&bot_id, at)).await {
        Err(error) => {
            tracing::error!(bot = %bot_id, error = %error, "取消作业任务失败");
        }
        Ok(Err(error)) => {
            tracing::error!(bot = %bot_id, error = %store_failed(error), "取消作业失败");
        }
        Ok(Ok(0)) => {}
        Ok(Ok(cancelled)) => {
            tracing::info!(bot = %bot_id, cancelled, "bot_stop 取消了未完成的作业");
        }
    }

    // 未结等待跟着停：`cancelled` 是终态，重启也不会再把它们捡回来。
    // 这里**不**发通知消息 —— 那需要一条 outgoing，而 bot 已经停了。
    let store = Arc::clone(state.store());
    let at = state.clock().now().unix_millis();
    match tokio::task::spawn_blocking(move || store.cancel_waits_for_bot(&bot_id, at)).await {
        Err(error) => {
            tracing::error!(bot = %bot_id, error = %error, "取消等待任务失败");
        }
        Ok(Err(error)) => {
            tracing::error!(bot = %bot_id, error = %store_failed(error), "取消等待失败");
        }
        Ok(Ok(0)) => {}
        Ok(Ok(cancelled)) => {
            tracing::info!(bot = %bot_id, cancelled, "bot_stop 取消了未结等待");
        }
    }

    let persisted = persist_status(state, bot_id, BOT_STATUS_STOPPED).await;
    state.events().emit(Event::BotStatus {
        bot_id,
        status: BotStatus::Stopped,
        error: None,
    });
    match persisted {
        Ok(()) => done(),
        Err(error) => failed(error),
    }
}

/// 停掉所有在跑的 bot，并把 `reason` 交给每个 `onStop`。
///
/// 切项目与进程退出都走这里：它们和「用户点停止」的唯一差别就是原因。
/// 没在跑的 bot 不会被叫到（`running()` 只列登记表里的），所以幂等。
pub async fn stop_all_with_reason(state: &AppState, reason: hbot_core::StopReason) {
    for bot_id in state.bots().running() {
        let response = stop_with_reason(state, bot_id, reason).await;
        if let Response::Error { error } = &response {
            tracing::warn!(bot = %bot_id, %reason, error = ?error, "退出时停止 bot 失败");
        }
    }
}

/// 注入一条入站消息。
///
/// 走的是**适配器同一条路**：`Supervisor::inject` → 幂等落库 `inbound` → 会话队列 →
/// 沙箱执行器。绝不直接调 handler —— 否则注入的消息绕过了去重、落库与串行队列，
/// 测试会绿而真机仍然坏。
pub async fn inject(
    state: &AppState,
    bot_id: BotId,
    conversation_id: ConversationId,
    text: String,
) -> Response {
    match inject_inner(state, bot_id, conversation_id, text).await {
        Ok(outcome) => {
            tracing::info!(bot = %bot_id, outcome = %outcome, "bot_inject");
            done()
        }
        Err(error) => failed(error),
    }
}

async fn inject_inner(
    state: &AppState,
    bot_id: BotId,
    conversation_id: ConversationId,
    text: String,
) -> Result<String, HbotError> {
    if text.trim().is_empty() {
        return Err(HbotError::invalid("bot_inject: text 不能为空"));
    }

    let supervisor = state.bots().supervisor_of(&bot_id).ok_or_else(|| {
        HbotError::conflict(format!(
            "bot {bot_id} 未运行：先 bot_start 再 bot_inject（注入和真实流量走同一条队列）"
        ))
    })?;

    let conversation = {
        let store = Arc::clone(state.store());
        state
            .blocking(move || {
                store
                    .get_conversation(&conversation_id)
                    .map_err(store_failed)
            })
            .await?
    }
    .ok_or_else(|| HbotError::not_found("conversation", conversation_id.to_string()))?;

    // external_id 必须每次不同：`inbound` 表的唯一键是 (bot_id, external_id)，
    // 复用同一个 id 会让第二次注入被当成重复投递直接丢掉。
    let external_id = format!("{INJECT_SENDER}-{}", state.ids().message_id());
    let kind = conversation_kind_from_str(&conversation.kind);
    let payload = json!({
        "text": text,
        "senderId": INJECT_SENDER,
        "conversationExternalId": conversation.external_id,
        "kind": conversation_kind_str(kind),
        "injected": true,
    });
    let event = PlatformEvent::new(
        external_id,
        ConversationRef::new(conversation.external_id.clone(), kind),
        INJECT_SENDER,
        payload,
    )
    .with_received_at(state.clock().now());

    match supervisor.inject(event).await? {
        InjectOutcome::Queued { message_id } => Ok(format!("queued {}", message_id)),
        InjectOutcome::Duplicate { external_id } => {
            // 走到这里说明 external_id 撞了（不该发生）；不吞：告诉调用方这条没执行。
            Err(HbotError::conflict(format!(
                "注入被去重丢弃：external_id `{external_id}` 已存在"
            )))
        }
    }
}

/// 读取最近 `limit` 条日志行，数组内**最旧在前**（UI 直接往下追加）。
///
/// `bot_id = None` 是全项目视图：包含所有 bot 行与 Agent 行（`source = 'agent'`）。
pub async fn logs_tail(state: &AppState, bot_id: Option<BotId>, limit: usize) -> Response {
    let capped = u32::try_from(limit.min(MAX_LOG_LIMIT)).unwrap_or(u32::MAX);
    let store = Arc::clone(state.store());
    let rows = match state
        .blocking(move || {
            store
                .list_logs(bot_id.as_ref(), capped)
                .map_err(store_failed)
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => return failed(error),
    };

    let mut logs: Vec<RunLog> = rows
        .into_iter()
        .map(|row| RunLog {
            at: Timestamp::from_unix_millis(row.at),
            level: log_level(&row.level),
            line: row.line,
            source: row.source,
        })
        .collect();
    // 仓储按 `ORDER BY id DESC` 返回（新→旧），DTO 要旧→新。
    logs.reverse();

    match serde_json::to_value(logs) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!(
            "logs_tail 序列化失败：{error}"
        ))),
    }
}

/// 列出当前项目下的 bot 及其状态（UI 启动时用它播种运行态，之后只吃 `bot.status`）。
///
/// 语义三条：
///
/// * **没有打开项目** → `-32002`：没有"当前项目"就没有"当前项目的 bot"，返回空数组
///   会让 UI 以为项目里真的没有 bot。
/// * **项目还没登记 / 没有 bot 行** → 空数组：还没 `bot_start` 过是正常状态。
/// * **`status` 以本进程活跃态优先**：registry 里有 supervisor 就是 `running`、占位中就是
///   `starting`；否则回落到库里最后一次落盘的值。上一进程崩溃留下的 `running` / `error`
///   行按 `stopped` 报——本进程没有它的 supervisor，报"在跑"就是骗 UI。
pub async fn bot_list(state: &AppState) -> Response {
    let Some(root) = state.project_root() else {
        return failed(HbotError::invalid(
            "没有打开的项目：bot_list 只能列出当前项目的 bot，先发 project_open",
        ));
    };
    let path = root.to_string_lossy().into_owned();
    // 项目行都没有 = 一个 bot 都没启动过（`bots.project_id` 有外键），空数组即可。
    let Some(project) = (match lookup_project(state, &path).await {
        Ok(project) => project,
        Err(error) => return failed(error),
    }) else {
        return match serde_json::to_value(Vec::<BotSummary>::new()) {
            Ok(value) => Response::with_data(value),
            Err(error) => failed(HbotError::internal(format!("bot_list 序列化失败：{error}"))),
        };
    };

    let store = Arc::clone(state.store());
    let project_id = project.id;
    // 一次 spawn_blocking 里把"该项目的 bot + 各自状态"读完：两次 await 之间
    // 状态可能变，读成一个快照更不容易自相矛盾。
    let rows = match state
        .blocking(move || {
            let bots = store.list_bots().map_err(store_failed)?;
            let mut rows = Vec::new();
            for bot in bots {
                if bot.project_id != project_id {
                    continue;
                }
                let status = store.get_bot_status(&bot.id).map_err(store_failed)?;
                rows.push((bot, status));
            }
            Ok(rows)
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => return failed(error),
    };

    let summaries: Vec<BotSummary> = rows
        .into_iter()
        .map(|(bot, status)| BotSummary {
            bot_id: bot.id,
            name: bot.name,
            platform: bot.platform,
            status: live_status(
                state,
                bot.id,
                status.as_ref().map(|row| row.status.as_str()),
            ),
            updated_at: Timestamp::from_unix_millis(
                status.as_ref().map(|row| row.updated_at).unwrap_or(0),
            ),
        })
        .collect();

    match serde_json::to_value(summaries) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!("bot_list 序列化失败：{error}"))),
    }
}

/// 列出某个 bot 已登记的会话（`bot_conversations`）。
///
/// 这是 UI 拿 `conversationId` 的正路：`bot_inject` 必须给它，而 `bot_start` 的
/// 返回是 `null`。此前 UI 只能正则匹配 `bot_start` 落的那行 info 日志。
///
/// 语义（与 `dto/mod.rs` 的形状表一致）：
///
/// * **bot 行不存在** → `-32001`，`data = {entity:"bot", id:"<bot_id>"}`。这条命令的
///   输入只有 `bot_id`，bot 行在不在就是它能判断的全部；不额外引入"项目未打开"分支
///   （那是 `bot_list` 的语义，它的输入是"当前项目"）。
/// * **bot 存在但一个会话都没有** → 成功 + **空数组**。空数组是"还没人跟它说过话"
///   这个正常状态：不是错误，也不是 `null`（`null` 会让 UI 的 `for … of` 直接炸）。
/// * **`kind` 原样透传**库里的字符串。契约里它是开放字符串（`private`/`group`/`channel`），
///   在这里发明枚举或归一化，等于平台每多一种会话形态就要先改这条命令。
/// * 顺序由存储层固定为 `(kind, external_id)`（见 `Store::list_conversations_for_bot`），
///   这里不再排序——否则两处排序规则迟早分叉。
pub async fn bot_conversations(state: &AppState, bot_id: BotId) -> Response {
    let store = Arc::clone(state.store());
    // 一次 spawn_blocking 里查两件事：bot 行 + 会话表。拆成两次 await 会落在不同的
    // 快照上（bot 刚被删、会话还在），而这里没有并发收益可言。
    let rows = state
        .blocking(move || {
            if store.get_bot(&bot_id).map_err(store_failed)?.is_none() {
                return Err(HbotError::not_found("bot", bot_id.to_string()));
            }
            store
                .list_conversations_for_bot(&bot_id)
                .map_err(store_failed)
        })
        .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return failed(error),
    };

    let summaries: Vec<ConversationSummary> = rows
        .into_iter()
        .map(|row| ConversationSummary {
            conversation_id: row.id,
            external_id: row.external_id,
            kind: row.kind,
        })
        .collect();

    match serde_json::to_value(summaries) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!(
            "bot_conversations 序列化失败：{error}"
        ))),
    }
}

/// 列一个 bot 最近的作业行（**新的在前**），给作业面板用。
///
/// 只读：不派发、不改状态、不跑 handler。`bot` 必须存在 —— 对一个不存在的 bot
/// 返回空数组会让面板显示「这个 bot 什么都没跑过」，而真相是这个 id 写错了（`-32001`）。
pub async fn bot_jobs(state: &AppState, bot_id: BotId, limit: usize) -> Response {
    let capped = u32::try_from(limit.min(MAX_VIEW_LIMIT)).unwrap_or(u32::MAX);
    let store = Arc::clone(state.store());
    let rows = match state
        .blocking(move || {
            if store.get_bot(&bot_id).map_err(store_failed)?.is_none() {
                return Err(HbotError::not_found("bot", bot_id.to_string()));
            }
            store
                .list_recent_jobs_for_bot(&bot_id, capped)
                .map_err(store_failed)
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => return failed(error),
    };

    let views: Vec<JobView> = rows
        .into_iter()
        .map(|row| JobView {
            id: row.id,
            name: row.name,
            state: row.state,
            attempts: row.attempts,
            budget_ms: row.budget_ms,
            conversation: row.conversation,
            next_at: row.next_at,
            last_error: row.last_error,
            created_at: row.created_at,
        })
        .collect();

    match serde_json::to_value(views) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!("bot_jobs 序列化失败：{error}"))),
    }
}

/// 列一个 bot 最近的等待行（**新的在前**），等待面板用。
///
/// 与 [`bot_jobs`] 同口径：只读快照、终态行也回、bot 不存在时 `-32001`。
/// 这里**不**重发提示、也不结算超时 —— 那是 tick 的活，面板读一眼不该改任何状态。
pub async fn bot_waits(state: &AppState, bot_id: BotId, limit: usize) -> Response {
    let capped = u32::try_from(limit.min(MAX_VIEW_LIMIT)).unwrap_or(u32::MAX);
    let store = Arc::clone(state.store());
    let rows = match state
        .blocking(move || {
            if store.get_bot(&bot_id).map_err(store_failed)?.is_none() {
                return Err(HbotError::not_found("bot", bot_id.to_string()));
            }
            store
                .list_recent_waits_for_bot(&bot_id, capped)
                .map_err(store_failed)
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => return failed(error),
    };

    let views: Vec<WaitView> = rows
        .into_iter()
        .map(|row| WaitView {
            id: row.id,
            key: row.key,
            prompt: row.prompt,
            scope: row.scope,
            state: row.state,
            conversation: row.conversation,
            deadline: row.deadline,
            reminders_left: row.reminders_left,
            timeout_ms: row.timeout_ms,
            created_at: row.created_at,
        })
        .collect();

    match serde_json::to_value(views) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!(
            "bot_waits 序列化失败：{error}"
        ))),
    }
}

/// `bots.status` 列（TEXT）→ DTO 的 [`BotStatus`]，先看本进程活跃态。
///
/// `degraded` 有对应档；`running` / `error` / `stopped` 在没有 supervisor 时一律
/// `stopped`——DTO 的 `BotStatus` 没有 `Error`（见模块文档），失败原因走
/// `bot.status` 事件的 `error` 字段与日志，不塞进状态枚举。
fn live_status(state: &AppState, bot_id: BotId, persisted: Option<&str>) -> BotStatus {
    if state.bots().is_running(&bot_id) {
        return BotStatus::Running;
    }
    if state.bots().is_reserved(&bot_id) {
        return BotStatus::Starting;
    }
    match persisted {
        Some(BOT_STATUS_DEGRADED) => BotStatus::Degraded,
        Some("stopped") | None => BotStatus::Stopped,
        Some(other) => {
            tracing::debug!(
                bot = %bot_id,
                status = other,
                "库里这份状态没有对应的活跃 supervisor，按 stopped 返回"
            );
            BotStatus::Stopped
        }
    }
}

/// `logs.level` 列（TEXT）→ DTO 的 [`LogLevel`]；未知值按 info 处理并留痕。
fn log_level(raw: &str) -> LogLevel {
    match raw {
        "trace" => LogLevel::Trace,
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        "info" => LogLevel::Info,
        other => {
            tracing::debug!(level = other, "日志行的 level 不在枚举内，按 info 返回");
            LogLevel::Info
        }
    }
}

/// 读库里的 bot 行（阻塞调用包进 `state.blocking`）。
async fn load_bot(state: &AppState, bot_id: BotId) -> Result<Option<Bot>, HbotError> {
    let store = Arc::clone(state.store());
    state
        .blocking(move || store.get_bot(&bot_id).map_err(store_failed))
        .await
}

/// 库里没有 bot 行时按 `bots.json` + 项目根补建（不落库，交给 supervisor 的 `ensure_bot_row`）。
async fn bootstrap_bot(
    state: &AppState,
    root: &Path,
    config: &BotConfig,
    bot_id: BotId,
) -> Result<Bot, HbotError> {
    let project = ensure_project(state, root).await?;
    let name = config.name.clone().unwrap_or_else(|| project_name(root));
    let platform = config
        .platform
        .clone()
        .unwrap_or_else(|| PLATFORM.to_owned());
    tracing::info!(
        bot = %bot_id,
        project = %project.id,
        platform = %platform,
        "库里还没有这个 bot：按 bots.json 补建 bots 行（M0 引导路径）"
    );
    Ok(Bot {
        id: bot_id,
        project_id: project.id,
        name,
        platform,
        // 真值在 bots.json；库里这份只是占位（token 永不入库）。
        config_json: "{}".to_owned(),
        enabled: true,
        created_at: state.clock().now().unix_millis(),
    })
}

/// 确保项目根在 `projects` 里有一行（`bots.project_id` 的外键依赖它）。
async fn ensure_project(state: &AppState, root: &Path) -> Result<Project, HbotError> {
    let path = root.to_string_lossy().into_owned();
    if let Some(project) = lookup_project(state, &path).await? {
        return Ok(project);
    }

    let project = Project {
        id: state.ids().project_id(),
        name: project_name(root),
        path: path.clone(),
        created_at: state.clock().now().unix_millis(),
    };
    let inserted = {
        let store = Arc::clone(state.store());
        let to_insert = project.clone();
        state
            .blocking(move || store.insert_project(&to_insert).map_err(store_failed))
            .await
    };
    match inserted {
        Ok(()) => {
            tracing::info!(project = %project.id, path = %path, "项目根已登记到 projects 表");
            Ok(project)
        }
        Err(error) => {
            // 并发启动两个 bot 时两边都会走到这里，后到的那次撞 UNIQUE(path)：
            // 重查一次，拿到先到者写的行；还是没有就把原错误抛出去。
            match lookup_project(state, &path).await? {
                Some(project) => Ok(project),
                None => Err(error),
            }
        }
    }
}

async fn lookup_project(state: &AppState, path: &str) -> Result<Option<Project>, HbotError> {
    let store = Arc::clone(state.store());
    let path = path.to_owned();
    state
        .blocking(move || store.get_project_by_path(&path).map_err(store_failed))
        .await
}

/// 项目根的目录名（用作 `projects.name` / `bots.name` 的缺省值）。
fn project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(|name| name.to_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// 写 `bots.status`（失败映射成 `store_failed`）。
async fn persist_status(
    state: &AppState,
    bot_id: BotId,
    status: &'static str,
) -> Result<(), HbotError> {
    let now = state.clock().now().unix_millis();
    let store = Arc::clone(state.store());
    state
        .blocking(move || {
            store
                .update_bot_status(&bot_id, status, now)
                .map_err(store_failed)
        })
        .await?;
    Ok(())
}

/// 启动失败的收尾：库记 `error`、推 `BotStatus`（带原因），自己不再返回错误。
async fn report_start_failure(state: &AppState, bot_id: BotId, message: &str) {
    tracing::error!(bot = %bot_id, error = message, "bot 启动失败");
    if let Err(error) = persist_status(state, bot_id, BOT_STATUS_ERROR).await {
        tracing::warn!(bot = %bot_id, error = %error, "启动失败后写 bots.status 也失败了");
    }
    state.events().emit(Event::BotStatus {
        bot_id,
        status: BotStatus::Stopped,
        error: Some(message.to_owned()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::ProjectId;
    use hbot_store::NewAgentLog;

    use crate::app::test_support::{async_temp_state_with_project, temp_state};

    const AT: i64 = 1_700_000_000_000;

    /// 取 `bot_list` 的 data 并断言它是数组。
    fn array_of(response: Response) -> Vec<serde_json::Value> {
        let data = response
            .into_result()
            .expect("bot_list 不该失败")
            .expect("bot_list 必须带 data");
        match data {
            serde_json::Value::Array(items) => items,
            other => panic!("bot_list 的 data 应该是 JSON 数组，实际是 {other}"),
        }
    }

    /// 取 `logs_tail` 的 data 并断言它是数组。
    fn log_array_of(response: Response) -> Vec<serde_json::Value> {
        let data = response
            .into_result()
            .expect("logs_tail 不该失败")
            .expect("logs_tail 必须带 data");
        match data {
            serde_json::Value::Array(items) => items,
            other => panic!("logs_tail 的 data 应该是 JSON 数组，实际是 {other}"),
        }
    }

    /// 在库里插一行项目（`bots.project_id` 的外键依赖它）。
    async fn insert_project(state: &AppState, root: &Path) -> Project {
        let project = Project {
            id: state.ids().project_id(),
            name: "测试项目".to_owned(),
            path: root.to_string_lossy().into_owned(),
            created_at: state.clock().now().unix_millis(),
        };
        let store = Arc::clone(state.store());
        let to_insert = project.clone();
        state
            .blocking(move || store.insert_project(&to_insert).map_err(store_failed))
            .await
            .expect("insert_project");
        project
    }

    /// 在库里插一行 bot，返回它的 id。
    async fn insert_bot(state: &AppState, project: ProjectId, name: &str) -> BotId {
        let bot = Bot {
            id: state.ids().bot_id(),
            project_id: project,
            name: name.to_owned(),
            platform: PLATFORM.to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: state.clock().now().unix_millis(),
        };
        let bot_id = bot.id;
        let store = Arc::clone(state.store());
        state
            .blocking(move || store.insert_bot(&bot).map_err(store_failed))
            .await
            .expect("insert_bot");
        bot_id
    }

    /// 写 `bots.status`（模拟上一进程落盘的状态）。
    async fn persist(state: &AppState, bot_id: BotId, status: &'static str, at: i64) {
        let store = Arc::clone(state.store());
        state
            .blocking(move || {
                store
                    .update_bot_status(&bot_id, status, at)
                    .map(|_| ())
                    .map_err(store_failed)
            })
            .await
            .expect("update_bot_status");
    }

    // 没打开项目 = 没有"当前项目的 bot"：必须报 -32002，而不是给一个空数组让 UI 以为项目里真没 bot。
    #[tokio::test]
    async fn bot_list_without_an_open_project_is_invalid() {
        let (_dir, state) = temp_state();

        match bot_list(&state).await.into_result() {
            Ok(data) => panic!("没打开项目时 bot_list 必须报错，实际返回 {data:?}"),
            Err(error) => assert_eq!(error.code, -32002, "没打开项目应该是 -32002：{error:?}"),
        }
    }

    // 项目打开了但一次都没 bot_start 过：projects/bots 里都没有行，空数组是正常状态。
    #[tokio::test]
    async fn bot_list_is_empty_before_any_bot_was_started() {
        let (_dir, state, _root) = async_temp_state_with_project().await;

        assert!(array_of(bot_list(&state).await).is_empty());
    }

    // 只有当前项目的 bot 进列表；键集与线上驼峰名必须与契约一字不差。
    #[tokio::test]
    async fn bot_list_reports_rows_of_the_current_project_only() {
        let (_dir, state, _root) = async_temp_state_with_project().await;
        // 用 `project_root()` 而不是夹具给的原始路径：`Workspace::open` 会把根规范化，
        // `bot_list` 查库用的也是这个值（用原始路径会查不到项目行）。
        let root = state.project_root().expect("已打开项目");
        let project = insert_project(&state, &root).await;
        // 另一个项目（路径不必真实存在：这里只是库里一行，用来验证过滤）。
        let other = insert_project(&state, Path::new("D:/hbot-another-project")).await;
        let bot_id = insert_bot(&state, project.id, "演示 bot").await;
        insert_bot(&state, other.id, "别的项目的 bot").await;
        persist(&state, bot_id, BOT_STATUS_DEGRADED, AT).await;

        let rows = array_of(bot_list(&state).await);
        assert_eq!(rows.len(), 1, "只该列出当前项目的 bot：{rows:?}");

        let row = &rows[0];
        let mut keys: Vec<&str> = row
            .as_object()
            .expect("每一行必须是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["botId", "name", "platform", "status", "updatedAt"],
            "键集必须与 BotSummary 的线上形状一致"
        );
        assert_eq!(row["botId"], json!(bot_id.to_string()));
        assert_eq!(row["name"], json!("演示 bot"));
        assert_eq!(row["platform"], json!(PLATFORM));
        // degraded 没有对应的活跃 supervisor 也照样报出来：它是平台侧的状态，不是本进程的。
        assert_eq!(row["status"], json!("degraded"));
        assert_eq!(row["updatedAt"], json!(AT));
    }

    // 库里残留 running 但本进程没有 supervisor：不能报"在跑"（那是骗 UI）；占位中报 starting。
    #[tokio::test]
    async fn bot_list_prefers_the_live_registry_state() {
        let (_dir, state, _root) = async_temp_state_with_project().await;
        let root = state.project_root().expect("已打开项目");
        let project = insert_project(&state, &root).await;
        let bot_id = insert_bot(&state, project.id, "崩溃残留的 bot").await;
        persist(&state, bot_id, BOT_STATUS_RUNNING, AT).await;

        assert_eq!(
            array_of(bot_list(&state).await)[0]["status"],
            json!("stopped"),
            "本进程没有它的 supervisor，残留的 running 必须按 stopped 报"
        );

        assert!(state.bots().reserve(bot_id), "占位应该成功");
        assert_eq!(
            array_of(bot_list(&state).await)[0]["status"],
            json!("starting"),
            "启动中的占位要报 starting"
        );

        state.bots().release(&bot_id);
        assert_eq!(
            array_of(bot_list(&state).await)[0]["status"],
            json!("stopped")
        );
    }

    // Agent 行没有 bot 归属：`bot_id: null` 的全项目视图必须包含它，按 bot 过滤时必须排除它。
    #[tokio::test]
    async fn logs_tail_none_reads_agent_rows_and_some_filters_them_out() {
        let (_dir, state, _root) = async_temp_state_with_project().await;
        let root = state.project_root().expect("已打开项目");
        let project = insert_project(&state, &root).await;
        let bot_id = insert_bot(&state, project.id, "日志 bot").await;

        state
            .append_log(bot_id, "info", "bot 日志")
            .expect("写 bot 日志");
        let store = Arc::clone(state.store());
        let at = state.clock().now().unix_millis();
        state
            .blocking(move || {
                store
                    .append_agent_log(NewAgentLog {
                        level: "info".to_owned(),
                        line: "[session 3f2a1b9c…] agent 工具 fs_read 成功（1 ms）".to_owned(),
                        at,
                    })
                    .map_err(store_failed)
            })
            .await
            .expect("写 Agent 日志");

        let all = log_array_of(logs_tail(&state, None, 50).await);
        assert_eq!(all.len(), 2, "全项目视图应包含 bot 行与 Agent 行：{all:?}");
        assert_eq!(all[0]["source"], json!("bot"));
        assert_eq!(all[1]["source"], json!("agent"));
        assert_eq!(
            all[1]["line"],
            json!("[session 3f2a1b9c…] agent 工具 fs_read 成功（1 ms）")
        );

        let bot_only = log_array_of(logs_tail(&state, Some(bot_id), 50).await);
        assert_eq!(
            bot_only.len(),
            1,
            "按 bot 过滤不该看到 Agent 行：{bot_only:?}"
        );
        assert_eq!(bot_only[0]["source"], json!("bot"));
    }
}
