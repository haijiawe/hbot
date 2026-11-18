//! What a *failed* dispatch still produces: the host output already captured.
//!
//! A handler that writes `ctx.log("…")` and then throws has already produced
//! real diagnostic output by the time the failure surfaces. Keeping it here —
//! and not on [`CodeError`] — is deliberate: the logs belong to **one dispatch**,
//! not to the class of failure. The same `HBOT_JS_EXCEPTION` from two events
//! carries two different log sets, and a `CodeError` is copied, matched and
//! logged in places that know nothing about a handler's stdout.

use crate::error::CodeError;

/// A dispatch that failed after the handler had already written host output.
///
/// [`Sandbox::dispatch`](crate::Sandbox::dispatch) keeps its historical
/// `Result<_, CodeError>` shape and drops these logs (every existing caller
/// treats the event as failed either way);
/// [`Sandbox::dispatch_collecting`](crate::Sandbox::dispatch_collecting) and
/// [`Sandbox::dispatch_with_debug_collecting`](crate::Sandbox::dispatch_with_debug_collecting)
/// are the entry points that keep them, so the host layer can still persist
/// `ctx.log` lines for an event that errored.
#[derive(Debug)]
pub struct EventFailure {
    /// Everything `ctx.log()` captured, in the order the handler wrote it.
    ///
    /// Empty only when no handler ever ran: the payload did not serialise, the
    /// isolate was already gone, or its inbox stayed full until the backstop.
    pub logs: Vec<String>,
    /// Why the event failed, already classified the same way
    /// [`Sandbox::dispatch`](crate::Sandbox::dispatch) reports it.
    ///
    /// Boxed because it rides in a `Result::Err`: `CodeError` is ~104 bytes and
    /// `Result<EventOutcome, EventFailure>` trips `clippy::result_large_err`
    /// above 128 bytes. Field access is unchanged (`failure.error.code()`).
    pub error: Box<CodeError>,
}

impl EventFailure {
    /// A failure raised before the handler could produce any output.
    ///
    /// Public because hosts need it too: `hbot-rpc`'s timer tick can fail before any
    /// isolate runs (a wedged slot, a dispatch task that panicked), and it reports
    /// those on the same `Result<EventOutcome, EventFailure>` path as a handler
    /// failure so one match handles both.
    pub fn without_output(error: CodeError) -> Self {
        Self {
            logs: Vec::new(),
            error: Box::new(error),
        }
    }
}
