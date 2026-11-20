//! 出站发送：**先落库**、串行发送、指数退避重试、成功标记。
//!
//! 主链路末端（`AGENTS.md` §4.1）：`outbound row → sender retry → mark sent`。
//!
//! ## 为什么先落库
//!
//! `outbound` 行先写、再交给发送器。进程在“写库之后、发送之前”崩溃时，行仍是
//! `pending`；下次启动由注入的 [`PendingOutboxSource`] 把它交回 outbox 续发——这就是
//! “允许下次启动续发”的落点。反过来（先发再写）会丢消息。
//!
//! ## 重试
//!
//! 两级预算，都在 [`OutboxConfig`] 里可配：
//!
//! - [`OutboxConfig::max_attempts`]（默认 3）：**本次运行内**每条消息最多试几次；
//!   用完就停手、把行**保留 `Pending`**，留给下次启动续发（进程重启不是错误，
//!   平台临时抖动也不该判死）。
//! - [`OutboxConfig::max_send_attempts`]（默认 10）：**跨重启累计**的硬上限。累计
//!   尝试一旦达到它，就把行转**死信**（`state='failed'`，`mark_outbound_failed`）并写
//!   一条 `error` 级 RunLog 说明放弃原因与尝试次数。
//!
//! 两级是必要的：只有第一级时，一条永远发不出去的消息会在每次启动都空转一轮
//! （`attempts` 2→3→4… 单调增长，永不进死信），既占住 `pending` 队列头部，也让
//! “重试”失去上界。死信行**保留**（不删）：排障需要原始 `target`/`text`。
//!
//! ## 串行
//!
//! 单 worker 串行发送：平台侧对同一 bot 的发送顺序与 `outbound` 落库顺序一致，不需要
//! 依赖平台保证顺序。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hbot_core::{AttachmentRef, BotId, Clock, HbotError, IdGenerator, OutboundId, Timestamp};
use hbot_store::{NewOutbound, Store};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::{BotAdapter, OutboundEnvelope};
use crate::event::ConversationRef;
use crate::store_ops::run_store;

/// 出站配置。
#[derive(Clone, Debug)]
pub struct OutboxConfig {
    /// 每条消息在**本次运行内**最多尝试次数（含首次），默认 3。
    ///
    /// 用完就停手、行保留 `pending`，等下次启动续发（见 [`OutboxConfig::max_send_attempts`]）。
    pub max_attempts: u32,
    /// 每条消息**跨重启累计**的尝试硬上限，默认 10。
    ///
    /// 达到它就转死信（`state='failed'`）+ 写一条 `error` RunLog，不再无限空转。
    /// 必须 ≥ `max_attempts`（`sanitized()` 会抬到那个值），否则单次运行就会超限。
    pub max_send_attempts: u32,
    /// 首次重试的退避基数，默认 200ms。
    pub base_backoff: Duration,
    /// 退避上限，默认 30s。
    pub max_backoff: Duration,
    /// 待发通道容量（有界），默认 1024。
    pub capacity: usize,
    /// 单次 `send` 超时，默认 30s；超时按失败处理（不静默挂住）。
    pub send_timeout: Duration,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_send_attempts: 10,
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            capacity: 1024,
            send_timeout: Duration::from_secs(30),
        }
    }
}

impl OutboxConfig {
    fn sanitized(&self) -> Self {
        let max_attempts = self.max_attempts.max(1);
        Self {
            max_attempts,
            // 总预算不能小于单次运行预算：否则一次运行就会撞死信线，
            // “留给下次启动”这句话就没有落点了。
            max_send_attempts: self.max_send_attempts.max(max_attempts),
            capacity: self.capacity.max(1),
            ..self.clone()
        }
    }

