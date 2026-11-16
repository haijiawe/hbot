//! 启动分段计时（默认关闭、零开销）。
//!
//! 只在环境变量 `HBOT_STARTUP_TRACE=1` 时向 stderr 写一行一个事件，格式固定：
//!
//! ```text
//! hbot-startup ev=<event> dur_ms=<f64> unix_ms=<u64> [k=v ...]
//! ```
//!
//! `scripts/startup-probe.ps1` 按这个格式解析。刻意不走 `tracing`：`RUST_LOG`
//! 的过滤规则会让启动早期的事件丢失，而这里要的就是"一条都不能少"。
//!
//! 关闭时 [`Phase::start`] 只构造一个 `Option::None`，不读时钟、不分配、不输出。

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static ENABLED: OnceLock<bool> = OnceLock::new();

/// 是否开启启动计时：只有 `HBOT_STARTUP_TRACE=1` 才开启。
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("HBOT_STARTUP_TRACE").as_deref() == Ok("1"))
}

/// Unix 毫秒。用于把不同 crate 的事件按墙上时间对齐（精度到毫秒）。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// 打一个没有时长的时间点事件。
pub fn point(event: &str, detail: &str) {
    if !enabled() {
        return;
    }
    if detail.is_empty() {
        eprintln!("hbot-startup ev={event} unix_ms={}", unix_ms());
    } else {
        eprintln!("hbot-startup ev={event} unix_ms={} {detail}", unix_ms());
    }
}

/// 一个阶段计时器。关闭时是空操作。
pub struct Phase {
    event: &'static str,
    started: Option<Instant>,
}

impl Phase {
    /// 开始计时。
    pub fn start(event: &'static str) -> Self {
        Self {
            event,
            started: enabled().then(Instant::now),
        }
    }

    /// 结束并输出 `dur_ms`。
    pub fn done(self) {
        self.done_with("");
    }

    /// 结束并附带明细（形如 `role=writer` 的 `k=v` 片段）。
    pub fn done_with(self, detail: &str) {
        let Some(started) = self.started else {
            return;
        };
        let dur_ms = started.elapsed().as_secs_f64() * 1000.0;
        if detail.is_empty() {
            eprintln!(
                "hbot-startup ev={} dur_ms={dur_ms:.3} unix_ms={}",
                self.event,
                unix_ms()
            );
        } else {
            eprintln!(
                "hbot-startup ev={} dur_ms={dur_ms:.3} unix_ms={} {detail}",
                self.event,
                unix_ms()
            );
        }
    }
}
