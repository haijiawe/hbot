//! QQ 官方机器人的 [`BotAdapter`] 实现（`bots.json` 的 `platform = "qq_official"`）。
//!
//! ## 为什么要有这个文件
//!
//! `hbot_adapters::platforms::qq` 里已经有一套完整的 QQ 官方机器人协议实现
//! （取 access token、连 WebSocket 网关、心跳与 resume、`GROUP_AT_MESSAGE_CREATE` /
//! `C2C_MESSAGE_CREATE` 归一化、被动回复）。但它是**具体协议客户端**，没有任何调用方：
//! 运行时的 `AdapterFactory` 只认 OneBot。这个文件是那把接线钳子 ——
//! 把 `QqClient` 接到 `hbot_bot::BotAdapter` 这个**端口**上，让 `bot_start` 对
//! `platform = "qq_official"` 的 bot 真的能收事件、跑 `bot.ts`、被动回复。
//!
//! ## 所有权：读者任务（与 `onebot.rs` 同一套解法）
//!
//! `QqClient::next_event(&mut self)` 要 `&mut`，而 `BotAdapter::subscribe(&self)` 只有
//! `&self`。解法不是 `Mutex<QqClient>`（读取循环得反复借那把锁，`subscribe` 之后就没法
//! 连续取事件），而是让**一个读者任务独占 client 的可变所有权**：
//!
//! ```text
//! subscribe(): probe_access_token ──► QqClient::start ──► tokio::spawn(reader) ──► mpsc::Sender<PlatformEvent>
//!              （一次性）                └── QqSender ──► 存进 self.reply（Clone，内部有界 channel）
//! subscribe() 返回：那个 mpsc::Receiver（一次性）
//! send():      self.reply.send_group / send_c2c
//! ```
//!
//! 事件 channel 是**有界**的（容量 [`ADAPTER_EVENT_CAPACITY`]）：满了读者的
//! `send().await` 就会卡住 → 不再 `next_event()` → 不再从网关读帧。
//! **暂停读取，不丢消息**（`docs/02-architecture.md` §4.1）。
//!
//! ## 已知边界（不要在这里假装支持）
//!
//! * **只支持被动回复**。QQ 官方平台要求回复必须带触发它的入站消息 ID（事件的 `d.id`），
//!   主动推送需要另一套"模板/主动消息"配额，适配器没有实现
//!   （`QqSender::send_group_active` / `send_c2c_active` 恒返回 `Unsupported`）。
//!   所以 `OutboundEnvelope.reply_to` 为 `None` 时这里**返回真错误**（-32005 denied），
//!   不伪造成功、不静默丢弃：伪造成功会让 outbound 行标成已发送，用户永远等不到消息，
//!   而日志里一个字都没有。
//! * 会话类型只有 `Group`（群）与 `Private`（单聊 c2c）。`Channel` / `Unknown` 明确报错
//!   —— QQ 官方的频道（`d.channel_id`）走的是另一套端点，适配器侧没有验证过，
//!   这里不猜。
//! * **启动前探测 token 放在 `subscribe()`**，不放在 `create()`：`create` 是
//!   `AdapterFactory` 的同步端口，既有语义是"只校验配置、不碰网络"
//!   （`tests/bot_runtime.rs::wss_is_supported_by_the_onebot_adapter` 钉了这条契约），
//!   而 `QqClient::start` 会立刻 spawn 一个去 `bots.qq.com` 取 token 的后台任务。
//!   探测失败经 `subscribe` 冒到 `bot_start` 的错误返回，效果与 OneBot 的
//!   `probe_connection` 相同：配置错在"点启动"那一下就说清楚，而不是启动成功 +
//!   后台无限重连的 warn。
//! * 日志里**不打印 `client_secret`**（`QqConfig` 的 `Debug` 已脱敏；本文件只在
//!   debug 级别打 `app_id`，它不是密钥，但仍然不刷屏）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hbot_adapters::AdapterError;
use hbot_adapters::qq::{
    NormalizedInbound, QqClient, QqConfig, QqSender, SendReceipt, probe_access_token,
};
use hbot_bot::supervisor::ADAPTER_EVENT_CAPACITY;
use hbot_bot::{AdapterSubscription, BotAdapter, ConversationRef, OutboundEnvelope, PlatformEvent};
use hbot_core::{ConversationKind, HbotError};
use hbot_store::Bot;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::runtime::AdapterFactory;
use crate::app::runtime::config::BotConfig;