    /// 第 `attempt` 次失败后的退避（attempt 从 1 开始）：`base * 2^(attempt-1)`，封顶。
    fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let factor = 1_u32 << shift;
        self.base_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// 一条待发送的落库请求（handler / RPC 侧构造）。
#[derive(Clone, Debug)]
pub struct OutboundRequest {
    /// 目标会话（平台外部 ID + 类型）。
    pub conversation: ConversationRef,
    /// 待发送文本。
    pub text: String,
    /// 触发这条回复的**平台消息 ID**（被动回复必须绑定，`AGENTS.md` §4.1）。
    ///
    /// `None` = 主动消息（定时任务、主动推送），没有可引用的历史事件，**不得**冒用旧 ID。
    /// `Some(id)` 时一路带到适配器：QQ/OneBot 用它组 `reply` 消息段，平台据此把回复
    /// 归属到原消息并计入被动回复配额（超配额会被拒发）。
    pub reply_to: Option<String>,
}

/// 已完成落库、等待发送的条目（跨重启恢复时由 [`PendingOutboxSource`] 提供）。
#[derive(Clone, Debug)]
pub struct PendingOutbound {
    /// `outbound.id`。
    pub id: OutboundId,
    /// 所属 bot。
    pub bot_id: BotId,
    /// 目标会话。
    pub conversation: ConversationRef,
    /// 待发送文本。
    pub text: String,
    /// 已尝试次数（来自库）。
    pub attempts: u32,
    /// 被动回复绑定的平台消息 ID（`outbound.reply_to`）。
    ///
    /// 重启恢复必须把它读回来：丢了这个字段，续发的消息就变成“主动消息”，
    /// 平台会拒发或在群里脱离上下文（`AGENTS.md` §4.1）。
    pub reply_to: Option<String>,
    /// 附件（来自 `outbound.attachment_json`）；`None` = 纯文本。
    ///
    /// 续发时必须把它读回来：丢了附件字段，重启前的「发这个文件」会退化成发一段空正文，
    /// 用户看到的是「文件没发出去但也没报错」。
    pub attachment: Option<AttachmentRef>,
}

/// 跨重启恢复未发 `outbound` 的注入点。
///
/// `hbot-store` 当前没有“按 bot 查 pending outbound”的读 API（只有写入与标记），所以
/// 这里用一个窄接口把“读”留给组合根/后续 store 扩展：supervisor 只负责把它们交回
/// outbox 续发。默认实现 [`NoPendingOutbox`] 返回空集（新装/测试环境）。
pub trait PendingOutboxSource: Send + Sync + 'static {
    /// 取出该 bot 仍未发送的 outbound 行。
    fn pending(&self, bot_id: &BotId) -> Result<Vec<PendingOutbound>, HbotError>;
}

/// 没有任何待发消息的实现。
pub struct NoPendingOutbox;

impl PendingOutboxSource for NoPendingOutbox {
    fn pending(&self, _bot_id: &BotId) -> Result<Vec<PendingOutbound>, HbotError> {
        Ok(Vec::new())
    }
}

/// outbox 计数器快照（UI/日志用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutboxStats {
    /// 发送成功条数。
    pub sent: u64,
    /// 重试次数（不含首次）。
    pub retried: u64,
    /// 放弃的条数：本次运行预算用完（保留 pending）**加上**转死信（`state='failed'`）的。
    ///
    /// 两种放弃都计数：对运维来说“这条这次没发出去”是同一件事，区别只在行状态
    /// （`pending` 会在下次启动再试，`failed` 不会）。
    pub failed: u64,
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    retried: AtomicU64,
    failed: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> OutboxStats {
        OutboxStats {
            sent: self.sent.load(Ordering::Acquire),
            retried: self.retried.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }
}

