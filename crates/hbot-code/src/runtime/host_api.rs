//! The `hbot.*` host surface exposed to a handler.
//!
//! Design of record: `docs/02-architecture.md` §6.3 — generated code gets *no*
//! `fs`/`net`/`process`/`require`/`eval`; it gets a small allowlist, all of it
//! backed by Rust. S1 implements the three calls needed to prove the bridge:
//!
//! | API | Signature | Backed by |
//! |---|---|---|
//! | `log(msg)` | `(unknown) => void` | [`HostOutput::logs`] |
//! | `reply(text)` | `(unknown) => void` | [`HostOutput::replies`] |
//! | `sleep(ms)` | `(number) => Promise<void>` | `tokio::time::sleep` |
//!
//! `sleep` is the important one: it resolves a JavaScript promise from a Rust
//! async task. The task is spawned onto the isolate runtime (`Async` /
//! `ctx.spawn`), so it is polled while the handler awaits — it never blocks the
//! event loop and never resolves the promise from a foreign thread
//! (§6.3: "async host returns via oneshot back to the same isolate thread").
//!
//! The functions are installed twice on purpose:
//!
//! * `globalThis.__hbot` — the raw bindings, used by the generated module below;
//! * the virtual ES module `hbot:host` — what user code is supposed to import
//!   (`import { defineBot } from 'hbot'`, §6.1).
//!
//! Keeping the module in JavaScript (not in Rust) means the entry contract is
//! visible in one short, reviewable source string, and `defineBot` validation
//! runs inside the isolate where the error is reported with a JS stack.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rquickjs::prelude::Async;
use rquickjs::{Ctx, Function, Object};

use crate::error::CodeError;
use crate::limits::Deadline;

/// Upper bound on captured `log`/`reply` entries per event.
///
/// A handler that floods the host must not be able to grow host memory without
/// bound; the cap is enforced here, not by trusting the handler.
pub(crate) const MAX_CAPTURED_ENTRIES: usize = 256;
/// Upper bound on one captured `log`/`reply` string, in bytes.
pub(crate) const MAX_CAPTURED_BYTES: usize = 8 * 1024;
/// Upper bound on a single `sleep` request, in milliseconds.
///
/// Guards against `sleep(1e18)`: the deadline clamp already shortens it, but
/// clamping the raw request keeps the arithmetic in range.
const MAX_SLEEP_MS: f64 = 10.0 * 60.0 * 1000.0;

/// Host-side accumulator for everything a handler emitted during one event.
#[derive(Clone, Debug, Default)]
pub struct HostOutput {
    logs: Vec<String>,
    replies: Vec<String>,
}

impl HostOutput {
    /// Messages passed to `log(...)`, in call order.
    pub fn logs(&self) -> &[String] {
        &self.logs
    }

    /// Messages passed to `reply(...)`, in call order.
    pub fn replies(&self) -> &[String] {
        &self.replies
    }

    /// Whether the handler emitted anything at all.
    pub fn is_empty(&self) -> bool {
        self.logs.is_empty() && self.replies.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.logs.clear();
        self.replies.clear();
    }

    pub(crate) fn push_log(&mut self, message: impl Into<String>) {
        push_capped(&mut self.logs, message.into());
    }

    pub(crate) fn push_reply(&mut self, message: impl Into<String>) {
        push_capped(&mut self.replies, message.into());
    }

    pub(crate) fn take(&mut self) -> (Vec<String>, Vec<String>) {
        (
            std::mem::take(&mut self.logs),
            std::mem::take(&mut self.replies),
        )
    }
}

/// Truncate to [`MAX_CAPTURED_BYTES`] on a char boundary and cap the entry count.
fn push_capped(target: &mut Vec<String>, message: String) {
    if target.len() >= MAX_CAPTURED_ENTRIES {
        return;
    }
    target.push(truncate(message));
}

fn truncate(message: String) -> String {
    if message.len() <= MAX_CAPTURED_BYTES {
        return message;
    }
    let mut end = MAX_CAPTURED_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = message[..end].to_owned();
    truncated.push('…');
    truncated
}

