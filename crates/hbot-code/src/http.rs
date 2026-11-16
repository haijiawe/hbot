//! The `http.get` host call: configuration, the transport seam, and the
//! request pipeline.
//!
//! `docs/02-architecture.md` §6.3 lists `http.get` as the one networked host
//! API. Three things are deliberately separated here:
//!
//! 1. **Authorization** — [`crate::permissions::authorize`] is the single
//!    judgement on `permissions.http`. Nothing in this module re-implements it.
//! 2. **The transport port** — [`HttpTransport`]. This crate's dependency
//!    edge is `hbot-core` only (`xtask/src/deps.rs`), and it has neither
//!    `tokio/net` nor a TLS stack, so it cannot open a socket itself. The
//!    client lives in `crates/hbot-adapters/src/http.rs` (hand-written HTTP/1.1
//!    plus an SSRF guard); whoever wires a real sandbox injects it here.
//!    Writing a second HTTP client in this crate would fork the SSRF
//!    judgement, which is exactly what the repository forbids.
//! 3. **The pipeline** — [`HttpConfig::get`] runs the checks in a fixed order
//!    (read-only, then scheme/host, then allow-list, then transport, then size)
//!    and turns every failure into a prefixed message ([`DENIED_CODE`] /
//!    [`FAILED_CODE`]).
//!
//! What this layer does *not* do, and must not pretend to: DNS resolution and
//! the private-address (SSRF) check. Those need the resolver, so they belong to
//! the transport — `hbot_adapters::http::is_blocked_ip` is the one implementation.
//! Tests inject a transport of their own (a loopback server), which is why no
//! `allow_private_for_tests` switch exists in production code.
//!
//! Deliberate limits, all of them visible to the handler as [`FAILED_CODE`]:
//!
//! * only `GET` — no request body, no POST/DELETE;
//! * no redirect following — a 3xx is a failure, not a hop the allow-list did
//!   not authorize (the transport decides, the adapters' client already refuses);
//! * no gzip/deflate — `body` is whatever the transport decoded;
//! * [`DEFAULT_MAX_BYTES`] on the response body, [`DEFAULT_TIMEOUT`] on the
//!   request, both clamped by the handler's own deadline.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use hbot_core::SandboxMode;

use crate::limits::Deadline;
use crate::manifest::Manifest;
use crate::permissions::{self, DENIED_CODE, FAILED_CODE};

/// Default per-request timeout. The handler deadline can shorten it; nothing can
/// lengthen it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard ceiling on one request, whatever `timeoutMs` the handler asks for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Default cap on the response body, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Most request headers one `http.get` may set.
pub(crate) const MAX_REQUEST_HEADERS: usize = 32;
/// Longest accepted header name, in bytes.
pub(crate) const MAX_HEADER_NAME_BYTES: usize = 256;
/// Longest accepted request header value, in bytes.
pub(crate) const MAX_HEADER_VALUE_BYTES: usize = 4 * 1024;
/// Most response headers passed back to the handler.
pub(crate) const MAX_RESPONSE_HEADERS: usize = 64;
/// Longest response header name/value passed back to the handler, in bytes.
pub(crate) const MAX_RESPONSE_HEADER_BYTES: usize = 8 * 1024;

/// One `GET`, as the transport receives it.
///
/// `timeout` and `max_bytes` are already clamped by [`HttpConfig::get`]; the
/// transport should still enforce both (it owns the socket).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The URL, exactly as the handler passed it.
    pub url: String,
    /// Extra request headers, validated (no CR/LF, capped length).
    pub headers: Vec<(String, String)>,
    /// How long the transport may take.
    pub timeout: Duration,
    /// Upper bound the transport should apply while reading the body.
    pub max_bytes: usize,
}

/// One `GET`'s response, as the handler sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body, decoded to UTF-8 by the transport (lossy is fine).
    pub body: String,
    /// Response headers, names lowercased, capped by the host layer.
    pub headers: Vec<(String, String)>,
}