/// QQ 官方机器人的平台标识（写进 `bots.json` 的 `platform`）。
pub const PLATFORM: &str = "qq_official";

/// 启动前探测的超时下限：10 秒。
///
/// 与 `onebot.rs` 的 [`PROBE_MIN_TIMEOUT`](super::onebot::PROBE_MIN_TIMEOUT) 同一个理由：
/// 探测的职责是"别让配错的人继续猜"，不是"快速失败"。跨地域 / 公司代理下取 token
/// 慢一点是正常的，用 1 秒那种激进超时会把本来能用的 bot 判成配置错误。
pub const PROBE_MIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 出站端口：把"被动回复"收窄成两条具体调用。
///
/// 为什么要有这一层（而不是直接 `QqSender`）：`QqSender::new` 是 `pub(crate)`，
/// 外部拿不到一个能注入的实例，`send_group_passive` 又必然发 HTTPS。若 `QqAdapter`
/// 直接持 `QqSender`，**"reply_to 传没传下去"就只能在真网络上验**。
/// 抽象出这个 trait 后，集成测试可以实现一个记录参数、永远返回成功的假端口
/// （`tests/qq_runtime.rs`），生产路径仍然只调 `QqSender` 的两个方法。
///
/// 刻意**不**把 `QqSender` 改成 trait 塞进 `hbot-adapters`：那是适配器库的公开 API，
/// 测试缝属于运行时（谁需要可测谁付抽象成本）。
#[async_trait]
pub trait PassiveReply: Send + Sync + 'static {
    /// 群里被动回复：`group_openid` + 入站消息 ID（`d.id`）。
    async fn send_group(
        &self,
        group_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError>;

    /// 单聊被动回复：`user_openid` + 入站消息 ID（`d.id`）。
    async fn send_c2c(
        &self,
        user_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError>;
}

#[async_trait]
impl PassiveReply for QqSender {
    async fn send_group(
        &self,
        group_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        self.send_group_passive(group_openid, msg_id, content).await
    }

    async fn send_c2c(
        &self,
        user_openid: &str,
        msg_id: &str,
        content: &str,
    ) -> Result<SendReceipt, AdapterError> {
        self.send_c2c_passive(user_openid, msg_id, content).await
    }
}

/// 一个 bot 的 QQ 官方适配器：一个读者任务 + 一个被动回复端口。
pub struct QqAdapter {
    /// 凭据与端点（`Debug` 已脱敏 `client_secret`）。只在这里活一次，日志不外带。
    config: QqConfig,
    /// 出站端口：`subscribe()` 成功时由 `QqClient::sender()` 填入；测试可预置假端口。
    reply: Mutex<Option<Arc<dyn PassiveReply>>>,
    /// 读者任务句柄；`Drop` 时 abort（client 在任务里，随之取消）。
    reader: Mutex<Option<JoinHandle<()>>>,
    /// 事件流是一次性交付口：第二次 `subscribe` 必须报错，不能返回一个永远收不到事件的
    /// channel（那会让 supervisor 看起来 Running 却一条消息都收不到）。
    subscribed: AtomicBool,
}

impl QqAdapter {
    /// 只校验凭据、**不碰网络**（不取 token、不连网关）。
    ///
    /// 这与 `OneBotAdapter::from_bot_config` 的 `create` 语义一致：`AdapterFactory::create`
    /// 是同步端口，跑不了 `await`，也不该在"还没点启动"的时候就去连平台。
    ///
    /// # Errors
    ///
    /// `appId` / `clientSecret` 为空、容量为 0、端点是非法 scheme —— 来自
    /// `QqConfig::validate`，经 `From<AdapterError> for HbotError` 映射成 -32002。
    pub fn new(config: QqConfig) -> Result<Self, HbotError> {
        config.validate().map_err(HbotError::from)?;
        Ok(Self {
            config,
            reply: Mutex::new(None),
            reader: Mutex::new(None),
            subscribed: AtomicBool::new(false),
        })
    }

    /// 从项目配置（`bots.json`）建一个 QQ 官方适配器（生产路径）。
    ///
    /// # Errors
    ///
    /// 缺 `appId` / `clientSecret`，或 [`QqAdapter::new`] 的校验失败 —— 都是 -32002，
    /// 消息里点名缺哪个字段。
    pub fn from_bot_config(config: &BotConfig) -> Result<Self, HbotError> {
        Self::new(qq_config(config)?)
    }

    /// 预置一个被动回复端口（**测试缝**，生产路径由 `subscribe()` 自己填）。
    ///
    /// 有了它，"`reply_to` 有没有作为 `msg_id` 传下去"与"`reply_to` 为 `None` 时是不是
    /// 返回 `Err`"都能在**不连真网络**的前提下验（`tests/qq_runtime.rs`）。
    #[doc(hidden)]
    pub fn with_reply_port(
        config: QqConfig,
        reply: Arc<dyn PassiveReply>,
    ) -> Result<Self, HbotError> {
        let adapter = Self::new(config)?;
        *lock(&adapter.reply) = Some(reply);
        Ok(adapter)
    }

    /// 这个适配器用的 AppID（诊断用；不是密钥，但仍然只在 debug 级别打）。
    pub fn app_id(&self) -> &str {
        self.config.app_id()
    }
}

impl std::fmt::Debug for QqAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QqAdapter")
            .field("app_id", &self.config.app_id())
            .field("subscribed", &self.subscribed.load(Ordering::SeqCst))
            .field("has_reply_port", &lock(&self.reply).is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for QqAdapter {
    fn drop(&mut self) {
        // abort 之后 client 在任务里被丢弃，`QqClient::drop` 会发 cancel 并 abort 监督任务。
        if let Some(reader) = lock(&self.reader).take() {
            reader.abort();
        }
    }
}

#[async_trait]
impl BotAdapter for QqAdapter {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    async fn subscribe(&self) -> Result<AdapterSubscription, HbotError> {
        // 一次性：第二次订阅必须是**错误**，不能静默交出空流。
        if self.subscribed.swap(true, Ordering::SeqCst) {
            return Err(HbotError::conflict(
                "qq 官方适配器只能订阅一次：事件读者任务已经把流交给了上一个订阅者",
            ));
        }

        // 启动前探测：`bot_start` 走到这里时配置与数据库行都已就位，但**还没**开始消费
        // 事件。此刻探一次 access token，成功就继续；失败当场报错，让 `bot_start` 带着
        // 一句人话失败（而不是"启动成功 + 后台无限重连的 warn"）。
        probe_startup(&self.config).await?;

        // 探测过了才连：配置错时不要留下半个后台监督任务。
        let mut client = QqClient::start(self.config.clone()).map_err(|error| {
            tracing::error!(
                app_id = self.config.app_id(),
                error = %error,
                "qq 官方网关客户端启动失败：bot_start 将被拒绝"
            );
            HbotError::from(error)
        })?;
        // 出站端口在 client 被移交给读者任务之前取出来（`QqSender` 是 Clone + 内部有界
        // channel，不借用 client）。
        *lock(&self.reply) = Some(Arc::new(client.sender()));

        let (tx, rx) = mpsc::channel::<PlatformEvent>(ADAPTER_EVENT_CAPACITY);
        let reader = tokio::spawn(async move {
            read_events(&mut client, &tx).await;
            // 事件流结束（网关关闭 / 订阅者消失）：显式收尾。
            client.stop().await;
        });
        *lock(&self.reader) = Some(reader);

        tracing::info!(
            app_id = self.config.app_id(),
            "qq 官方适配器已订阅（单一读任务 + 有界事件通道）"
        );
        Ok(AdapterSubscription::new(rx))
    }

    async fn send(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        let reply = lock(&self.reply).clone().ok_or_else(|| {
            // 还没订阅就来发：这是**运行顺序错了**，不是用户配置问题。
            HbotError::internal(format!(
                "qq 官方适配器还没有订阅（QqClient 在 subscribe 里才启动）：bot {} 的这条出站无法发送",
                envelope.bot_id
            ))
        })?;

        // `send` 只回 `()`：回执里的 message_id / msg_seq 属于适配器层，进程里没有
        // 第二个消费者（outbox 记录的是我们自己的 OutboundId）。
        dispatch_passive_reply(reply.as_ref(), &envelope)
            .await
            .map(|_receipt| ())
            .map_err(|error| {
                tracing::warn!(
                    outbound = %envelope.id,
                    conversation = %envelope.conversation_id,
                    kind = crate::app::runtime::conversation_kind_str(envelope.conversation.kind),
                    error = %error,
                    "qq 官方适配器发送失败"
                );
                error
            })
    }
}

/// 通过 `bots.json` 里的 `appId` / `clientSecret` 造真 QQ 官方适配器（生产路径）。
#[derive(Debug, Default, Clone, Copy)]
pub struct QqAdapterFactory;

impl AdapterFactory for QqAdapterFactory {
    fn create(&self, bot: &Bot, config: &BotConfig) -> Result<Arc<dyn BotAdapter>, HbotError> {
        if bot.platform != PLATFORM {
            return Err(HbotError::invalid(format!(
                "bot {} 的平台是 `{}`，不是 `{PLATFORM}`：不能用 QQ 官方适配器启动",
                bot.id, bot.platform
            )));
        }
        let adapter = QqAdapter::from_bot_config(config)?;
        // app_id 不是密钥，但没必要每行日志都带：info 只说结果，debug 才带标识。
        tracing::info!(
            bot = %bot.id,
            "qq 官方适配器已就绪（凭据已校验；事件读者任务在 subscribe 时启动）"
        );
        tracing::debug!(bot = %bot.id, app_id = adapter.app_id(), "qq 官方适配器配置");
        Ok(Arc::new(adapter))
    }
}

/// `BotConfig` → `QqConfig`：缺字段就报 -32002 并**点名缺哪个**。
///
/// 这里再做一次校验（`read_bot_config` 已经按 `platform` 校验过）：`BotConfig` 的所有
/// 字段都是 `pub`，测试与将来的 IDE 写入路径都可能绕过文件解析直接造它。
fn qq_config(config: &BotConfig) -> Result<QqConfig, HbotError> {
    let app_id = config
        .app_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            HbotError::invalid(format!(
                "platform = `{PLATFORM}` 的 bot 缺少 appId：bots.json 里该条目要写 \
                 \"appId\" 与 \"clientSecret\"（QQ 开放平台机器人后台的 AppID / AppSecret）"
            ))
        })?;
    let client_secret = config
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            HbotError::invalid(format!(
                "platform = `{PLATFORM}` 的 bot 缺少 clientSecret：bots.json 里该条目要写 \
                 \"appId\" 与 \"clientSecret\"（QQ 开放平台机器人后台的 AppID / AppSecret）"
            ))
        })?;
    Ok(QqConfig::new(app_id, client_secret))
}

