//! The QuickJS side of the sandbox: one isolate, one worker thread, one host
//! surface.
//!
//! Module map:
//!
//! * [`isolate`] — thread + `LocalSet` + `AsyncContext`, the command loop, the
//!   ES module loader and per-event invocation;
//! * [`host_api`] — `log`/`reply`/`sleep` bindings and the virtual `hbot` module.
//!
//! Limits live in [`crate::limits`]; compile-time work in [`crate::compile`].

mod host_api;
mod isolate;

use std::time::Duration;

pub use crate::limits::Limits;
pub use host_api::HostOutput;
pub use isolate::Sandbox;

use crate::error::CodeError;

/// Heap numbers as QuickJS reports them.
///
/// All three are bytes. `limit_bytes` is [`Limits::memory_bytes`] echoed back, so
/// a report is self-contained for logs and the S1 measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryReport {
    /// Bytes currently used by the isolate (`JSMemoryUsage::memory_used_size`).
    pub used_bytes: i64,
    /// Bytes allocated from malloc for the isolate (`malloc_size`).
    pub heap_bytes: i64,
    /// Configured cap (`malloc_limit`).
    pub limit_bytes: i64,
}

/// What one host-side event dispatch produced.
#[derive(Clone, Debug)]
pub struct EventOutcome {
    /// Everything the handler passed to `log(...)`, in call order.
    pub logs: Vec<String>,
    /// Everything the handler passed to `reply(...)`, in call order.
    pub replies: Vec<String>,
    /// Wall-clock time spent inside the handler.
    pub duration: Duration,
    /// Heap after the handler returned.
    pub memory: MemoryReport,
}

impl EventOutcome {
    /// Whether the handler produced no observable output at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.logs.is_empty() && self.replies.is_empty()
    }
}

/// Wrap a bare `rquickjs` error in [`CodeError`] when no exception object is
/// available to describe it.
pub(crate) fn js_error(error: rquickjs::Error) -> CodeError {
    CodeError::JsException {
        message: error.to_string(),
        stack: None,
    }
}
