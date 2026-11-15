//! 运行时装配：把适配器、执行器、supervisor 拼成一个能跑的 bot。
//!
//! 这是 `bot_start` 的唯一实现路径（`docs/02-architecture.md` §4）：
//!
//! ```text
//! OneBotClient ──► BotAdapter（入站归一化 / 出站发送）
//!                      │
//!                      ▼
//!                 Supervisor ──► SessionQueues ──► SandboxHandler（QuickJS）
//!                      │
//!                      ▼
//!                   Outbox ──► BotAdapter::send
//! ```
//!
//! 为什么装配放在 `hbot-rpc` 而不是 `hbot-bot`：`hbot-bot` 不认识平台协议，也不认识
//! QuickJS（两个 crate 各自被测试独立驱动）。把"谁和谁接上"放在应用层，是新平台、
//! 新执行器接入时唯一要改的地方。
//!
//! ## 组成
//!
//! * [`config`]：项目根的 `bots.json`（M0 的 bot 配置来源，schema 见该文件）；
//! * [`onebot`]：`BotAdapter` 的 OneBot 11 实现（单一读任务 + 有界 mpsc）；
//! * [`fingerprint_cache`]：项目内容指纹的两层缓存（元数据 TTL + 内容地板），
//!   保证热路径不读文件内容；
//! * [`sandbox_handler`]：`HandlerExecutor` 的 QuickJS 实现（每 bot ≤2 个 isolate）；
//! * [`outbox_source`]：进程重启后按 bot 续发未发 outbound 的 store 实现；
//! * [`AdapterFactory`]：装配点唯一按平台分支的地方，测试可注入假适配器。

pub mod config;
pub mod fingerprint_cache;
pub mod onebot;
pub mod outbox_source;
pub mod sandbox_handler;

use std::sync::Arc;

use hbot_bot::BotAdapter;
use hbot_core::{ConversationKind, HbotError};
use hbot_store::Bot;

use crate::app::runtime::config::BotConfig;

/// 装配期错误：把各 crate 的错误归一成一句能看懂的说明。
///
/// 这里不新增错误码，只在需要时用 [`HbotError::invalid`] / `internal` 包一层：
/// wire 上的错误码只有 `docs/02-architecture.md` §9.4 那一张表。
pub fn setup_failed(what: &str, detail: impl std::fmt::Display) -> HbotError {
    HbotError::invalid(format!("{what}: {detail}"))
}

/// 按平台造一个 `BotAdapter`。
///
/// `bot_start` 走到这里时，配置、数据库行、执行器都已经就位；**这是唯一**按平台分支的
/// 地方（`docs/03-directory.md` §2：`hbot-rpc` 负责编排，`hbot-adapters` 负责协议）。
///
/// 存在的另一个理由是测试：真机测试不能连真的 OneBot 服务端，[`crate::app::AppState`]
/// 允许换掉工厂注入假适配器，而队列、去重、outbox、沙箱这些真链路一条都不替换。
pub trait AdapterFactory: Send + Sync + 'static {
    /// 造一个还没开始消费事件的适配器。
    ///
    /// 失败必须是**真原因**（如 `wss://` 的证书校验失败报 `AdapterError::Tls`），不得退回到
    /// 一个"什么也不做"的实现。
    fn create(&self, bot: &Bot, config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError>;
}

/// 会话类型 → `hbot_store` 里 `conversations.kind` 列的字符串。
///
/// 与 `hbot_adapters::platforms::onebot::conversation_kind_str` 的取值一致
/// （`private` / `group` / `channel` / `unknown`）：存储层只有一列文本，两个方向必须是
/// 同一张映射表，否则 `bot_inject` 造出来的会话类型会和适配器归一化出来的不一样。
pub fn conversation_kind_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Private => "private",
        ConversationKind::Group => "group",
        ConversationKind::Channel => "channel",
        ConversationKind::Unknown => "unknown",
    }
}

/// `conversations.kind` 字符串 → 会话类型；无法识别时归为
/// [`ConversationKind::Unknown`]（不猜、不报错：老数据可能来自更早的版本）。
pub fn conversation_kind_from_str(kind: &str) -> ConversationKind {
    match kind {
        "private" => ConversationKind::Private,
        "group" => ConversationKind::Group,
        "channel" => ConversationKind::Channel,
        _ => ConversationKind::Unknown,
    }
}