/// Why a transport could not produce a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The transport refused the target itself: SSRF guard, a redirect it will
    /// not follow, an unsupported scheme. Reported as [`DENIED_CODE`].
    Blocked(String),
    /// The peer did not answer within the budget.
    Timeout,
    /// The body exceeded the transport's own cap.
    TooLarge,
    /// Connect/DNS/protocol failure.
    Failed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked(reason) => write!(formatter, "refused: {reason}"),
            Self::Timeout => formatter.write_str("timed out"),
            Self::TooLarge => formatter.write_str("response body too large"),
            Self::Failed(reason) => write!(formatter, "failed: {reason}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// The network seam of the isolate.
///
/// `hbot-code` deliberately has no HTTP client of its own: it depends on
/// `hbot-core` only (`xtask/src/deps.rs`), has no TLS crate, and the in-tree
/// client — `crates/hbot-adapters/src/http.rs`, with the SSRF guard, redirect and
/// gzip rules — must not be duplicated here. The transport is therefore injected
/// by whoever constructs the sandbox, from a crate that may depend on both.
///
/// `get` is **blocking**: [`HttpConfig::get`] runs it on `spawn_blocking`, so a
/// client may use `std::net` (or `block_on` a client future) without stalling the
/// isolate thread.
pub trait HttpTransport: Send + Sync {
    /// Perform one `GET`.
    ///
    /// # Errors
    ///
    /// Any [`TransportError`]; the host layer maps them onto
    /// [`DENIED_CODE`]/[`FAILED_CODE`] without interpreting the text.
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// A shared transport is a transport, so callers can keep their own `Arc`.
impl<T: HttpTransport + ?Sized> HttpTransport for Arc<T> {
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).get(request)
    }
}

/// Network configuration of one sandbox: the sandbox mode, the manifest's
/// `permissions.http` list, and the transport to run authorized requests on.
///
/// [`HttpConfig::deny_all`] is what [`crate::Sandbox::spawn`] uses: an empty
/// allow-list and no transport, so `http.get` answers [`DENIED_CODE`] and never
/// opens a socket.
#[derive(Clone)]
pub struct HttpConfig {
    mode: SandboxMode,
    allowed: Vec<String>,
    transport: Option<Arc<dyn HttpTransport>>,
    timeout: Duration,
    max_bytes: usize,
}

impl HttpConfig {
    /// Build a config from a sandbox mode and an already-parsed allow-list.
    pub fn new(mode: SandboxMode, allowed: Vec<String>) -> Self {
        Self {
            mode,
            allowed,
            transport: None,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Build a config from a `hbot.json` manifest, so `permissions.http` is
    /// actually consumed instead of being a decorative field.
    pub fn from_manifest(manifest: &Manifest, mode: SandboxMode) -> Self {
        Self::new(mode, manifest.permissions.http.clone())
    }

    /// No authorization and no transport: every `http.get` is denied.
    pub fn deny_all() -> Self {
        Self::new(SandboxMode::WorkspaceWrite, Vec::new())
    }

    /// Install the transport that performs authorized requests.
    ///
    /// Without it, an authorized request fails with [`FAILED_CODE`] ("no
    /// transport") rather than silently doing nothing.
    ///
    /// Takes the transport by value; a caller holding a shared one can pass the
    /// `Arc` itself, because `Arc<T: HttpTransport>` is a transport too.
    pub fn with_transport<T: HttpTransport + 'static>(mut self, transport: T) -> Self {
        self.transport = Some(Arc::new(transport));
        self
    }

    /// Install an already-shared transport (`Arc<dyn HttpTransport>`).
    pub fn with_shared_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Override the per-request timeout (clamped to `1 ms ..= `[`MAX_TIMEOUT`]).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = clamp_timeout(timeout);
        self
    }

