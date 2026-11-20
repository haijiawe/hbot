//! Limits for one QuickJS isolate: heap cap, stack cap, handler deadline.
//!
//! Every limit here is enforced *inside* the isolate so a misbehaving handler
//! cannot take the process down (see `docs/02-architecture.md` §6.3 and
//! `docs/adr/0002-agent-in-rust.md` §10.1):
//!
//! * memory — [`Limits::memory_bytes`] is handed to `Runtime::set_memory_limit`,
//!   which only works with QuickJS' built-in allocator (no `rust-alloc`);
//! * stack — [`Limits::stack_bytes`] goes to `Runtime::set_max_stack_size`;
//! * time — [`Limits::handler_timeout`] is armed by [`Deadline`] and polled from
//!   a `set_interrupt_handler` callback, which turns a runaway handler into an
//!   *uncatchable* exception (the script cannot swallow it).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default heap cap for one isolate: 32 MiB.
pub const DEFAULT_MEMORY_BYTES: usize = 32 * 1024 * 1024;
/// Default QuickJS stack cap for one isolate: 512 KiB.
pub const DEFAULT_STACK_BYTES: usize = 512 * 1024;
/// Default wall-clock budget for one `onMessage` invocation.
pub const DEFAULT_HANDLER_TIMEOUT: Duration = Duration::from_secs(2);
/// Default wall-clock budget for one whole `dispatch` round trip.
///
/// Kept above [`DEFAULT_HANDLER_TIMEOUT`] so the isolate reports the precise
/// timeout first; this is only the backstop for a wedged worker thread.
pub const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Resource limits applied when an isolate is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// QuickJS heap cap in bytes.
    pub memory_bytes: usize,
    /// QuickJS stack cap in bytes.
    pub stack_bytes: usize,
    /// Wall-clock budget for one handler invocation.
    pub handler_timeout: Duration,
    /// 后台作业（`onJob`）单次预算的**宿主上限**，毫秒；`0` = 不限。
    ///
    /// 与 `handler_timeout` 分开：handler 是「一条消息最多跑多久」（秒级、由宿主硬掐），
    /// 作业是「一个后台任务最多跑多久」（分钟级、由声明给出），把两者塞进同一个字段会让
    /// 「作业跑得久」和「handler 卡住」变成同一件事，排障时分不清。
    ///
    /// 缺省是核心契约的上限（10 分钟），即「宿主不额外收紧」——**默认行为与没有这个字段
    /// 时完全一致**。手机端这台宿主会把它设成几十秒（`docs/08-multi-host.md`）。
    pub job_budget_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_MEMORY_BYTES,
            stack_bytes: DEFAULT_STACK_BYTES,
            handler_timeout: DEFAULT_HANDLER_TIMEOUT,
            job_budget_ms: hbot_core::MAX_JOB_BUDGET_MS,
        }
    }
}

impl Limits {
    /// The production defaults (32 MiB heap, 512 KiB stack, 2 s handler).
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the heap cap.
    pub fn with_memory_bytes(mut self, bytes: usize) -> Self {
        self.memory_bytes = bytes;
        self
    }

    /// Override the stack cap.
    pub fn with_stack_bytes(mut self, bytes: usize) -> Self {
        self.stack_bytes = bytes;
        self
    }

    /// Override the per-handler wall-clock budget.
    pub fn with_handler_timeout(mut self, timeout: Duration) -> Self {
        self.handler_timeout = timeout;
        self
    }

    /// The handler budget in whole milliseconds (`u64` for error reporting).
    pub fn handler_timeout_ms(&self) -> u64 {
        u64::try_from(self.handler_timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// Override the host ceiling for one background job (milliseconds; `0` = no ceiling).
    pub fn with_job_budget_ms(mut self, millis: u64) -> Self {
        self.job_budget_ms = millis;
        self
    }

    /// The host ceiling for one background job, in whole milliseconds.
    ///
    /// 夹回核心契约的区间：写 0 是「不限」，写 1 是「1 毫秒」这种没有意义的配置，
    /// 一律按 `MIN_JOB_BUDGET_MS` 处理，避免装配方各自解释。
    pub fn job_budget_ms(&self) -> u64 {
        if self.job_budget_ms == 0 {
            return hbot_core::MAX_JOB_BUDGET_MS;
        }
        self.job_budget_ms
            .clamp(hbot_core::MIN_JOB_BUDGET_MS, hbot_core::MAX_JOB_BUDGET_MS)
    }
}

/// Sentinel stored in [`Deadline::deadline_ns`] while no budget is armed.
const DISARMED: u64 = u64::MAX;

/// Lock-free deadline polled from QuickJS' interrupt handler.
///
/// QuickJS calls the interrupt handler from arbitrary bytecode positions, so the
/// check must be cheap and must never block or allocate. It is also called on
/// the isolate thread only, so no `Send`/`Sync` gymnastics are needed beyond the
/// atomics themselves.
pub(crate) struct Deadline {
    start: Instant,
    /// Offset from `start`, in nanoseconds; [`DISARMED`] means "no budget".
    deadline_ns: AtomicU64,
    interrupted: AtomicBool,
}

impl Deadline {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            deadline_ns: AtomicU64::new(DISARMED),
            interrupted: AtomicBool::new(false),
        }
    }

