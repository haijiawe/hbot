//! 重连 / 重试退避计算（可注入抖动，测试不靠随机数）。
//!
//! 规则来自 `docs/02-architecture.md` §4.3：**1s、2s、4s…上限 30s，带 ±20% 抖动**。
//!
//! 抖动为什么必须可注入：测试要断言"第 3 次重连是 4s±20%"，如果每次调
//! `rand`，断言只能写成宽区间，回归时抓不到"退避被算成常数"这类 bug。
//! 因此这里定义 [`Jitter`] trait：
//!
//! - 生产用 [`SeededJitter`]（xorshift64*，进程内种子，无 `rand` 依赖）；
//! - 测试用 [`SeededJitter::new(固定种子)`] 或 [`NoJitter`]，结果完全确定。
//!
//! 时间单位一律 [`Duration`]；`attempt` 从 **1** 开始（第 1 次失败后的等待是 1s）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 默认重连起点：1s。
pub const DEFAULT_RECONNECT_BASE: Duration = Duration::from_secs(1);
/// 默认重连上限：30s。
pub const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 抖动系数来源：返回 `[0.8, 1.2]` 区间的乘数。
///
/// 实现必须是 `Send + Sync`（被 `Arc` 共享到连接任务）。
pub trait Jitter: Send + Sync {
    /// 本次抖动系数，必须在 `[0.8, 1.2]` 内；实现应自行保证（测试会检查）。
    fn factor(&self) -> f64;
}

/// 不抖动（系数恒为 1.0）；用于测试与确定性重放。
#[derive(Clone, Copy, Debug, Default)]
pub struct NoJitter;

impl Jitter for NoJitter {
    fn factor(&self) -> f64 {
        1.0
    }
}

/// 种子化抖动源：xorshift64* 产生 `[0.8, 1.2)` 的系数。
///
/// 不引 `rand`：退避抖动不需要密码学随机，xorshift 足够且零依赖。
/// 同一个种子给出同一串系数，测试可以断言确切值。
#[derive(Debug)]
pub struct SeededJitter {
    state: AtomicU64,
}

impl SeededJitter {
    /// 由种子构造；种子为 0 时换成固定非零值（xorshift 的零状态会卡死）。
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            }),
        }
    }

    /// 下一个 64 位状态（xorshift64*）。
    fn next_u64(&self) -> u64 {
        let mut state = self.state.load(Ordering::Relaxed);
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.state.store(state, Ordering::Relaxed);
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

impl Jitter for SeededJitter {
    fn factor(&self) -> f64 {
        // 取高 53 位映射到 [0,1)，再线性映射到 [0.8,1.2)。
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        0.8 + 0.4 * unit
    }
}

/// 指数退避：`base * 2^(attempt-1)`，上限 `max`，再乘抖动系数。
///
/// - `base` 为 0 时按 1ms 处理（避免忙等）；
/// - `max < base` 时按 `base` 处理；
/// - 抖动作用在**上限之前**的结果上，所以实际等待最长 `max * 1.2`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    max: Duration,
}

impl Backoff {
    /// 构造退避参数；非法输入按文档规则收敛，不 panic。
    pub fn new(base: Duration, max: Duration) -> Self {
        let base = if base.is_zero() {
            Duration::from_millis(1)
        } else {
            base
        };
        let max = if max < base { base } else { max };
        Self { base, max }
    }

    /// 连接重连的默认参数：1s 起、30s 封顶。
    pub fn reconnect_default() -> Self {
        Self::new(DEFAULT_RECONNECT_BASE, DEFAULT_RECONNECT_MAX)
    }

    /// 上限。
    pub fn max(&self) -> Duration {
        self.max
    }

    /// 起点。
    pub fn base(&self) -> Duration {
        self.base
    }

    /// 不含抖动的退避时长（已封顶）。
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(31);
        let base_ms = u128::from(u64::try_from(self.base.as_millis()).unwrap_or(u64::MAX));
        let scaled = base_ms.saturating_mul(1u128 << shift);
        let max_ms = u128::from(u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX));
        let capped = scaled.min(max_ms);
        Duration::from_millis(u64::try_from(capped).unwrap_or(u64::MAX))
    }

    /// 含抖动的退避时长。
    pub fn delay(&self, attempt: u32, jitter: &dyn Jitter) -> Duration {
        let base = self.base_delay(attempt);
        let factor = jitter.factor();
        // 抖动系数在 [0.8,1.2]；这里不假设实现守约，仍做下界保护。
        let factor = factor.clamp(0.8, 1.2);
        let millis = (base.as_secs_f64() * 1000.0 * factor).round();
        if !millis.is_finite() || millis <= 0.0 {
            return Duration::from_millis(1);
        }
        Duration::from_millis(u64::try_from(millis.round() as u128).unwrap_or(u64::MAX))
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::reconnect_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubling_and_cap() {
        let backoff = Backoff::reconnect_default();
        let expected_ms = [1000u64, 2000, 4000, 8000, 16000, 30000, 30000, 30000];
        for (index, expected) in expected_ms.iter().enumerate() {
            let attempt = u32::try_from(index + 1).expect("small");
            assert_eq!(
                backoff.base_delay(attempt).as_millis(),
                u128::from(*expected)
            );
        }
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let backoff = Backoff::reconnect_default();
        for seed in 1..=32u64 {
            let jitter = SeededJitter::new(seed);
            for attempt in 1..=8u32 {
                let base = backoff.base_delay(attempt).as_secs_f64();
                let delay = backoff.delay(attempt, &jitter).as_secs_f64();
                assert!(
                    delay >= base * 0.8 - 0.001 && delay <= base * 1.2 + 0.001,
                    "attempt {attempt}: {delay} vs {base}"
                );
            }
        }
    }

    #[test]
    fn same_seed_is_deterministic_and_no_jitter_is_exact() {
        let backoff = Backoff::reconnect_default();
        let first = SeededJitter::new(7);
        let second = SeededJitter::new(7);
        for attempt in 1..=5u32 {
            assert_eq!(
                backoff.delay(attempt, &first),
                backoff.delay(attempt, &second)
            );
            assert_eq!(
                backoff.delay(attempt, &NoJitter),
                backoff.base_delay(attempt)
            );
        }
    }

    #[test]
    fn invalid_inputs_are_clamped_not_panicking() {
        let backoff = Backoff::new(Duration::ZERO, Duration::from_secs(5));
        assert_eq!(backoff.base(), Duration::from_millis(1));
        assert_eq!(backoff.max(), Duration::from_secs(5));

        let inverted = Backoff::new(Duration::from_secs(10), Duration::from_secs(2));
        assert_eq!(inverted.max(), Duration::from_secs(10));
        assert_eq!(inverted.base_delay(3), Duration::from_secs(10));
    }

    #[test]
    fn huge_attempt_does_not_overflow() {
        let backoff = Backoff::reconnect_default();
        assert_eq!(backoff.base_delay(u32::MAX), Duration::from_secs(30));
    }
}