/// 探测用的配置：把连接与请求超时抬到不低于 [`PROBE_MIN_TIMEOUT`]。
fn probe_config(config: &QqConfig) -> QqConfig {
    config
        .clone()
        .with_connect_timeout(config.connect_timeout().max(PROBE_MIN_TIMEOUT))
        .with_request_timeout(config.request_timeout().max(PROBE_MIN_TIMEOUT))
}

/// 启动前取一次 access token；失败时把原因翻成一句人话并记日志。
///
/// 只探 token、不连网关：token 端点覆盖"AppID/AppSecret 错"与"出不了网"这两类最常见的
/// 配置/环境错误，而且不会留下一个连上网关又要断开的连接。
async fn probe_startup(config: &QqConfig) -> Result<(), HbotError> {
    match probe_access_token(&probe_config(config)).await {
        Ok(probe) => {
            // 只打长度与有效期：token 本身一个字都不进日志。
            tracing::debug!(
                app_id = config.app_id(),
                token_len = probe.token_len,
                expires_in_ms = u64::try_from(probe.expires_in.as_millis()).unwrap_or(u64::MAX),
                "qq 官方 access token 探测通过"
            );
            Ok(())
        }
        Err(error) => {
            let reason = describe_probe_failure(&error, config);
            // 日志与错误返回用**同一句**原因：用户在控制台看到的那句话必须能在日志里搜到。
            tracing::error!(
                app_id = config.app_id(),
                error = %error,
                reason = %reason,
                "qq 官方 access token 探测失败：bot_start 将被拒绝"
            );
            Err(probe_error(&error, reason))
        }
    }
}