/// 出站发送器：一个 bot 一个，单 worker 串行。
pub struct Outbox {
    tx: mpsc::Sender<OutboundEnvelope>,
    stop: watch::Sender<bool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    counters: Arc<Counters>,
    config: OutboxConfig,
    bot_id: BotId,
    store: Arc<Store>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl Outbox {
    /// 启动发送 worker。
    pub fn spawn(
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        adapter: Arc<dyn BotAdapter>,
        bot_id: BotId,
        config: OutboxConfig,
    ) -> Self {
        let config = config.sanitized();
        let (tx, rx) = mpsc::channel::<OutboundEnvelope>(config.capacity);
        let (stop_tx, stop_rx) = watch::channel(false);
        let counters = Arc::new(Counters::default());
        let worker = tokio::spawn(run_worker(
            Arc::clone(&store),
            Arc::clone(&clock),
            adapter,
            config.clone(),
            Arc::clone(&counters),
            rx,
            stop_rx,
        ));
        Self {
            tx,
            stop: stop_tx,
            worker: Mutex::new(Some(worker)),
            counters,
            config,
            bot_id,
            store,
            ids,
            clock,
        }
    }

    /// 当前配置。
    pub fn config(&self) -> &OutboxConfig {
        &self.config
    }

    /// 计数器快照。
    pub fn stats(&self) -> OutboxStats {
        self.counters.snapshot()
    }

    /// 落库并入队一条出站消息（`pending`）。
    ///
    /// 先写 `outbound` 行再进通道：通道满时会等待（背压传给调用方），而消息已经是
    /// `pending` 行，等待期间崩溃也能在下次启动续发。
    pub async fn enqueue(&self, request: OutboundRequest) -> Result<OutboundId, HbotError> {
        self.enqueue_inner(request, None).await
    }

    /// 落库并入队一条**带附件**的出站消息。
    ///
    /// 与 [`Self::enqueue`] 的区别只有 `attachment`：为什么不做成给 `OutboundRequest` 加一个
    /// `Option` 字段 —— 那条路要求全仓十几处结构体字面量同批改（`hbot-rpc` 五处、`hbot-parity`
    /// 一处、各测试），而其中没有一处会因为多一个 `None` 变得更好读。文本与附件共用同一条
    /// 出站行、同一个重试与死信路径，差别只在适配器怎么翻译载荷。
    ///
    /// 附件的路径必须已经由宿主解析并核验（在项目根内、文件存在、大小不超限）。
    pub async fn enqueue_with_attachment(
        &self,
        request: OutboundRequest,
        attachment: AttachmentRef,
    ) -> Result<OutboundId, HbotError> {
        self.enqueue_inner(request, Some(attachment)).await
    }

    async fn enqueue_inner(
        &self,
        request: OutboundRequest,
        attachment: Option<AttachmentRef>,
    ) -> Result<OutboundId, HbotError> {
        let outbound_id = self.ids.outbound_id();
        let conversation_id =
            crate::conversation_id_for(&self.bot_id, &request.conversation.external_id);
        let row = NewOutbound {
            id: outbound_id,
            bot_id: self.bot_id,
            conversation_id,
            text: request.text.clone(),
            created_at: self.clock.now().unix_millis(),
        };
        let store = Arc::clone(&self.store);
        // 必须用 `enqueue_outbound_with_target`：`enqueue_outbound` 走的是
        // `OutboundTarget::unknown()`，`reply_to` 会永远落成 NULL（被动回复退化成主动消息）。
        let mut target = hbot_store::OutboundTarget::new("{}", request.reply_to.clone());
        if let Some(attachment) = &attachment {
            target = target.with_attachment(attachment.to_json());
        }
        run_store("enqueue_outbound", move || {
            store.enqueue_outbound_with_target(row, target)
        })
        .await?;
        let envelope = OutboundEnvelope {
            id: outbound_id,
            bot_id: self.bot_id,
            conversation_id,
            conversation: request.conversation,
            text: request.text,
            reply_to: request.reply_to,
            attachment,
            attempts: 0,
        };
        self.send_envelope(envelope).await?;
        Ok(outbound_id)
    }

    /// 恢复一批已落库的待发消息（启动顺序里的“恢复未发 outbox”）。
    pub async fn recover(&self, pending: Vec<PendingOutbound>) -> Result<usize, HbotError> {
        let mut restored = 0;
        for item in pending {
            let envelope = OutboundEnvelope {
                id: item.id,
                bot_id: item.bot_id,
                conversation_id: crate::conversation_id_for(
                    &item.bot_id,
                    &item.conversation.external_id,
                ),
                conversation: item.conversation,
                text: item.text,
                reply_to: item.reply_to,
                attachment: item.attachment,
                attempts: item.attempts,
            };
            self.send_envelope(envelope).await?;
            restored += 1;
        }
        Ok(restored)
    }

    async fn send_envelope(&self, envelope: OutboundEnvelope) -> Result<(), HbotError> {
        self.tx
            .send(envelope)
            .await
            .map_err(|_| HbotError::conflict("outbox worker stopped; outbound row remains pending"))
    }

    /// 停止发送：让 worker 完成手上这条后退出；未发出的行保持 `pending`。
    ///
    /// 返回计数器快照。超时仍未退出则 abort（行仍是 pending，下次启动续发）。
    pub async fn stop(&self, timeout: Duration) -> Result<OutboxStats, HbotError> {
        let _ = self.stop.send(true);
        let worker = {
            let mut guard = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
            guard.take()
        };
        if let Some(mut worker) = worker {
            let stopped = tokio::time::timeout(timeout, &mut worker).await;
            if stopped.is_err() {
                worker.abort();
                return Err(HbotError::timeout(
                    "flush outbox",
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                ));
            }
        }
        Ok(self.counters.snapshot())
    }
}

/// 单 worker 串行发送循环。
async fn run_worker(
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    adapter: Arc<dyn BotAdapter>,
    config: OutboxConfig,
    counters: Arc<Counters>,
    mut rx: mpsc::Receiver<OutboundEnvelope>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let envelope = tokio::select! {
            received = rx.recv() => match received {
                Some(envelope) => envelope,
                None => break,
            },
            _ = stop.changed() => break,
        };
        if *stop.borrow() {
            // 关停时不动这条：它还没开始发，保持 pending。
            break;
        }
        deliver(
            Arc::clone(&store),
            Arc::clone(&clock),
            Arc::clone(&adapter),
            &config,
            &counters,
            envelope,
            &mut stop,
        )
        .await;
    }
    tracing::debug!("outbox worker stopped");
}

