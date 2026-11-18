//! Bot 智能能力（C5）在沙箱里的宿主侧装配：`ctx.ai.complete(options)`。
//!
//! 契约类型、错误码与**参数闸**都在 `hbot_core::bot_ai`（已冻结）；本模块只做
//! 三件事：
//!
//! 1. [`AiSlot`] —— `Sandbox` 上的可替换槽位（与 [`crate::state::StateSlot`]、
//!    [`crate::device::DeviceSlot`] 同一个写法：每次调用读当前值，所以"换掉模型"
//!    或"拔掉模型"对下一次调用立刻生效）；
//! 2. **每事件预算** —— 次数 [`MAX_CALLS_PER_EVENT`] 与时长 [`MAX_TIME_PER_EVENT`]，
//!    在 `build_event` 里逐事件清零；
//! 3. [`call`] —— 统一调用形状：`tokio::time::timeout` 包住端口的 future，并且
//!    **把等待时间从 handler 预算里抵掉**（见 [`call`] 的文档）。
//!
//! # 为什么必须有每事件预算
//!
//! handler 的预算是 2 秒（`crate::limits::DEFAULT_HANDLER_TIMEOUT`），而一次真实
//! 补全要 3–30 秒。等模型的时间如果不抵账，`ctx.ai.complete` 在 2 秒预算下**永远**
//! 超时 —— 这个能力等于不存在。所以等待时间从预算里扣掉（[`call`] 里那句
//! `deadline.arm(left)`）。但抵账不等于无限：一个 handler 不许在一轮里把用户的
//! 额度烧光，所以还有次数与总时长两条上限，超了报 `HBOT_AI_BUDGET`。
//!
//! # 为什么预算记账不退还
//!
//! 超时与失败同样消耗"这一轮花掉的时间"：超时的那一刻时间确实已经从 handler
//! 的墙钟里过去了。退还等于允许"反复超时直到永远"。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hbot_core::{AiClient, AiError, AiRequest, AiText, MAX_CALLS_PER_EVENT, MAX_TIME_PER_EVENT};

use crate::limits::Deadline;

/// 单次补全的默认上限。
///
/// 与 [`MAX_TIME_PER_EVENT`] 分开：单次上限挡"一个请求挂住"，每事件上限挡
/// "四次请求各挂 20 秒"。实际取两者与剩余事件预算的最小值。
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 宿主没装配模型客户端时的说明（`HBOT_AI_UNAVAILABLE` 前缀由 [`unavailable`] 加）。
pub const UNASSEMBLED: &str = "宿主没有可用的模型配置（请在 IDE 设置里配置 provider 与 key）";

/// `Sandbox` 上的智能槽。
///
/// `client` 与 `budget` 分开两把锁：预算在每个 `.complete()` 上都要动，而客户端
/// 只在装配时写。合成一把会让"读客户端"与"记一次调用"互相排队。
#[derive(Default)]
pub(crate) struct AiSlot {
    client: Mutex<Option<Arc<dyn AiClient>>>,
    budget: Mutex<AiBudget>,
}

/// 一次事件里已经用掉的额度。
#[derive(Clone, Copy, Debug, Default)]
struct AiBudget {
    calls: u32,
    waited: Duration,
}

