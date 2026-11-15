//! 时间与随机源的生产实现。
//!
//! `hbot-core` 只定义端口 trait（[`hbot_core::Clock`]），具体实现放在适配器层，
//! 由组合根注入；测试里换成固定时钟即可让时间确定（`AGENTS.md` §5
//! "时间、随机、UUID 通过 trait 注入"）。

use hbot_core::Clock;
use hbot_core::domain::Timestamp;

/// 系统时钟：`now()` 读 `SystemTime`。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_system_time(std::time::SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_monotonic_enough() {
        let clock = SystemClock;
        let first = clock.now();
        let second = clock.now();
        assert!(second.unix_millis() >= first.unix_millis());
        // 2020-01-01 之后的时间（sanity：不是 0 或负数）
        assert!(first.unix_millis() > 1_577_836_800_000);
    }
}