#[allow(clippy::too_many_arguments)]
async fn deliver(
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    adapter: Arc<dyn BotAdapter>,
    config: &OutboxConfig,
    counters: &Counters,
    envelope: OutboundEnvelope,
    stop: &mut watch::Receiver<bool>,
) {
    let mut first_attempt = true;
    loop {
        let recorded = {
            let store = Arc::clone(&store);
            let id = envelope.id;
            run_store("mark_outbound_attempt", move || {
                store.mark_outbound_attempt(&id)
            })
            .await
        };
        // `attempts` 以库为准：跨重启时它可能已经大于 0，内存里的计数不可信。
        let attempts = match recorded {
            Ok(Some(attempts_in_db)) => attempts_in_db,
            Ok(None) => {
                tracing::warn!(
                    outbound = %envelope.id,
                    "outbound row missing; dropping queued send"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(outbound = %envelope.id, %error, "record outbound attempt failed");
                counters.failed.fetch_add(1, Ordering::AcqRel);
                return;
            }
        };
        if !first_attempt {
            counters.retried.fetch_add(1, Ordering::AcqRel);
        }
        first_attempt = false;
        // 这一轮已经试了几次：库里的 `attempts` 是**跨重启累计**的，减去入队/恢复时的
        // 起点才是本次运行的次数。不减去它，一条重启前已经试过 2 次的消息在重启后会
        // 立刻被判“本轮预算用完”，一次都不再试 —— “留给下次启动续发”就落空了。
        let run_attempts = attempts.saturating_sub(envelope.attempts);
        // 先看跨重启的硬上限，再看本次运行的预算：顺序反了的话，一条累计已超限的
        // 消息会先被“留给下次启动”放过去，于是每次启动都空转一轮。
        //
        // 判据是 `>`：`mark_outbound_attempt` 已经把**这一次**记进库里了，所以
        // `attempts == max_send_attempts` 表示“这次正好是最后一次允许的尝试”，要照发。
        if attempts > config.max_send_attempts {
            dead_letter(
                Arc::clone(&store),
                Arc::clone(&clock),
                counters,
                config,
                &envelope,
                attempts,
                None,
            )
            .await;
            return;
        }
        if run_attempts > config.max_attempts {
            counters.failed.fetch_add(1, Ordering::AcqRel);
            tracing::error!(
                outbound = %envelope.id,
                attempts,
                run_attempts,
                max_attempts = config.max_attempts,
                max_send_attempts = config.max_send_attempts,
                "outbound exceeded this run's retry budget; left pending for next start"
            );
            return;
        }

        let candidate = OutboundEnvelope {
            attempts,
            ..envelope.clone()
        };
        let send = tokio::time::timeout(config.send_timeout, adapter.send(candidate)).await;
        let outcome = match send {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(HbotError::timeout(
                format!("adapter send {}", adapter.id()),
                u64::try_from(config.send_timeout.as_millis()).unwrap_or(u64::MAX),
            )),
        };
        match outcome {
            Ok(()) => {
                let at: Timestamp = clock.now();
                let store = Arc::clone(&store);
                let id = envelope.id;
                match run_store("mark_outbound_sent", move || {
                    store.mark_outbound_sent(&id, at.unix_millis())
                })
                .await
                {
                    Ok(_) => {
                        counters.sent.fetch_add(1, Ordering::AcqRel);
                        tracing::debug!(outbound = %envelope.id, attempts, "outbound sent");
                    }
                    Err(error) => {
                        counters.failed.fetch_add(1, Ordering::AcqRel);
                        tracing::error!(
                            outbound = %envelope.id,
                            %error,
                            "sent but marking failed; row stays pending and may resend"
                        );
                    }
                }
                return;
            }
            Err(error) => {
                tracing::warn!(
                    outbound = %envelope.id,
                    attempts,
                    max_attempts = config.max_attempts,
                    %error,
                    "outbound send failed"
                );
                if attempts >= config.max_send_attempts {
                    dead_letter(
                        Arc::clone(&store),
                        Arc::clone(&clock),
                        counters,
                        config,
                        &envelope,
                        attempts,
                        Some(&error),
                    )
                    .await;
                    return;
                }
                if run_attempts >= config.max_attempts {
                    counters.failed.fetch_add(1, Ordering::AcqRel);
                    tracing::error!(
                        outbound = %envelope.id,
                        attempts,
                        run_attempts,
                        max_attempts = config.max_attempts,
                        max_send_attempts = config.max_send_attempts,
                        %error,
                        "outbound exhausted this run's retries; left pending for next start"
                    );
                    return;
                }
                let backoff = config.backoff(attempts);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.changed() => {
                        // 关停优先：不再重试，行保持 pending。
                        return;
                    }
                }
            }
        }
    }
}