impl AiSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 装入（或替换）模型客户端。
    pub(crate) fn set(&self, client: Arc<dyn AiClient>) {
        *self.client.lock().unwrap_or_else(PoisonError::into_inner) = Some(client);
    }

    /// 当前客户端。
    pub(crate) fn client(&self) -> Option<Arc<dyn AiClient>> {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn is_assembled(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// 清零一次事件的额度。
    ///
    /// 由 `build_event` 在**每次事件之前**调用（与 `device` 的会话身份、`state`
    /// 的 bot 身份同一时机）：isolate 跨事件复用，额度不随事件清零就等于按
    /// isolate 计，一个活着的 bot 只能补全四次。
    pub(crate) fn reset_event(&self) {
        *self.budget.lock().unwrap_or_else(PoisonError::into_inner) = AiBudget::default();
    }

    /// 占一次调用名额。
    fn take_call(&self) -> Result<(), AiError> {
        let mut budget = self.budget.lock().unwrap_or_else(PoisonError::into_inner);
        if budget.calls >= MAX_CALLS_PER_EVENT {
            return Err(AiError::Budget {
                message: format!(
                    "本事件最多 {MAX_CALLS_PER_EVENT} 次模型调用，已经用光；要更多就拆成多条消息或等 jobs（未实现）"
                ),
            });
        }
        budget.calls += 1;
        Ok(())
    }

    /// 记一次等待。
    fn charge(&self, waited: Duration) -> Result<(), AiError> {
        let mut budget = self.budget.lock().unwrap_or_else(PoisonError::into_inner);
        budget.waited = budget.waited.saturating_add(waited);
        if budget.waited > MAX_TIME_PER_EVENT {
            return Err(AiError::Budget {
                message: format!(
                    "本事件累计等模型 {} 毫秒，超过上限 {} 毫秒",
                    budget.waited.as_millis(),
                    MAX_TIME_PER_EVENT.as_millis()
                ),
            });
        }
        Ok(())
    }
}

impl std::fmt::Debug for AiSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AiSlot")
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 执行一次补全，带超时与时间抵账。
///
/// # 抵账为什么是 `deadline.arm(left)`
///
/// [`Deadline::arm`] 的语义是"从**现在**起给一段预算"（`arm` 内部把
/// `start.elapsed()` 当零点）。所以先记下调用前的剩余时间 `left`，等模型回来后
/// 再 `arm(left)`，等于把这次等待从 handler 的墙钟里抹掉、剩余预算与调用前一模
/// 一样。
///
/// 不做这一步的后果是确定的：2 秒 handler 里任何超过 2 秒的补全都会在 await
/// 返回后撞中断器，被报成 `CodeError::Timeout` —— 看起来像"模型太慢"，实际是
/// 预算没抵账。
///
/// `left` 为 `None`（deadline 未 armed）时不动它。
pub(crate) async fn call(
    slot: &AiSlot,
    deadline: &Arc<Deadline>,
    request: AiRequest,
) -> Result<AiText, String> {
    slot.take_call().map_err(|error| error.to_string())?;

    let Some(client) = slot.client() else {
        return Err(unavailable(UNASSEMBLED));
    };

    let left = deadline.remaining();
    let budget = match left {
        Some(left) => left.min(DEFAULT_CALL_TIMEOUT),
        None => DEFAULT_CALL_TIMEOUT,
    };
    // 剩余预算已经不是正数时不要等：那说明这一轮早就该结束了。
    if budget.is_zero() {
        return Err(AiError::Budget {
            message: String::from("本事件的预算已经用完，没有时间等模型"),
        }
        .to_string());
    }

    let started = Instant::now();
    let outcome = tokio::time::timeout(budget, client.complete(&request)).await;
    let waited = started.elapsed();

    // 先抵账再回报，两条路径都要走：超时/失败同样花掉了墙钟时间。
    if let Some(left) = left {
        deadline.arm(left);
    }

    // 等待时间一律记账；超了就是 `HBOT_AI_BUDGET`（比 `TIMEOUT` 更能说明是
    // "这一轮花得太多"而不是"这一个请求太慢"）。
    let charged = slot.charge(waited);

    match outcome {
        Ok(Ok(text)) => {
            charged.map_err(|error| error.to_string())?;
            Ok(text)
        }
        // 上游自己的错误优先原样交给 handler（`Failed` 的正文是排障的唯一线索，
        // 而且 `AiError::Display` 已经带码）。
        Ok(Err(error)) => Err(error.to_string()),
        Err(_elapsed) => Err(AiError::Timeout {
            ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        }
        .to_string()),
    }
}

/// 没装配时的错误 message。
#[must_use]
pub fn unavailable(detail: &str) -> String {
    AiError::Unavailable {
        reason: detail.to_owned(),
    }
    .to_string()
}

/// 本次事件已经发生过的调用次数（测试与诊断用）。
///
/// 生产路径不读它：额度判据只有 [`AiSlot::take_call`] 一处。
#[cfg(test)]
pub(crate) fn calls_used(slot: &AiSlot) -> u32 {
    slot.budget
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .calls
}

/// 计数用原子量：**只给槽位的 `Debug` 之外的场景**留一个可观测点。
///
/// 目前没有被生产代码使用，保留是因为 `s2_ai.rs` 需要在不读私有字段的前提下
/// 断言"后端一次都没被调"。放在这里而不是 `#[cfg(test)]`，集成测试（另一个
/// crate 边界）才能用。
#[derive(Clone, Default)]
pub struct CallCounter(Arc<AtomicU64>);

impl CallCounter {
    /// 记一次。
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// 已发生多少次。
    #[must_use]
    pub fn count(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::AiUsage;
    use std::pin::Pin;
    use std::sync::atomic::AtomicU32;

    struct Fixed {
        outcome: Result<AiText, AiError>,
        delay: Duration,
        calls: Arc<AtomicU32>,
    }

    impl AiClient for Fixed {
        fn complete<'a>(
            &'a self,
            _request: &'a AiRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<AiText, AiError>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.outcome.clone();
            let delay = self.delay;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                outcome
            })
        }
    }

    fn text(value: &str) -> AiText {
        AiText {
            text: value.to_owned(),
            usage: AiUsage::default(),
            model: String::from("fake"),
        }
    }

    /// 手搓一个只开时间盘的 runtime（本 crate 的 tokio 没有 `macros`）。
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("建一个只开时间盘的 runtime")
    }

    #[test]
    fn the_slot_starts_empty_and_reports_when_assembled() {
        let slot = AiSlot::new();
        assert!(!slot.is_assembled());
        assert!(slot.client().is_none());
        slot.set(Arc::new(Fixed {
            outcome: Ok(text("hi")),
            delay: Duration::ZERO,
            calls: Arc::new(AtomicU32::new(0)),
        }));
        assert!(slot.is_assembled());
    }

    #[test]
    fn an_unassembled_slot_reports_unavailable() {
        let slot = AiSlot::new();
        let deadline = Arc::new(Deadline::new());
        let outcome = runtime().block_on(call(&slot, &deadline, AiRequest::new("q")));
        let message = outcome.expect_err("应当报未装配");
        assert!(message.starts_with("HBOT_AI_UNAVAILABLE"), "{message}");
    }

    #[test]
    fn the_fifth_call_in_one_event_is_a_budget_error() {
        let slot = AiSlot::new();
        slot.set(Arc::new(Fixed {
            outcome: Ok(text("ok")),
            delay: Duration::ZERO,
            calls: Arc::new(AtomicU32::new(0)),
        }));
        let deadline = Arc::new(Deadline::new());
        let runtime = runtime();
        for attempt in 1..=MAX_CALLS_PER_EVENT {
            let outcome = runtime.block_on(call(&slot, &deadline, AiRequest::new("q")));
            assert!(outcome.is_ok(), "第 {attempt} 次应当成功");
        }
        let outcome = runtime.block_on(call(&slot, &deadline, AiRequest::new("q")));
        let message = outcome.expect_err("第五次应当超额度");
        assert!(message.starts_with("HBOT_AI_BUDGET"), "{message}");
        assert_eq!(calls_used(&slot), MAX_CALLS_PER_EVENT);
    }

    #[test]
    fn a_slow_call_times_out_and_is_not_refunded() {
        let slot = AiSlot::new();
        slot.set(Arc::new(Fixed {
            outcome: Ok(text("late")),
            delay: Duration::from_millis(300),
            calls: Arc::new(AtomicU32::new(0)),
        }));
        // 剩余预算 20ms < 300ms 的延迟 → 必然超时。
        let deadline = Arc::new(Deadline::new());
        deadline.arm(Duration::from_millis(20));
        let outcome = runtime().block_on(call(&slot, &deadline, AiRequest::new("q")));
        let message = outcome.expect_err("应当超时");
        assert!(message.starts_with("HBOT_AI_TIMEOUT"), "{message}");
    }

    #[test]
    fn a_successful_call_gives_the_event_budget_back() {
        // 抵账的正面证据：等待 60ms 之后，剩余预算与调用前近似相等。
        let slot = AiSlot::new();
        slot.set(Arc::new(Fixed {
            outcome: Ok(text("ok")),
            delay: Duration::from_millis(60),
            calls: Arc::new(AtomicU32::new(0)),
        }));
        let deadline = Arc::new(Deadline::new());
        deadline.arm(Duration::from_millis(500));
        let before = deadline.remaining().expect("已 armed");
        let runtime = runtime();
        runtime
            .block_on(call(&slot, &deadline, AiRequest::new("q")))
            .expect("应当成功");
        let after = deadline.remaining().expect("已 armed");
        // 容差：抵账只抹掉等待时间，调用本身的几微秒仍在预算里。
        assert!(
            after + Duration::from_millis(5) >= before,
            "抵账后剩余预算应当追平调用前：before={before:?} after={after:?}"
        );
    }

    #[test]
    fn the_event_budget_resets_between_events() {
        let slot = AiSlot::new();
        slot.set(Arc::new(Fixed {
            outcome: Ok(text("ok")),
            delay: Duration::ZERO,
            calls: Arc::new(AtomicU32::new(0)),
        }));
        let deadline = Arc::new(Deadline::new());
        let runtime = runtime();
        for _ in 0..MAX_CALLS_PER_EVENT {
            assert!(
                runtime
                    .block_on(call(&slot, &deadline, AiRequest::new("q")))
                    .is_ok()
            );
        }
        slot.reset_event();
        assert_eq!(calls_used(&slot), 0);
        assert!(
            runtime
                .block_on(call(&slot, &deadline, AiRequest::new("q")))
                .is_ok()
        );
    }

    #[test]
    fn an_upstream_error_keeps_its_own_code() {
        let slot = AiSlot::new();
        slot.set(Arc::new(Fixed {
            outcome: Err(AiError::Failed {
                message: String::from("401"),
            }),
            delay: Duration::ZERO,
            calls: Arc::new(AtomicU32::new(0)),
        }));
        let outcome =
            runtime().block_on(call(&slot, &Arc::new(Deadline::new()), AiRequest::new("q")));
        let message = outcome.expect_err("应当失败");
        assert!(message.starts_with("HBOT_AI_FAILED"), "{message}");
    }
}
