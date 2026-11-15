//! 出站发送：**串行队列 + 有界重试 + 每次尝试留痕**。
//!
//! 与 `docs/02-architecture.md` §4 的出站链路对应：
//!
//! ```text
//! outbound 行（运行时/库负责）
//!   → OutboundSender::enqueue（有界队列，满则背压）
//!   → 单个 worker 串行取出（同会话顺序 = 入队顺序）
//!   → 第 1..=max_attempts 次尝试，失败按指数退避
//!   → SendReceipt { attempts, history } / 明确错误（不静默吞）
//! ```
//!
//! 为什么不在这里再开并发：`AGENTS.md` §4.1 要求"每会话串行"，同一个
//! `OutboundSender` 对应一个会话的发送序列；并发由上层按会话开多个 sender
//! 获得，而不是在同一个队列里乱序。
//!
//! 为什么发送要用 trait [`ActionSender`] 而不是直接拿 [`super::client::OneBotClient`]：
//! 重试与退避逻辑不该依赖 WebSocket，测试可以注入"先失败两次再成功"的假发送器，
//! 断言 `attempts == 3` 与退避步数，而不必启动真实连接。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use hbot_core::Clock;
use hbot_core::domain::Timestamp;

use super::backoff::{Backoff, Jitter, NoJitter};
use crate::error::AdapterError;

/// 盒装的 `Send` future：让 [`ActionSender`] 能作为 `dyn` 对象放进 `Arc`。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 发送一次平台动作（`send_group_msg` / `send_private_msg` …）。
///
/// 实现负责把动作交给连接层并在超时/断线时返回错误；**不做重试**，
/// 重试由 [`OutboundSender`] 统一控制（这样重试策略只有一份）。
pub trait ActionSender: Send + Sync {
    /// 发送动作并等待平台响应（成功时返回平台的 `data` 字段）。
    fn send_action<'a>(
        &'a self,
        action: String,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, AdapterError>>;
}

/// 发送重试策略。
#[derive(Clone)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff: Backoff,
    jitter: Arc<dyn Jitter>,
}

impl RetryPolicy {
    /// 构造策略；`max_attempts` 为 0 时按 1 处理（必须至少尝试一次）。
    pub fn new(max_attempts: u32, backoff: Backoff) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff,
            jitter: Arc::new(NoJitter),
        }
    }

    /// 默认策略：最多 3 次，间隔 500ms 起、8s 封顶（`docs/02-architecture.md` §12）。
    pub fn default_for_send() -> Self {
        Self::new(
            3,
            Backoff::new(
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(8),
            ),
        )
    }

    /// 换成自定义抖动源（测试用固定种子，生产可换成种子化实现）。
    pub fn with_jitter(mut self, jitter: Arc<dyn Jitter>) -> Self {
        self.jitter = jitter;
        self
    }

    /// 最大尝试次数（含首次）。
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// 退避参数。
    pub fn backoff(&self) -> &Backoff {
        &self.backoff
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_for_send()
    }
}

impl std::fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

/// 一次发送尝试的结果记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendAttempt {
    /// 第几次尝试（从 1 开始）。
    pub attempt: u32,
    /// 失败原因；成功为 `None`。
    pub error: Option<String>,
    /// 该次尝试结束时间。
    pub at: Timestamp,
}

/// 发送回执：平台接受后的结果 + 尝试痕迹。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    /// 平台返回的消息 ID（`data.message_id`），缺失为 `None`。
    pub message_id: Option<i64>,
    /// 总尝试次数（含成功那次）。
    pub attempts: u32,
    /// 逐次尝试记录，长度等于 `attempts`。
    pub history: Vec<SendAttempt>,
}

impl SendReceipt {
    /// 由平台动作响应构造；`message_id` 兼容数字与字符串两种方言。
    pub fn from_response(value: &Value, attempts: u32, history: Vec<SendAttempt>) -> Self {
        Self {
            message_id: value
                .get("message_id")
                .and_then(value_to_i64)
                .or_else(|| value_to_i64(value)),
            attempts,
            history,
        }
    }
}

/// 整数方言容错（与 `event.rs` 的规则一致）：数字、`"123"`、`" 123 "` 都认。
fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| text.trim().parse::<f64>().ok().map(|float| float as i64)),
        _ => None,
    }
}

/// 队列里的一个发送任务。
struct OutboundJob {
    action: String,
    params: Value,
    reply: oneshot::Sender<Result<SendReceipt, AdapterError>>,
}

