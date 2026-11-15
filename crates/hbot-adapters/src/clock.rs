//! 时间源的生产实现（**再导出**）。
//!
//! [`SystemClock`] 的实现已上移到 `hbot-core`：读系统时间不是平台适配（没有协议、
//! 没有网络），放在这里会逼着组合根为了一个时钟依赖整个 `hbot-adapters`，
//! 出现 `hbot-desktop -> hbot-adapters` 这种违反依赖方向的边
//! （`cargo run -p xtask -- deps` 会报错）。端口 trait 在 [`hbot_core::Clock`]，
//! 实现在 [`hbot_core::SystemClock`]。
//!
//! 这里保留 `pub use` 是为了历史调用点（`hbot_adapters::SystemClock` /
//! `hbot_adapters::clock::SystemClock`）一个都不用改。

pub use hbot_core::SystemClock;

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::Clock;

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