/// Install `globalThis.__hbot` (`log`, `reply`, `sleep`) into `ctx`.
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    output: Arc<Mutex<HostOutput>>,
    deadline: Arc<Deadline>,
) -> Result<(), CodeError> {
    let host = Object::new(ctx.clone()).map_err(super::js_error)?;

    let log = {
        let output = Arc::clone(&output);
        Function::new(ctx.clone(), move |message: String| {
            // A poisoned lock would mean another thread panicked while holding
            // it; recovering keeps the handler alive and the loss visible in the
            // captured output rather than as a silent no-op.
            output
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_log(message);
        })
        .and_then(|function| function.with_name("log"))
        .map_err(super::js_error)?
    };

    let reply = {
        let output = Arc::clone(&output);
        Function::new(ctx.clone(), move |message: String| {
            output
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_reply(message);
        })
        .and_then(|function| function.with_name("reply"))
        .map_err(super::js_error)?
    };

    let sleep = {
        let deadline = Arc::clone(&deadline);
        Function::new(
            ctx.clone(),
            Async(move |ms: f64| {
                let deadline = Arc::clone(&deadline);
                async move {
                    let requested = normalise_sleep_ms(ms);
                    // Clamp to the handler budget so `await sleep(60000)` in a
                    // 2 s handler wakes up at 2 s; the next JS instruction then
                    // trips the interrupt handler and the event reports a
                    // timeout instead of hanging the isolate.
                    let budget = deadline.remaining();
                    let effective = match budget {
                        Some(left) => requested.min(left),
                        None => requested,
                    };
                    tokio::time::sleep(effective).await;
                }
            }),
        )
        .and_then(|function| function.with_name("sleep"))
        .map_err(super::js_error)?
    };

    host.set("log", log).map_err(super::js_error)?;
    host.set("reply", reply).map_err(super::js_error)?;
    host.set("sleep", sleep).map_err(super::js_error)?;
    ctx.globals().set("__hbot", host).map_err(super::js_error)?;
    Ok(())
}

fn normalise_sleep_ms(ms: f64) -> Duration {
    if !ms.is_finite() || ms <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_millis(ms.min(MAX_SLEEP_MS) as u64)
}

/// Source of the virtual ES module `hbot:host`.
///
/// This is the *only* bare specifier a project may import
/// (`docs/02-architecture.md` §6.1); everything else answers `HBOT_BARE_IMPORT`.
/// It lives in JavaScript because it is part of the user-facing contract and is
/// expected to move to a generated `hbot.d.ts` in M1 (`crates/hbot-code/src/dts.rs`).
pub(crate) const HOST_MODULE_SOURCE: &str = r#"
const host = globalThis.__hbot;
if (!host) {
  throw new Error('HBOT_HOST_MISSING: host bindings were not installed');
}

export function defineBot(config) {
  if (config === null || typeof config !== 'object') {
    throw new TypeError('defineBot(config): config must be an object');
  }
  if (typeof config.onMessage !== 'function') {
    throw new TypeError('defineBot(config): config.onMessage must be a function');
  }
  return config;
}

export function log(message) {
  return host.log(message);
}

export function reply(text) {
  return host.reply(text);
}

export function sleep(ms) {
  return host.sleep(ms);
}

export default { defineBot, log, reply, sleep };
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_messages_are_kept_verbatim() {
        assert_eq!(truncate("hello".to_owned()), "hello");
    }

    #[test]
    fn long_messages_are_truncated_on_a_char_boundary() {
        let message = "é".repeat(MAX_CAPTURED_BYTES);
        let truncated = truncate(message);
        // `…` is 3 bytes, so the result stays within the cap plus the marker.
        assert!(truncated.len() <= MAX_CAPTURED_BYTES + 3);
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().all(|c| c == 'é' || c == '…'));
    }

    #[test]
    fn captured_entries_are_capped() {
        let mut output = HostOutput::default();
        for index in 0..(MAX_CAPTURED_ENTRIES + 10) {
            output.push_log(format!("line {index}"));
        }
        assert_eq!(output.logs().len(), MAX_CAPTURED_ENTRIES);
        assert_eq!(output.logs()[0], "line 0");
        assert!(output.replies().is_empty());
    }

    #[test]
    fn take_drains_both_buffers() {
        let mut output = HostOutput::default();
        output.push_log("a".to_owned());
        output.push_reply("b".to_owned());
        let (logs, replies) = output.take();
        assert_eq!(logs, vec!["a".to_owned()]);
        assert_eq!(replies, vec!["b".to_owned()]);
        assert!(output.is_empty());
    }

    #[test]
    fn sleep_requests_are_normalised() {
        assert_eq!(normalise_sleep_ms(f64::NAN), Duration::ZERO);
        assert_eq!(normalise_sleep_ms(-5.0), Duration::ZERO);
        assert_eq!(normalise_sleep_ms(0.0), Duration::ZERO);
        assert_eq!(normalise_sleep_ms(15.0), Duration::from_millis(15));
        assert_eq!(
            normalise_sleep_ms(MAX_SLEEP_MS * 10.0),
            Duration::from_millis(MAX_SLEEP_MS as u64)
        );
    }
}