/// 串行出站发送器（每个会话一个）。
///
/// `Clone` 只克隆队列句柄，**worker 仍然只有一个**，所以顺序不会乱。
#[derive(Clone)]
pub struct OutboundSender {
    tx: mpsc::Sender<OutboundJob>,
}

impl OutboundSender {
    /// 启动发送 worker。
    ///
    /// - `sender`：实际把动作交给平台的实现（生产用 [`super::client::ActionHandle`]）；
    /// - `policy`：重试策略；
    /// - `clock`：用于给每次尝试打时间戳（测试可注入固定时钟）；
    /// - `capacity`：有界队列容量，必须 > 0；满时 [`Self::enqueue`] 会等待（背压）。
    pub fn start(
        sender: Arc<dyn ActionSender>,
        policy: RetryPolicy,
        clock: Arc<dyn Clock>,
        capacity: usize,
    ) -> Result<Self, AdapterError> {
        if capacity == 0 {
            return Err(AdapterError::unsupported(
                "outbound queue capacity must be greater than 0",
            ));
        }
        let (tx, mut rx) = mpsc::channel::<OutboundJob>(capacity);
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let result = send_with_retry(
                    sender.as_ref(),
                    &policy,
                    clock.as_ref(),
                    job.action.as_str(),
                    &job.params,
                )
                .await;
                // 接收方可能已经走了（调用方超时放弃）：不是错误，忽略即可，
                // 但发送结果本身已经在上面记录过 tracing。
                let _ = job.reply.send(result);
            }
            tracing::debug!("outbound sender worker stopped");
        });
        Ok(Self { tx })
    }

    /// 入队并等待发送结果。
    ///
    /// 队列满时等待（背压，不丢消息）；worker 已停止时返回
    /// [`AdapterError::Transport`]，由调用方决定重试或进死信。
    pub async fn enqueue(
        &self,
        action: impl Into<String>,
        params: Value,
    ) -> Result<SendReceipt, AdapterError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(OutboundJob {
                action: action.into(),
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AdapterError::transport("outbound sender stopped"))?;
        reply_rx
            .await
            .map_err(|_| AdapterError::transport("outbound sender dropped the job"))?
    }
}

impl std::fmt::Debug for OutboundSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundSender").finish_non_exhaustive()
    }
}

/// 执行"最多 N 次、指数退避"的发送。
async fn send_with_retry(
    sender: &dyn ActionSender,
    policy: &RetryPolicy,
    clock: &dyn Clock,
    action: &str,
    params: &Value,
) -> Result<SendReceipt, AdapterError> {
    let max_attempts = policy.max_attempts();
    let mut history = Vec::with_capacity(max_attempts as usize);
    let mut last_error: Option<String> = None;
    for attempt in 1..=max_attempts {
        match sender.send_action(action.to_string(), params.clone()).await {
            Ok(value) => {
                history.push(SendAttempt {
                    attempt,
                    error: None,
                    at: clock.now(),
                });
                return Ok(SendReceipt::from_response(&value, attempt, history));
            }
            Err(err) => {
                // 每次失败都留结构化日志：不静默吞错（`AGENTS.md` §0.8）。
                tracing::warn!(
                    action,
                    attempt,
                    max_attempts,
                    error = %err,
                    "outbound attempt failed"
                );
                last_error = Some(err.to_string());
                history.push(SendAttempt {
                    attempt,
                    error: last_error.clone(),
                    at: clock.now(),
                });
                if attempt < max_attempts {
                    let delay = policy.backoff().delay(attempt, policy.jitter.as_ref());
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(AdapterError::transport(format!(
        "send `{action}` failed after {max_attempts} attempt(s): {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_reads_message_id_dialects() {
        let number = serde_json::json!({ "message_id": 123 });
        assert_eq!(
            SendReceipt::from_response(&number, 1, Vec::new()).message_id,
            Some(123)
        );
        let string = serde_json::json!({ "message_id": "456" });
        assert_eq!(
            SendReceipt::from_response(&string, 1, Vec::new()).message_id,
            Some(456)
        );
        let top_level = serde_json::json!(789);
        assert_eq!(
            SendReceipt::from_response(&top_level, 1, Vec::new()).message_id,
            Some(789)
        );
        let missing = serde_json::json!({ "ok": true });
        assert_eq!(
            SendReceipt::from_response(&missing, 1, Vec::new()).message_id,
            None
        );
    }

    #[test]
    fn policy_normalizes_invalid_attempts() {
        assert_eq!(
            RetryPolicy::new(0, Backoff::reconnect_default()).max_attempts(),
            1
        );
        assert_eq!(
            RetryPolicy::default_for_send().max_attempts(),
            3,
            "默认最多 3 次（含首次）"
        );
    }
}