    /// Arm a budget measured from *now* (not from construction).
    pub(crate) fn arm(&self, budget: Duration) {
        let now = Self::ns(self.start.elapsed());
        let deadline = now.saturating_add(Self::ns(budget));
        // `DISARMED` stays reserved for "not armed", so saturate one below it.
        self.deadline_ns
            .store(deadline.min(DISARMED - 1), Ordering::SeqCst);
    }

    /// Disarm the budget (called after the handler returned).
    pub(crate) fn disarm(&self) {
        self.deadline_ns.store(DISARMED, Ordering::SeqCst);
    }

    /// Whether an armed budget has elapsed.
    pub(crate) fn expired(&self) -> bool {
        let deadline = self.deadline_ns.load(Ordering::SeqCst);
        deadline != DISARMED && Self::ns(self.start.elapsed()) > deadline
    }

    /// Time left before the armed budget expires, or `None` while disarmed.
    ///
    /// Host calls that suspend the isolate (for example `sleep`) use this so a
    /// handler cannot park the isolate past its budget: the awaited call is
    /// clamped to the remaining time, and the next JS instruction then hits the
    /// interrupt handler.
    pub(crate) fn remaining(&self) -> Option<Duration> {
        let deadline = self.deadline_ns.load(Ordering::SeqCst);
        if deadline == DISARMED {
            return None;
        }
        let now = Self::ns(self.start.elapsed());
        Some(Duration::from_nanos(deadline.saturating_sub(now)))
    }

    /// Interrupt-handler body: returns `true` to abort the running script.
    pub(crate) fn check(&self) -> bool {
        if self.expired() {
            self.interrupted.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Forget any previous interruption (start of a fresh invocation).
    pub(crate) fn reset(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    /// Read and clear the "we had to interrupt" flag.
    ///
    /// Reading it here — rather than trusting the JavaScript error message — is
    /// what makes [`crate::CodeError::Timeout`] distinguishable from a script
    /// that just threw a string containing the word "timeout".
    pub(crate) fn take_interrupted(&self) -> bool {
        self.interrupted.swap(false, Ordering::SeqCst)
    }

    fn ns(duration: Duration) -> u64 {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_budget() {
        let limits = Limits::default();
        assert_eq!(limits.memory_bytes, 32 * 1024 * 1024);
        assert_eq!(limits.stack_bytes, 512 * 1024);
        assert_eq!(limits.handler_timeout, Duration::from_secs(2));
        assert_eq!(limits.handler_timeout_ms(), 2_000);
    }

    #[test]
    fn builders_override_single_fields() {
        let limits = Limits::new()
            .with_memory_bytes(1024)
            .with_stack_bytes(2048)
            .with_handler_timeout(Duration::from_millis(25));
        assert_eq!(limits.memory_bytes, 1024);
        assert_eq!(limits.stack_bytes, 2048);
        assert_eq!(limits.handler_timeout_ms(), 25);
    }

    #[test]
    fn disarmed_deadline_never_expires() {
        let deadline = Deadline::new();
        std::thread::sleep(Duration::from_millis(5));
        assert!(!deadline.expired());
        assert!(!deadline.check());
        assert!(!deadline.take_interrupted());
    }

    #[test]
    fn armed_deadline_expires_and_remembers_the_interrupt() {
        let deadline = Deadline::new();
        deadline.arm(Duration::from_millis(10));
        assert!(!deadline.check());
        std::thread::sleep(Duration::from_millis(25));
        assert!(deadline.check());
        assert!(deadline.take_interrupted());
        // `take_interrupted` clears the flag; the deadline itself stays expired.
        assert!(!deadline.take_interrupted());
        assert!(deadline.expired());
        deadline.disarm();
        assert!(!deadline.expired());
    }

    #[test]
    fn reset_clears_a_previous_interrupt() {
        let deadline = Deadline::new();
        deadline.arm(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(deadline.check());
        deadline.disarm();
        deadline.reset();
        assert!(!deadline.take_interrupted());
    }
}
