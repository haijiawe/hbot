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
//! * [`qq`]：`BotAdapter` 的 QQ 官方机器人实现（同一套读任务骨架，被动回复必须带 `d.id`）；
//! * [`fingerprint_cache`]：项目内容指纹的两层缓存（元数据 TTL + 内容地板），
//!   保证热路径不读文件内容；
//! * [`sandbox_handler`]：`HandlerExecutor` 的 QuickJS 实现（每 bot ≤2 个 isolate）；
//! * [`outbox_source`]：进程重启后按 bot 续发未发 outbound 的 store 实现；
//! * [`AdapterFactory`]：[`PlatformRouter`] 是生产用的按平台路由，测试可注入假适配器。

pub mod config;
pub mod fingerprint_cache;
pub mod onebot;
pub mod outbox_source;
pub mod qq;
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

/// 允许的平台取值（**唯一一份**）。顺序即错误消息里的列举顺序。
///
/// 新增平台时在这里加一项、在 [`PlatformRouter`] 的 `match` 里加一条分支；两处少一处都会
/// 在 `bots.json` 校验与路由上露馅（前者报"不认识"，后者 `match` 编译不过）。
pub const PLATFORMS: [&str; 2] = [onebot::PLATFORM, qq::PLATFORM];

/// 允许的平台值 → 给人看的一句话（错误消息用，例如 "`onebot11`、`qq_official`"）。
pub fn platform_list() -> String {
    PLATFORMS
        .iter()
        .map(|platform| format!("`{platform}`"))
        .collect::<Vec<_>>()
        .join("、")
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

/// 生产用的适配器工厂：按 `bots.platform` 精确匹配，选 OneBot 或 QQ 官方。
///
/// 为什么要有这一层：在这之前 `AppState::open` 硬绑 [`onebot::OneBotAdapterFactory`]，
/// 于是 QQ 官方适配器（`crates/hbot-adapters/src/platforms/qq`）写好了却**没有任何调用方**
/// —— `bot_start` 对一个 `platform = "qq_official"` 的 bot 会拿 OneBot 工厂去连一个
/// 不存在的 `ws://` 地址。路由把"平台 → 工厂"这一跳补上，并且：
///
/// * **字符串精确匹配**（不做前缀/大小写/别名匹配）：`bots.json` 与 `bots.platform` 列
///   里存的都必须是 [`onebot::PLATFORM`] 或 [`qq::PLATFORM`]，其它值 -32002 并列出允许值；
/// * `bots.json` 里显式写了 `platform` 但与库里的 `bots.platform` 不一致时**报错**，
///   不按任何一边静默降档：两处不一致说明用户在改配置但没改库，猜一边就是猜错一半
///   （`docs/02-architecture.md` §4.2「写错档位不许静默降档」）；
/// * 具体的 [`onebot::OneBotAdapterFactory`] / [`qq::QqAdapterFactory`] 都保留为可注入的
///   具体类型：测试注入假工厂的既有能力（[`crate::app::AppState::with_adapter_factory`]）
///   不受影响，也仍然可以单独测某一个平台的工厂。
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRouter;

impl AdapterFactory for PlatformRouter {
    fn create(&self, bot: &Bot, config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError> {
        match resolve_platform(bot, config)? {
            onebot::PLATFORM => onebot::OneBotAdapterFactory.create(bot, config),
            qq::PLATFORM => qq::QqAdapterFactory.create(bot, config),
            other => Err(unknown_platform(bot, other)),
        }
    }
}

/// `bot.platform`（库里的列，权威）与 `bots.json` 的 `platform` 对账，返回生效的平台值。
///
/// 平台以库为准（`bots.platform` 是 `bot_start` 拿到的行的一部分），但 `bots.json` 显式
/// 写了别的值时不能装作没看见：那说明配置与库已经分叉，按哪边都可能是错的，必须让用户
/// 自己决定改哪边。
///
/// # Errors
///
/// 两边不一致、或平台值不认识 —— 都是 -32002，消息里列出允许值。
pub fn resolve_platform<'a>(bot: &'a Bot, config: &'a BotConfig) -> Result<&'a str, HbotError> {
    let stored = bot.platform.trim();
    if let Some(declared) = config
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|declared| !declared.is_empty())
        && declared != stored
    {
        return Err(HbotError::invalid(format!(
            "bot {} 的平台不一致：bots.platform 列是 `{stored}`，bots.json 里写的是 \
             `{declared}`；两边必须一致（允许的平台：{}）",
            bot.id,
            platform_list()
        )));
    }
    if !PLATFORMS.contains(&stored) {
        return Err(unknown_platform(bot, stored));
    }
    Ok(stored)
}

/// 平台值不认识时的错误：**列出允许值**，不悄悄当成 OneBot。
fn unknown_platform(bot: &Bot, platform: &str) -> HbotError {
    HbotError::invalid(format!(
        "bot {} 的平台 `{platform}` 不认识：允许的取值是 {}（`{}` 等价于不写 platform）",
        bot.id,
        platform_list(),
        onebot::PLATFORM
    ))
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
