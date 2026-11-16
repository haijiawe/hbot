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
//! | `http.get(url, options?)` | `(string, object?) => Promise<{status, body, headers}>` | [`crate::http::HttpConfig`] |
//!
//! `sleep` and `http.get` are the important ones: they resolve (or reject) a
//! JavaScript promise from a Rust async task. The task is spawned onto the
//! isolate runtime (`Async` / `ctx.spawn`), so it is polled while the handler
//! awaits — it never blocks the event loop and never resolves the promise from a
//! foreign thread (§6.3: "async host returns via oneshot back to the same isolate
//! thread"). `http.get` adds one rule on top: its network work happens on
//! `spawn_blocking`, so a slow server cannot stall the isolate thread either.
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
use rquickjs::{Ctx, Exception, Function, IntoJs, Object, Value};

use crate::error::CodeError;
use crate::http::{GetOptions, HttpConfig, HttpResponse};
use crate::limits::Deadline;

/// The keys [`install`] writes on `globalThis.__hbot`, in one place.
///
/// This is data, not prose: `crate::dts`'s drift tests compare it with the
/// exports of [`HOST_MODULE_SOURCE`] and with the declarations in `hbot.d.ts`,
/// so a binding added here without its two declarations is a test failure.
pub(crate) const HOST_BINDING_KEYS: &[&str] = &["log", "reply", "sleep", "http"];

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

/// Install `globalThis.__hbot` (`log`, `reply`, `sleep`, `http.get`) into `ctx`.
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    output: Arc<Mutex<HostOutput>>,
    deadline: Arc<Deadline>,
    http: Arc<HttpConfig>,
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

    let http_object = {
        let config = Arc::clone(&http);
        let deadline = Arc::clone(&deadline);
        let get = Function::new(
            ctx.clone(),
            Async(move |url: String, options: Value<'js>| {
                // Parsing rides along with the future instead of returning a
                // rejected promise here: `Async` already turns an `Err` from
                // `IntoJs` into a rejection, and this keeps one error shape.
                let parsed = parse_get_options(&options);
                let config = Arc::clone(&config);
                let deadline = Arc::clone(&deadline);
                async move {
                    let outcome = match parsed {
                        Ok(options) => config.get(url, options, &deadline).await,
                        Err(message) => Err(message),
                    };
                    HttpOutcome(outcome)
                }
            }),
        )
        .and_then(|function| function.with_name("get"))
        .map_err(super::js_error)?;
        let http_object = Object::new(ctx.clone()).map_err(super::js_error)?;
        http_object.set("get", get).map_err(super::js_error)?;
        http_object
    };

    host.set("log", log).map_err(super::js_error)?;
    host.set("reply", reply).map_err(super::js_error)?;
    host.set("sleep", sleep).map_err(super::js_error)?;
    host.set("http", http_object).map_err(super::js_error)?;
    debug_assert_eq!(
        host.keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| HOST_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        HOST_BINDING_KEYS.len(),
        "every key in HOST_BINDING_KEYS must be installed: {HOST_BINDING_KEYS:?}"
    );
    ctx.globals().set("__hbot", host).map_err(super::js_error)?;
    Ok(())
}

/// The JS value `http.get` settles with: a plain object, or a rejection.
struct HttpOutcome(Result<HttpResponse, String>);

impl<'js> IntoJs<'js> for HttpOutcome {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(response) => {
                let object = Object::new(ctx.clone())?;
                object.set("status", i64::from(response.status))?;
                object.set("body", response.body.as_str())?;
                let headers = Object::new(ctx.clone())?;
                for (name, value) in &response.headers {
                    headers.set(name.as_str(), value.as_str())?;
                }
                object.set("headers", headers)?;
                Ok(object.into_value())
            }
            // A thrown exception is what `.catch()`/`try` sees in the handler;
            // the code prefix (`HBOT_HTTP_DENIED` / `HBOT_HTTP_FAILED`) is part
            // of the message because the message is all a bot gets to inspect.
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// Read `http.get(url, options)`'s second argument.
///
/// Only the two documented keys are read; `timeoutMs` is clamped later against
/// both [`crate::http::MAX_TIMEOUT`] and the handler's remaining budget.
fn parse_get_options<'js>(value: &Value<'js>) -> Result<GetOptions, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(GetOptions::default());
    }
    let Some(object) = value.as_object() else {
        return Err(format!(
            "{}: http.get(url, options): options must be an object",
            crate::permissions::FAILED_CODE
        ));
    };

    let mut options = GetOptions::default();
    if let Some(headers) = object
        .get::<_, Option<Object<'js>>>("headers")
        .map_err(js_message)?
    {
        for name in headers.keys::<String>() {
            let name = name.map_err(js_message)?;
            let value: Value<'js> = headers.get(name.as_str()).map_err(js_message)?;
            // No implicit conversion: `{ headers: { x: 1 } }` is a mistake worth
            // reporting, not a header called "1".
            if !value.is_string() {
                return Err(format!(
                    "{}: http.get(url, options): header `{name}` must be a string",
                    crate::permissions::FAILED_CODE
                ));
            }
            let text = value
                .as_string()
                .and_then(|string| string.to_string().ok())
                .unwrap_or_default();
            options.headers.push((name, text));
        }
    }

    if let Some(ms) = object
        .get::<_, Option<f64>>("timeoutMs")
        .map_err(js_message)?
    {
        if !ms.is_finite() || ms <= 0.0 {
            return Err(format!(
                "{}: http.get(url, options): timeoutMs must be a positive number",
                crate::permissions::FAILED_CODE
            ));
        }
        options.timeout = Some(Duration::from_millis(ms.min(MAX_SLEEP_MS) as u64));
    }

    Ok(options)
}

fn js_message(error: rquickjs::Error) -> String {
    format!("{}: {error}", crate::permissions::FAILED_CODE)
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

/**
 * HTTP, restricted to the hosts hbot.json's `permissions.http` allows.
 *
 * Rejects (never resolves) on a host outside the list, a non-http(s) URL, a
 * timeout or an oversized response; the message starts with `HBOT_HTTP_DENIED`
 * or `HBOT_HTTP_FAILED`. GET only, no redirects, no gzip.
 */
export const http = {
  get(url, options) {
    return host.http.get(url, options);
  },
};

export default { defineBot, log, reply, sleep, http };
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