/// 把取 token 的原始错误翻成"用户能照做"的一句话。
///
/// 分类依据只能是错误文本：`AdapterError` 的粒度到不了"401 / 429 / DNS / 超时"这一层
/// （HTTP 状态码只在消息文本里）。所以按子串分流，**每条分支都给可执行的下一步**。
/// 消息里绝不带 `client_secret`（也不带 token：`probe_access_token` 只在成功时返回它）。
fn describe_probe_failure(error: &AdapterError, config: &QqConfig) -> String {
    let raw = error.to_string();
    let lower = raw.to_ascii_lowercase();
    let token_url = config.token_url();
    let hint = if lower.contains("app id or client secret is not accepted")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        "AppID / AppSecret 不被 QQ 开放平台接受：去 QQ 开放平台机器人后台核对 appId 与 \
         clientSecret（AppSecret 只在后台重置时显示一次，别把机器人 token 当成它）"
    } else if lower.contains("lookup address")
        || lower.contains("no such host")
        || lower.contains("failed to lookup")
        || lower.contains("dns")
    {
        "域名解析失败（DNS）：连不上 QQ 开放平台的取 token 端点 —— 检查本机 DNS / 代理"
    } else if matches!(error, AdapterError::Tls { .. })
        || lower.contains("certificate")
        || lower.contains("unknown issuer")
    {
        "TLS/证书校验失败：取 token 的 HTTPS 端点证书不受信任（公司代理做了中间人替换？\
         本项目没有跳过校验的开关）"
    } else if matches!(error, AdapterError::Timeout { .. }) || lower.contains("timed out") {
        "取 access token 超时：网络不通或被防火墙/代理拦住（bots.qq.com 需要出网 HTTPS）"
    } else if lower.contains("429")
        || lower.contains("rate limit")
        || matches!(error, AdapterError::Limit { .. })
    {
        "被 QQ 开放平台限流（HTTP 429）：等一会儿再启动，别连续重试"
    } else if matches!(error, AdapterError::Blocked { .. }) {
        "请求被安全策略拒绝（端点不是 http(s) / 指到了内网）：检查 appId 对应的端点配置"
    } else if matches!(error, AdapterError::Unsupported { .. }) {
        "当前构建没有走 HTTPS 的能力（TLS 后端缺失）：这不是配置错误"
    } else {
        "取 access token 失败"
    };
    format!("{hint}；token 端点 {token_url}；原始错误：{raw}")
}