/// 把一条消息转死信：`state='failed'` + 一条 `error` 级 RunLog。
///
/// 三件事必须一起做，缺一件都会让“放弃”变成静默：
///
/// - `mark_outbound_failed`：行离开 `pending`，`list_pending_outbound*`（`WHERE state='pending'`）
///   不再捡它——这是**跨重启生效**的唯一机制，计数留在库里而不是内存里；
/// - `append_log`：`logs_tail` / `run.log` 里能看见“为什么放弃、试了几次”；
/// - `counters.failed`：`BotSnapshot.outbox` 上能看见。
///
/// 行不删：排障需要原始 `target` / `text`（`models.rs` 的 `OUTBOUND_STATE_FAILED` 注释）。
#[allow(clippy::too_many_arguments)]
async fn dead_letter(
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    counters: &Counters,
    config: &OutboxConfig,
    envelope: &OutboundEnvelope,
    attempts: u32,
    error: Option<&HbotError>,
) {
    counters.failed.fetch_add(1, Ordering::AcqRel);
    let id = envelope.id;
    let marked = {
        let store = Arc::clone(&store);
        run_store("mark_outbound_failed", move || {
            store.mark_outbound_failed(&id, attempts)
        })
        .await
    };
    match &marked {
        Ok(true) => {}
        Ok(false) => tracing::warn!(outbound = %envelope.id, "dead letter: outbound row missing"),
        Err(failure) => tracing::error!(
            outbound = %envelope.id,
            %failure,
            "dead letter: marking failed did not persist; row stays pending"
        ),
    }

    let reason = error.map_or_else(
        || "累计尝试次数已达上限".to_owned(),
        |failure| failure.to_string(),
    );
    let line = format!(
        "出站消息放弃重试（尝试 {attempts} 次，上限 {}）：{reason}",
        config.max_send_attempts
    );
    tracing::error!(
        outbound = %envelope.id,
        attempts,
        max_send_attempts = config.max_send_attempts,
        %reason,
        "outbound dead-lettered; giving up"
    );

    // RunLog 要求 `logs.bot_id` 指向真实 bot 行（外键）：没有就只留 tracing，不要把
    // 一次日志写失败升级成 panic —— 死信本身已经落库了。
    let at = clock.now().unix_millis();
    let bot_id = envelope.bot_id;
    let entry = hbot_store::NewLog {
        bot_id,
        level: "error".to_owned(),
        line,
        at,
    };
    if let Err(failure) = run_store("append_log", move || store.append_log(entry)).await {
        tracing::error!(outbound = %envelope.id, %failure, "dead letter: run log not written");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{NoPendingOutbox, OutboxConfig, OutboxStats, PendingOutboxSource};

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let config = OutboxConfig {
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(500),
            ..OutboxConfig::default()
        };
        assert_eq!(config.backoff(1), Duration::from_millis(100));
        assert_eq!(config.backoff(2), Duration::from_millis(200));
        assert_eq!(config.backoff(3), Duration::from_millis(400));
        assert_eq!(config.backoff(4), Duration::from_millis(500), "capped");
        assert_eq!(
            config.backoff(40),
            Duration::from_millis(500),
            "no overflow"
        );
    }

    #[test]
    fn sanitized_config_never_zero() {
        let config = OutboxConfig {
            max_attempts: 0,
            capacity: 0,
            ..OutboxConfig::default()
        }
        .sanitized();
        assert_eq!(config.max_attempts, 1);
        assert_eq!(config.capacity, 1);
    }

    #[test]
    fn no_pending_outbox_is_empty() {
        let source = NoPendingOutbox;
        let bot = hbot_core::BotId::from_uuid(uuid::Uuid::from_u128(2));
        assert!(source.pending(&bot).expect("empty").is_empty());
    }

    #[test]
    fn default_stats_are_zero() {
        assert_eq!(
            OutboxStats::default(),
            OutboxStats {
                sent: 0,
                retried: 0,
                failed: 0
            }
        );
    }

    /// 两级预算：累计上限必须 ≥ 单轮上限，否则第一次运行就会撞死信线，
    /// “用完留给下次启动”的语义当场失效。
    #[test]
    fn total_budget_is_never_below_the_per_run_budget() {
        let config = OutboxConfig {
            max_attempts: 7,
            max_send_attempts: 2,
            ..OutboxConfig::default()
        }
        .sanitized();
        assert_eq!(config.max_attempts, 7);
        assert_eq!(config.max_send_attempts, 7, "被抬到单轮预算");

        // 正常配置原样保留。
        let config = OutboxConfig {
            max_attempts: 3,
            max_send_attempts: 12,
            ..OutboxConfig::default()
        }
        .sanitized();
        assert_eq!(config.max_send_attempts, 12);

        // 默认值就是文档写的 3 / 10。
        let defaults = OutboxConfig::default();
        assert_eq!(defaults.max_attempts, 3);
        assert_eq!(defaults.max_send_attempts, 10);
    }
}