    /// Override the response body cap (at least 1 byte).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// The sandbox mode this config was built for.
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// The manifest's `permissions.http`, verbatim.
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// The configured per-request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The configured response body cap.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Run one authorized `GET`.
    ///
    /// The check order is the contract (see the module docs): a read-only
    /// sandbox is refused **before** the allow-list is even consulted, so a
    /// read-only handler cannot probe which hosts exist in `permissions.http`,
    /// and no transport is reachable from that point.
    pub(crate) async fn get(
        &self,
        url: String,
        options: GetOptions,
        deadline: &Deadline,
    ) -> Result<HttpResponse, String> {
        // 1. Sandbox mode first: not a permission decision the manifest can
        //    influence, and it must cost no DNS round-trip.
        if self.mode == SandboxMode::ReadOnly {
            return Err(format!(
                "{DENIED_CODE}: the sandbox is read-only, network access is disabled regardless of permissions.http"
            ));
        }

        // 2. The single authorization judgement.
        let target = permissions::authorize_target(&self.allowed, &url)
            .map_err(|error| error.message().to_owned())?;
        let where_ = target.display();

        // 3. A malformed request is a failure, not a refusal: it was authorized.
        let headers = options.headers;
        validate_headers(&headers)
            .map_err(|detail| format!("{FAILED_CODE}: {where_}: {detail}"))?;

        // 4. No transport installed for this sandbox.
        let transport = self.transport.as_ref().ok_or_else(|| {
            format!("{FAILED_CODE}: {where_}: no HTTP transport is installed in this sandbox")
        })?;
        let transport = Arc::clone(transport);

        // 5. The budget is the smaller of the requested/default timeout and the
        //    handler's remaining time: a 2 s handler must not wait 10 s for a
        //    transport that only answers after its deadline.
        let mut budget = clamp_timeout(options.timeout.unwrap_or(self.timeout));
        if let Some(left) = deadline.remaining() {
            budget = budget.min(left);
        }
        if budget.is_zero() {
            budget = Duration::from_millis(1);
        }

        let request = HttpRequest {
            url: url.clone(),
            headers,
            timeout: budget,
            max_bytes: self.max_bytes,
        };

        // 6. Blocking transport off the isolate thread. A timeout here stops
        //    *waiting*; it cannot cancel the socket, so the transport must honour
        //    `request.timeout` itself.
        let outcome = tokio::time::timeout(
            budget,
            tokio::task::spawn_blocking(move || transport.get(&request)),
        )
        .await;
        let response = match outcome {
            Err(_elapsed) => {
                return Err(format!(
                    "{FAILED_CODE}: {where_} timed out after {} ms",
                    budget.as_millis()
                ));
            }
            Ok(Err(join_error)) => {
                tracing::error!(
                    host = %target.host,
                    error = %join_error,
                    "the http transport panicked"
                );
                return Err(format!("{FAILED_CODE}: {where_}: transport panicked"));
            }
            Ok(Ok(Err(error))) => return Err(map_transport_error(error, &where_)),
            Ok(Ok(Ok(response))) => response,
        };

        // 7. Size cap. The body itself must never enter the message: the handler
        //    gets the length, not the payload, and the rejection text is shown to
        //    the user and the model.
        if response.body.len() > self.max_bytes {
            return Err(format!(
                "{FAILED_CODE}: {where_} returned {} bytes, above the {}-byte limit",
                response.body.len(),
                self.max_bytes
            ));
        }

        Ok(HttpResponse {
            status: response.status,
            body: response.body,
            headers: sanitise_response_headers(response.headers),
        })
    }
}

impl fmt::Debug for HttpConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The transport is a trait object with no `Debug`; report its presence
        // and let the rest of the config stay readable in a log line.
        formatter
            .debug_struct("HttpConfig")
            .field("mode", &self.mode)
            .field("allowed", &self.allowed)
            .field("transport", &self.transport.is_some())
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

/// `http.get` options, already converted out of JavaScript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GetOptions {
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) timeout: Option<Duration>,
}

fn clamp_timeout(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT)
}

/// Refuse header names/values that could split the request line.
///
/// CR/LF in a header value is request smuggling (a second request on the same
/// connection), so it is rejected here even though the transport would also have
/// to be careful: this is the layer that knows the value came from the handler.
fn validate_headers(headers: &[(String, String)]) -> Result<(), String> {
    if headers.len() > MAX_REQUEST_HEADERS {
        return Err(format!(
            "at most {MAX_REQUEST_HEADERS} request headers are allowed"
        ));
    }
    for (name, value) in headers {
        if name.is_empty() {
            return Err("a request header has an empty name".to_owned());
        }
        if name.len() > MAX_HEADER_NAME_BYTES {
            return Err(format!(
                "a request header name is longer than {MAX_HEADER_NAME_BYTES} bytes"
            ));
        }
        if value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(format!(
                "the `{name}` request header is longer than {MAX_HEADER_VALUE_BYTES} bytes"
            ));
        }
        // Header names are tokens; a space or colon would let a caller smuggle a
        // line break or a second header through the name.
        if !is_header_token(name) {
            return Err(format!("`{name}` is not a valid request header name"));
        }
        if value
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
        {
            return Err(format!("the `{name}` request header contains CR/LF"));
        }
    }
    Ok(())
}