/// 探测失败 → wire 上的错误码。
///
/// 与 `onebot.rs` 的 `probe_error` 口径一致的地方是"**必须让 bot_start 失败**"，
/// 但分类更贴 `hbot_core::errors` 的既有表格：
///
/// * `Protocol` / `Limit` —— 凭据或请求参数不对，能改的是 `bots.json` → -32002 invalid；
/// * `Blocked` / `Unsupported` —— 策略层拒绝（"这么做不允许"）→ -32005 denied；
/// * `Timeout` —— 保留 -32004（探测自己的超时）；
/// * 其余（`Transport` / `Tls`）—— 链路问题，用户改不了程序也改不了 → -32006 internal。
///
/// 为什么这里不像 OneBot 那样把"网络不通"也说成 -32002：QQ 的探测只做一次 HTTPS
/// 取 token，`Transport` 的含义就是"这台机器现在出不了网"，把它报成"参数错误"会让用户
/// 去改一个没写错的 `clientSecret`。原因文本里两种情况都写清楚了，用户不至于无路可走。
fn probe_error(error: &AdapterError, reason: String) -> HbotError {
    match error {
        AdapterError::Protocol { .. } | AdapterError::Limit { .. } => HbotError::invalid(reason),
        AdapterError::Blocked { .. } | AdapterError::Unsupported { .. } => {
            HbotError::denied(reason)
        }
        AdapterError::Timeout {
            operation,
            timeout_ms,
        } => HbotError::timeout(format!("qq access token 探测（{operation}）"), *timeout_ms),
        AdapterError::Transport { .. } | AdapterError::Tls { .. } => HbotError::internal(reason),
    }
}

