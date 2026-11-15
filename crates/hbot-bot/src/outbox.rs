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
//! 每条消息最多 [`OutboxConfig::max_attempts`] 次（默认 3）；每次尝试前用
//! `mark_outbound_attempt` 把 `attempts` 落库，失败按 `base * 2^(n-1)` 退避（上限
//! [`OutboxConfig::max_backoff`]）。超过上限**保留 `Pending`** 并计数上报（不静默丢、
//! 也不假装成功）；下一次启动仍可续发。
//!
//! ## 串行
//!
//! 单 worker 串行发送：平台侧对同一 bot 的发送顺序与 `outbound` 落库顺序一致，不需要
//! 依赖平台保证顺序。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hbot_core::{BotId, Clock, HbotError, IdGenerator, OutboundId, Timestamp};
use hbot_store::{NewOutbound, Store};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::{BotAdapter, OutboundEnvelope};
use crate::event::ConversationRef;
use crate::store_ops::run_store;

/// 出站配置。
#[derive(Clone, Debug)]
pub struct OutboxConfig {
    /// 每条消息最多尝试次数（含首次），默认 3。
    pub max_attempts: u32,
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
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            capacity: 1024,
            send_timeout: Duration::from_secs(30),
        }
    }
}

impl OutboxConfig {
    fn sanitized(&self) -> Self {
        Self {
            max_attempts: self.max_attempts.max(1),
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
    /// 超过重试上限、保留 pending 的条数。
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
        run_store("enqueue_outbound", move || store.enqueue_outbound(row)).await?;
        let envelope = OutboundEnvelope {
            id: outbound_id,
            bot_id: self.bot_id,
            conversation_id,
            conversation: request.conversation,
            text: request.text,
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
        if attempts > config.max_attempts {
            counters.failed.fetch_add(1, Ordering::AcqRel);
            tracing::error!(
                outbound = %envelope.id,
                attempts,
                max_attempts = config.max_attempts,
                "outbound exceeded retry budget; left pending for next start"
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
                if attempts >= config.max_attempts {
                    counters.failed.fetch_add(1, Ordering::AcqRel);
                    tracing::error!(
                        outbound = %envelope.id,
                        attempts,
                        "outbound exhausted retries; left pending for next start"
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
}