/// Whether `name` is an RFC 9110 header-field-name token.
///
/// The separators below are the ones a hostile caller would use to end the line
/// or start a new header; refusing them keeps a handler from smuggling a second
/// request over the transport's connection.
fn is_header_token(name: &str) -> bool {
    const SEPARATORS: &[u8] = b"()<>@,;:\\\"/[]?={} \t";
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !SEPARATORS.contains(&byte))
}

/// Lowercase response header names and cap their count/size, so a hostile server
/// cannot hand the isolate an unbounded `Record<string, string>`.
fn sanitise_response_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .take(MAX_RESPONSE_HEADERS)
        .map(|(name, value)| {
            let name = name.to_ascii_lowercase();
            let value = if value.len() > MAX_RESPONSE_HEADER_BYTES {
                let mut end = MAX_RESPONSE_HEADER_BYTES;
                while end > 0 && !value.is_char_boundary(end) {
                    end -= 1;
                }
                value[..end].to_owned()
            } else {
                value
            };
            (name, value)
        })
        .collect()
}

fn map_transport_error(error: TransportError, where_: &str) -> String {
    match error {
        // The transport's SSRF/redirect refusal is an authorization outcome even
        // though it happened below the allow-list.
        TransportError::Blocked(reason) => format!("{DENIED_CODE}: {where_}: {reason}"),
        TransportError::Timeout => format!("{FAILED_CODE}: {where_} timed out"),
        TransportError::TooLarge => format!(
            "{FAILED_CODE}: {where_}: the transport refused a response body above its limit"
        ),
        TransportError::Failed(reason) => format!("{FAILED_CODE}: {where_}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport that records how often it was asked and answers with a fixed
    /// body — enough to prove the ordering rules without a socket.
    #[derive(Default)]
    struct RecordingTransport {
        calls: AtomicUsize,
        response: Mutex<HttpResponse>,
    }

    impl RecordingTransport {
        fn with_body(body: &str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                response: Mutex::new(HttpResponse {
                    status: 200,
                    body: body.to_owned(),
                    headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
                }),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl HttpTransport for RecordingTransport {
        fn get(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.response.lock().expect("poisoned").clone())
        }
    }

    fn config(mode: SandboxMode, allowed: &[&str]) -> HttpConfig {
        HttpConfig::new(
            mode,
            allowed.iter().map(|entry| (*entry).to_owned()).collect(),
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn a_read_only_sandbox_is_denied_before_the_allow_list() {
        let transport = Arc::new(RecordingTransport::with_body("hello"));
        let config =
            config(SandboxMode::ReadOnly, &["example.com"]).with_transport(transport.clone());
        let deadline = Deadline::new();
        let started = std::time::Instant::now();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("read-only must refuse");
        assert!(error.starts_with(DENIED_CODE), "{error}");
        assert!(error.contains("read-only"), "{error}");
        assert_eq!(transport.calls(), 0, "no transport call may happen");
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn an_empty_allow_list_is_denied_without_a_transport_call() {
        let transport = Arc::new(RecordingTransport::with_body("hello"));
        let config = config(SandboxMode::WorkspaceWrite, &[]).with_transport(transport.clone());
        let deadline = Deadline::new();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/?token=SECRET".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("empty list must refuse");
        assert!(error.starts_with(DENIED_CODE), "{error}");
        assert!(!error.contains("SECRET"), "{error}");
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn an_authorized_request_returns_the_transport_response() {
        let transport = Arc::new(RecordingTransport::with_body("hello"));
        let config =
            config(SandboxMode::WorkspaceWrite, &["example.com"]).with_transport(transport.clone());
        let deadline = Deadline::new();
        let response = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect("authorized");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
        // Response header names are normalised for the JS object.
        assert_eq!(
            response.headers,
            vec![("content-type".to_owned(), "text/plain".to_owned())]
        );
        assert_eq!(transport.calls(), 1);
    }

    #[test]
    fn an_authorized_request_without_a_transport_fails_loudly() {
        let config = config(SandboxMode::WorkspaceWrite, &["example.com"]);
        let deadline = Deadline::new();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("no transport");
        assert!(error.starts_with(FAILED_CODE), "{error}");
        assert!(error.contains("no HTTP transport"), "{error}");
    }

    #[test]
    fn an_oversized_body_is_refused_and_never_echoed() {
        let transport = Arc::new(RecordingTransport::with_body(&"SECRETBODY".repeat(64)));
        let config = config(SandboxMode::WorkspaceWrite, &["example.com"])
            .with_transport(transport)
            .with_max_bytes(16);
        let deadline = Deadline::new();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("too large");
        assert!(error.starts_with(FAILED_CODE), "{error}");
        assert!(error.contains("above the 16-byte limit"), "{error}");
        assert!(!error.contains("SECRETBODY"), "{error}");
    }

    #[test]
    fn crlf_in_a_request_header_is_refused() {
        let transport = Arc::new(RecordingTransport::with_body("hello"));
        let config =
            config(SandboxMode::WorkspaceWrite, &["example.com"]).with_transport(transport.clone());
        let deadline = Deadline::new();
        let options = GetOptions {
            headers: vec![("x-evil".to_owned(), "a\r\nHost: attacker.test".to_owned())],
            timeout: None,
        };
        let error = runtime()
            .block_on(config.get("https://example.com/".to_owned(), options, &deadline))
            .expect_err("CRLF");
        assert!(error.starts_with(FAILED_CODE), "{error}");
        assert!(error.contains("CR/LF"), "{error}");
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn a_transport_refusal_is_denied() {
        struct Blocked;
        impl HttpTransport for Blocked {
            fn get(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
                Err(TransportError::Blocked("private address".to_owned()))
            }
        }
        let config =
            config(SandboxMode::WorkspaceWrite, &["example.com"]).with_transport(Arc::new(Blocked));
        let deadline = Deadline::new();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("blocked");
        assert!(error.starts_with(DENIED_CODE), "{error}");
        assert!(error.contains("private address"), "{error}");
    }

    #[test]
    fn a_transport_timeout_is_failed_after_the_budget() {
        struct Slow;
        impl HttpTransport for Slow {
            fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
                std::thread::sleep(request.timeout + Duration::from_millis(20));
                Err(TransportError::Timeout)
            }
        }
        let config = config(SandboxMode::WorkspaceWrite, &["example.com"])
            .with_transport(Arc::new(Slow))
            .with_timeout(Duration::from_millis(20));
        let deadline = Deadline::new();
        let started = std::time::Instant::now();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("slow");
        assert!(error.starts_with(FAILED_CODE), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_timeout_is_clamped() {
        assert_eq!(clamp_timeout(Duration::ZERO), Duration::from_millis(1));
        assert_eq!(clamp_timeout(Duration::from_secs(3600)), MAX_TIMEOUT);
        assert_eq!(
            clamp_timeout(Duration::from_millis(500)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn response_headers_are_lowercased_and_capped() {
        let many: Vec<(String, String)> = (0..(MAX_RESPONSE_HEADERS + 10))
            .map(|index| (format!("X-H{index:02}"), "v".to_owned()))
            .collect();
        let sanitised = sanitise_response_headers(many);
        assert_eq!(sanitised.len(), MAX_RESPONSE_HEADERS);
        assert_eq!(sanitised[0].0, "x-h00");

        let long = vec![("X-Big".to_owned(), "é".repeat(MAX_RESPONSE_HEADER_BYTES))];
        let sanitised = sanitise_response_headers(long);
        assert!(sanitised[0].1.len() <= MAX_RESPONSE_HEADER_BYTES + 2);
    }

    #[test]
    fn from_manifest_reads_permissions_http() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"name":"t","platform":{"kind":"onebot11","ws":"ws://127.0.0.1:3001"},"permissions":{"http":["api.example.com"]}}"#,
        )
        .expect("manifest parses");
        let config = HttpConfig::from_manifest(&manifest, SandboxMode::WorkspaceWrite);
        assert_eq!(config.allowed().len(), 1);
        assert_eq!(config.allowed()[0], "api.example.com");
        assert_eq!(config.mode(), SandboxMode::WorkspaceWrite);
        assert_eq!(config.max_bytes(), DEFAULT_MAX_BYTES);
        assert_eq!(config.timeout(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn deny_all_grants_nothing() {
        let config = HttpConfig::deny_all();
        assert!(config.allowed().is_empty());
        let deadline = Deadline::new();
        let error = runtime()
            .block_on(config.get(
                "https://example.com/".to_owned(),
                GetOptions::default(),
                &deadline,
            ))
            .expect_err("denied");
        assert!(error.starts_with(DENIED_CODE), "{error}");
    }
}