/// QQ 官方被动回复的两个端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassiveTarget {
    Group,
    C2c,
}

/// 选端点并调出去 —— **这就是"`reply_to` 有没有变成 `msg_id`"的测试缝**。
///
/// 单独成函数（而不是塞在 `QqAdapter::send` 里）：集成测试可以拿一个假
/// [`PassiveReply`] 直接验参数，不需要 `QqSender`（它 `pub(crate)` 且必然发 HTTPS）。
async fn dispatch_passive_reply(
    reply: &dyn PassiveReply,
    envelope: &OutboundEnvelope,
) -> Result<SendReceipt, HbotError> {
    let target = passive_target(envelope)?;
    let openid = envelope.conversation.external_id.trim();
    if openid.is_empty() {
        return Err(HbotError::invalid(
            "qq 会话地址为空：没有 group_openid / user_openid 就无法被动回复",
        ));
    }
    let msg_id = match envelope.reply_to.as_deref().map(str::trim) {
        Some(msg_id) if !msg_id.is_empty() => msg_id,
        _ => return Err(passive_required(envelope.conversation.kind)),
    };
    let sent = match target {
        PassiveTarget::Group => reply.send_group(openid, msg_id, &envelope.text).await,
        PassiveTarget::C2c => reply.send_c2c(openid, msg_id, &envelope.text).await,
    };
    sent.map_err(HbotError::from)
}

/// 会话类型 → 端点；QQ 官方只有群与单聊两个被动回复端点。
fn passive_target(envelope: &OutboundEnvelope) -> Result<PassiveTarget, HbotError> {
    match envelope.conversation.kind {
        ConversationKind::Group => Ok(PassiveTarget::Group),
        ConversationKind::Private => Ok(PassiveTarget::C2c),
        other => Err(HbotError::denied(format!(
            "qq 官方适配器没有向 {} 会话发送的接口（QQ 官方平台目前只有群聊 group 与单聊 \
             c2c 两个被动回复端点；频道消息走另一套端点，适配器侧没有验证过）",
            crate::app::runtime::conversation_kind_str(other)
        ))),
    }
}

/// `reply_to` 缺失时的错误。
///
/// 为什么是 **-32005 denied** 而不是 -32006 internal 或"伪装成功"：
///
/// * QQ 官方平台**只允许被动回复**，主动推送没有实现（`send_group_active` /
///   `send_c2c_active` 恒 `Unsupported`）。这是"平台不允许/没实现这个动作"，
///   正是 `AdapterError::Unsupported → HbotError::denied` 这条既有映射的语义；
/// * 用 `internal` 会把用户往"程序有 bug"的方向带，而实际上缺的是**触发消息 ID**：
///   `OutboundEnvelope.reply_to` 由 `sandbox_handler::passive_reply_id` 从入站
///   `external_id` 透传（QQ 侧 `external_id == d.id`），为 `None` 只可能是注入消息
///   （`payload.injected == true`）或没接上入站上下文；
/// * 绝不返回 `Ok`：那会把 outbound 行标成已发送，用户永远等不到消息，日志里也没有痕迹。
fn passive_required(kind: ConversationKind) -> HbotError {
    HbotError::denied(format!(
        "qq 官方机器人只允许被动回复：这条发往 {} 会话的回复没有 reply_to（被动回复必须带上\
         触发它的入站消息 ID，即事件的 d.id）；主动推送没有实现，这条消息没有发出去",
        crate::app::runtime::conversation_kind_str(kind)
    ))
}

