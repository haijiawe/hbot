//! 回放命令：用当前代码重跑历史入站，逐条对比历史出站。
//!
//! 实现内核在 [`hbot_replay`]（与 CLI 的 `hbot replay` **共用同一份**）：本文件只做三件事
//! ——校验参数、在阻塞线程池里跑回放、把内部报告映射成 wire DTO（`dto/common.rs` 的
//! `ReplayReport`）。回放要**编译项目**再**逐条 dispatch**，是同步的重活，所以绝不能在
//! 命令线程上跑（会卡住整个 WebSocket 派发），必须走 [`AppState::blocking`]。
//!
//! ## 读的是哪一份库
//!
//! IDE / 桌面 / 无头进程用的是 [`AppState`] 里那份**应用库** `<data_dir>/hbot.db`，
//! 不是 CLI 用的 `<project>/.hbot/hbot.db`。理由与边界写在 `hbot-replay` 的 crate 文档
//! "两种库"一节：桌面进程的 bot 是 `AppState` 起的，历史入站只可能在应用库里；而 CLI 的
//! `hbot replay` 是独立进程，读的是项目自己那份 `.hbot/hbot.db`。两边语义一样（都是
//! "这个 bot 跑过什么"），但**文件不是同一个**，所以 `hbot-replay` 才设计成"调用方给
//! `&Store`"，而不是自己去 `Store::open`。
//!
//! ## 错误码
//!
//! | 情况 | 码 | 谁来判 |
//! |---|---|---|
//! | `limit = Some(0)` | `-32002` | 本文件（进 blocking 之前） |
//! | 没有打开的项目 | `-32002` | 本文件（`state.project_root()`） |
//! | 库里没有这个 bot | `-32001` | blocking 闭包里预检 |
//! | 没有历史入站 | `-32001` | blocking 闭包里预检 |
//! | 编译失败 / isolate 起不来 / 读库失败 / payload 坏了 | `-32006` | `hbot_replay` 的 `report.error` |
//!
//! 前两类是"参数/前置条件不对"，属于调用方的错；后两类是"这台机器上跑不起来"，
//! 属于环境。把"没有历史"判成 `-32001` 而不是 `-32006`，是因为 UI 该提示"这个 bot 还没
//! 跑过消息"，而不是"内部错误"。

use std::sync::Arc;

use super::{failed, unimplemented};
use crate::app::AppState;
use crate::dto::{Command, ReplayEntry, ReplayReplyChange, ReplayReport as WireReport, Response};
use hbot_core::{BotId, HbotError};

/// `replay_run` 的入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::ReplayRun {
            bot_id,
            from,
            limit,
        } => run(state, bot_id, from, limit).await,
        other => unimplemented(other.kind()),
    }
}

/// 真正的流程：校验 → 阻塞跑 → 映射。
async fn run(state: &AppState, bot_id: BotId, from: Option<i64>, limit: Option<u32>) -> Response {
    // `limit = 0` 不是"默认值"，是"一条都不回放"：`list_inbound_history_for_bot` 会把 0
    // 当 LIMIT 0，最后得到一份空报告。与其让 UI 收到一份`events = 0`的"成功"，不如直接
    // 说清楚要全量就别给 limit。
    if limit == Some(0) {
        return failed(HbotError::invalid(
            "limit=0 会一条都不回放；要全量就别给 limit",
        ));
    }
    let Some(project_dir) = state.project_root() else {
        return failed(HbotError::invalid(
            "没有打开的项目：先发 project_open（回放要用项目根里的 hbot.json 与源码）",
        ));
    };

    let store = Arc::clone(state.store());
    let options = hbot_replay::ReplayOptions {
        database: state.data_dir().join("hbot.db"),
        project_dir,
        bot: bot_id.to_string(),
        limit,
        from,
        save_fixture: None,
    };
    // 闭包要 `'static`：只搬 `Arc<Store>` 与 `options`，不借 `state`。
    let replayed = state
        .blocking(move || -> Result<hbot_replay::ReplayReport, HbotError> {
            // 两类"根本没法开始"的情况在这里分开判，别等 `report.error` 的字符串文案：
            // 那会把"bot 选错了"说成 `-32006` 内部错误。
            if store
                .get_bot(&bot_id)
                .map_err(crate::app::store_failed)?
                .is_none()
            {
                return Err(HbotError::not_found("bot", bot_id.to_string()));
            }
            let history = store
                .list_inbound_history_for_bot(
                    &bot_id,
                    options.from,
                    options.limit.unwrap_or(u32::MAX),
                )
                .map_err(crate::app::store_failed)?;
            if history.is_empty() {
                return Err(HbotError::not_found("inbound", bot_id.to_string()));
            }
            drop(history);

            let report = hbot_replay::replay(&store, &options);
            match report.error.clone() {
                // 编译失败 / isolate 起不来 / 读库失败 / payload 坏了都在这里：文案带真实
                // 原因（`hbot_replay` 的每一处错误都已经拼了路径与底层 error）。
                Some(error) => Err(HbotError::internal(error)),
                None => Ok(report),
            }
        })
        .await;

    let report = match replayed {
        Ok(report) => report,
        Err(error) => return failed(error),
    };
    match serde_json::to_value(wire(&report)) {
        Ok(data) => Response::with_data(data),
        Err(error) => failed(HbotError::internal(format!("回放报告序列化失败：{error}"))),
    }
}

/// 内部报告 → wire 报告（字段一一对应）。
fn wire(report: &hbot_replay::ReplayReport) -> WireReport {
    WireReport {
        project: report.project.clone(),
        bot: report.bot.clone(),
        from: report.from,
        limit: report.limit,
        events: count(report.events),
        matched_events: count(report.matched_events),
        differing_events: count(report.differing_events),
        failed_events: count(report.failed_events),
        same: count(report.same),
        extra: count(report.extra),
        missing: count(report.missing),
        reordered: count(report.reordered),
        entries: report.entries.iter().map(entry).collect(),
    }
}

/// 一条内部明细 → wire 明细。
fn entry(detail: &hbot_replay::InboundReplay) -> ReplayEntry {
    ReplayEntry {
        inbound_id: detail.inbound_id.clone(),
        external_id: detail.external_id.clone(),
        conversation_external_id: detail.conversation_external_id.clone(),
        received_at: detail.received_at,
        status: detail.status.as_str().to_owned(),
        historical: detail.historical.clone(),
        replayed: detail.replayed.clone(),
        same: detail.same.clone(),
        extra: detail.extra.clone(),
        missing: detail.missing.clone(),
        reordered: detail
            .reordered
            .iter()
            .map(|change| ReplayReplyChange {
                text: change.text.clone(),
                historical_index: count(change.historical_index),
                replayed_index: count(change.replayed_index),
            })
            .collect(),
        error: detail.error.clone(),
    }
}

/// `usize` → `u32`：wire 上是 `u32`（`dto/common.rs` 的既有形状）。一批最多几十万条，
/// 真溢出不可能，但 `as` 会静默截断，所以显式饱和而不是强转。
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