/// 唯一的事件读者：独占 `QqClient` 的可变所有权，把归一化事件转成 [`PlatformEvent`]。
///
/// 通道满了就在这里 await —— 这是**有意的背压**（§4.1：暂停读取，不丢消息）。
async fn read_events(client: &mut QqClient, tx: &mpsc::Sender<PlatformEvent>) {
    while let Some(event) = client.next_event().await {
        let platform_event = platform_event_of(&event);
        if tx.send(platform_event).await.is_err() {
            // 订阅者消失（适配器被换掉 / supervisor 停了）：退出去收尾。
            return;
        }
    }
    tracing::info!("qq 官方事件流已结束（网关关闭）");
}

/// `NormalizedInbound` → `PlatformEvent`。
///
/// 关键字段：`external_id` 是 QQ 的 `d.id`（被动回复必须带的那条 ID），
/// `conversation.external_id` 是 `group_openid` / `user_openid`（**不是 QQ 号**）。
/// 两者都由适配器侧归一化好，这里只做搬运，不重新解释。
fn platform_event_of(event: &NormalizedInbound) -> PlatformEvent {
    let conversation = ConversationRef::new(event.conversation_external_id.clone(), event.kind);
    PlatformEvent::new(
        event.external_id.clone(),
        conversation,
        event.sender_id.clone(),
        event.payload_json(),
    )
    .with_received_at(event.received_at)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QQ 的平台标识是写进 `bots.json` 的字符串，改名等于改用户配置格式。
    #[test]
    fn platform_id_is_stable() {
        assert_eq!(PLATFORM, "qq_official");
    }

    /// 探测的超时地板值不许被调小：调小会把"慢但能连"判成配错。
    #[test]
    fn probe_timeout_floor_is_ten_seconds() {
        assert_eq!(PROBE_MIN_TIMEOUT, Duration::from_secs(10));
    }

    /// `Debug` 的输出里绝不能出现 `client_secret`：日志、错误上下文都会用到它。
    #[test]
    fn adapter_debug_redacts_client_secret() {
        let config = QqConfig::new("10001", "SECRET-for-debug-test");
        let adapter = QqAdapter::new(config).expect("valid config");
        let printed = format!("{adapter:?}");
        assert!(!printed.contains("SECRET-for-debug-test"), "{printed}");
        assert!(printed.contains("10001"), "{printed}");
    }

    /// 缺凭据时点名缺哪个字段（这是 -32002 的验收标准之一）。
    #[test]
    fn missing_credentials_name_the_field() {
        let mut config = BotConfig::new("ws://127.0.0.1:3001");
        config.platform = Some(PLATFORM.to_owned());
        let error = QqAdapter::from_bot_config(&config).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("appId"), "{error}");

        let mut config = BotConfig::new("ws://127.0.0.1:3001");
        config.platform = Some(PLATFORM.to_owned());
        config.app_id = Some("10001".to_owned());
        let error = QqAdapter::from_bot_config(&config).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("clientSecret"), "{error}");
    }

    /// 探测失败的错误码分类（与 `probe_error` 的注释一一对应）。
    #[test]
    fn probe_errors_map_to_the_documented_codes() {
        assert_eq!(
            probe_error(&AdapterError::protocol("bad secret"), String::new()).code(),
            -32002
        );
        assert_eq!(
            probe_error(&AdapterError::blocked("blocked"), String::new()).code(),
            -32005
        );
        assert_eq!(
            probe_error(&AdapterError::transport("no route"), String::new()).code(),
            -32006
        );
        assert_eq!(
            probe_error(
                &AdapterError::timeout("qq_token", Duration::from_secs(10)),
                String::new()
            )
            .code(),
            -32004
        );
    }
}
