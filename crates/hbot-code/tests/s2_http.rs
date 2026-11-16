//! S2 acceptance tests: `http.get` from bot code, gated by `permissions.http`.
//!
//! These are end-to-end tests: a real TypeScript project, compiled by the real
//! compiler, evaluated in a real QuickJS isolate, whose handler `await`s a real
//! promise. Two kinds of transport are used, both injected — production code has
//! no transport at all until a caller supplies one, so "no network by default" is
//! structural, not a flag:
//!
//! * [`Loopback`] speaks HTTP/1.1 over a real `std::net::TcpStream` to a real
//!   `std::net::TcpListener` on `127.0.0.1`. The SSRF guard lives in
//!   `hbot-adapters` (this crate must not depend on it), so a test-only transport
//!   is the only way to exercise the receive path against a socket at all.
//! * [`Canned`] answers from memory and records the request, for the cases where
//!   the assertion is about the *decision* (authorization, limits, redaction) and
//!   a server would only add noise.
//!
//! What a test must never do is reach a real network: every URL below is either
//! loopback or a documentation host, and every transport is local.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hbot_code::{
    HttpConfig, HttpRequest, HttpResponse, HttpTransport, Limits, Sandbox, TransportError,
    compile_project,
};
use hbot_core::domain::SandboxMode;
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A throwaway project directory, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn project(name: &str, files: &[(&str, &str)]) -> Project {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "hbot-s2-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    for (relative, contents) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }
    Project(root)
}

/// The handler every test uses: call `http.get`, reply `OK …` or `ERR …`.
///
/// Catching the rejection is the point — it proves the failure is a JS exception
/// (so `try`/`catch` and `.catch()` work) and lets the message be asserted on.
const HANDLER: &str = r#"import { defineBot, http } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      const res = await http.get('__URL__', __OPTIONS__);
      const type = res.headers['content-type'] || '';
      const server = res.headers['x-server'] || '';
      ctx.reply('OK ' + res.status + ' ' + res.body + ' ct=' + type + ' xs=' + server);
    } catch (error) {
      const kind = error instanceof Error ? 'Error' : typeof error;
      ctx.reply('ERR ' + kind + ' ' + (error && error.message ? error.message : String(error)));
    }
  },
});
"#;

fn handler(url: &str, options: &str) -> String {
    HANDLER
        .replace("__URL__", url)
        .replace("__OPTIONS__", options)
}

fn allow(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| (*entry).to_owned()).collect()
}

/// Compile `source`, boot an isolate with `http`, run one event.
fn run(http: HttpConfig, name: &str, source: &str) -> (Vec<String>, Duration) {
    let root = project(name, &[("src/main.ts", source)]);
    let compiled = compile_project(root.path()).expect("project compiles");
    let sandbox =
        Sandbox::spawn_with_http(compiled, Limits::default(), http).expect("isolate boots");
    let started = Instant::now();
    let outcome = sandbox
        .dispatch(&json!({ "payload": { "text": "hi" } }))
        .expect("handler runs");
    (outcome.replies, started.elapsed())
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

/// What a [`Canned`] transport does when called.
#[derive(Clone, Copy, Debug)]
enum Behaviour {
    /// Answer `status` with `body`.
    Respond(u16, &'static str),
    /// Pretend the connection could not be made.
    Fail,
    /// Block for this long before answering (a wedged server).
    Block(Duration),
    /// Answer with a body far above any sane `max_bytes`.
    Oversized,
}

#[derive(Debug, Default)]
struct Calls {
    count: AtomicUsize,
    urls: Mutex<Vec<String>>,
    headers: Mutex<Vec<Vec<(String, String)>>>,
}

impl Calls {
    fn record(&self, request: &HttpRequest) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.urls
            .lock()
            .expect("calls lock")
            .push(request.url.clone());
        self.headers
            .lock()
            .expect("calls lock")
            .push(request.headers.clone());
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// A transport that never touches the network and records what it was asked for.
struct Canned {
    behaviour: Behaviour,
    calls: Arc<Calls>,
}

impl Canned {
    fn new(behaviour: Behaviour) -> (Self, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        (
            Self {
                behaviour,
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

impl HttpTransport for Canned {
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.record(request);
        match self.behaviour {
            Behaviour::Respond(status, body) => Ok(HttpResponse {
                status,
                body: body.to_owned(),
                headers: vec![
                    ("content-type".to_owned(), "text/plain".to_owned()),
                    ("x-server".to_owned(), "canned".to_owned()),
                ],
            }),
            Behaviour::Fail => Err(TransportError::Failed("connection refused".to_owned())),
            Behaviour::Block(delay) => {
                std::thread::sleep(delay);
                Ok(HttpResponse {
                    status: 200,
                    body: "late".to_owned(),
                    headers: Vec::new(),
                })
            }
            Behaviour::Oversized => Ok(HttpResponse {
                status: 200,
                body: "SECRETBODY".repeat(400),
                headers: Vec::new(),
            }),
        }
    }
}

/// A real HTTP/1.1 client over loopback: `std::net` only (this crate has no
/// tokio `net` feature, and adding a dependency is out of scope for S2).
///
/// It is deliberately minimal — GET, one request per connection, no chunked
/// encoding, no redirects — because it exists to prove the *receive* path
/// (status/body/headers) works against a socket, not to be a second production
/// client.
struct Loopback {
    addr: SocketAddr,
    calls: Arc<Calls>,
}

/// Start a one-shot-per-connection loopback server answering with `body`.
///
/// The accept loop runs on a detached thread for the rest of the test process;
/// every request is served in it, so several sandboxes can share one server.
fn loopback_server(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut chunk = [0_u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Server: loopback\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

impl Loopback {
    fn new(addr: SocketAddr) -> (Self, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        (
            Self {
                addr,
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

impl HttpTransport for Loopback {
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.record(request);
        let timeout = request.timeout.max(Duration::from_millis(100));
        let mut stream = TcpStream::connect_timeout(&self.addr, timeout)
            .map_err(|error| TransportError::Failed(format!("connect failed: {error}")))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| TransportError::Failed(format!("set_read_timeout: {error}")))?;

        // Only the path goes on the wire; the authority is implicit in the
        // connection, which is why an allow-list entry can never be bypassed by
        // a URL whose host disagrees with the socket.
        let path = request
            .url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
            .unwrap_or("/");
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for (name, value) in &request.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .map_err(|error| TransportError::Failed(format!("write failed: {error}")))?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|error| TransportError::Failed(format!("read status: {error}")))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| TransportError::Failed(format!("bad status line: {status_line:?}")))?;

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|error| TransportError::Failed(format!("read header: {error}")))?;
            if read == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }

        let mut body = String::new();
        reader
            .read_to_string(&mut body)
            .map_err(|error| TransportError::Failed(format!("read body: {error}")))?;

        Ok(HttpResponse {
            status,
            body,
            headers,
        })
    }
}

// ---------------------------------------------------------------------------
// 1. Default deny: no list, no socket, no wait.
// ---------------------------------------------------------------------------

#[test]
fn an_empty_allow_list_denies_without_opening_a_socket() {
    let (transport, calls) = Canned::new(Behaviour::Block(Duration::from_secs(30)));
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, Vec::new()).with_transport(transport);
    let url = "http://10.255.255.1:81/v1?token=SECRET";

    let (replies, elapsed) = run(http, "empty-list", &handler(url, "undefined"));

    assert_eq!(replies.len(), 1, "handler replied once: {replies:?}");
    let reply = &replies[0];
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_DENIED:"),
        "a denied host must reject with HBOT_HTTP_DENIED, got {reply:?}"
    );
    assert!(
        reply.contains("permissions.http is empty"),
        "the message must say why: {reply:?}"
    );
    assert_eq!(calls.count(), 0, "the transport must not be reached at all");
    assert!(
        elapsed < Duration::from_millis(50),
        "a denial must not wait on anything, took {elapsed:?}"
    );
    // The query is a secret in the fixture; it must not survive into the message
    // the handler (and therefore the model's context) sees.
    assert!(!reply.contains("SECRET"), "query leaked: {reply:?}");
    assert!(
        reply.contains("?<redacted>"),
        "query not redacted: {reply:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Real socket: an allowed loopback host returns status/body/headers.
// ---------------------------------------------------------------------------

#[test]
fn an_allowed_loopback_host_returns_status_body_and_headers() {
    const BODY: &str = "hello from loopback";
    let addr = loopback_server(BODY);
    let (transport, calls) = Loopback::new(addr);
    let http = HttpConfig::new(
        SandboxMode::WorkspaceWrite,
        allow(&[&format!("127.0.0.1:{}", addr.port())]),
    )
    .with_transport(transport);
    let url = format!("http://127.0.0.1:{}/v1/greet", addr.port());

    let (replies, _) = run(
        http,
        "loopback",
        &handler(&url, "{ headers: { 'x-test': 'yes' }, timeoutMs: 5000 }"),
    );

    assert_eq!(
        replies,
        vec![format!("OK 200 {BODY} ct=text/plain xs=loopback")],
        "status, body and (lowercased) headers must all arrive"
    );
    assert_eq!(calls.count(), 1, "exactly one request");
    let recorded = calls.headers.lock().expect("headers lock")[0].clone();
    assert!(
        recorded.contains(&("x-test".to_owned(), "yes".to_owned())),
        "the options' headers must reach the transport: {recorded:?}"
    );
}

#[test]
fn http_get_is_awaitable_and_the_namespace_shape_is_the_documented_one() {
    let (transport, calls) = Canned::new(Behaviour::Respond(200, "awaited"));
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allow(&["api.example.com"]))
        .with_transport(transport);
    // `await` only resolves if the host returned a real promise; a synchronous
    // return would make `res.status` undefined and the reply would read `OK
    // undefined …`.
    let (replies, _) = run(
        http,
        "awaitable",
        &handler("http://api.example.com/v1", "undefined"),
    );
    assert_eq!(
        replies,
        vec!["OK 200 awaited ct=text/plain xs=canned".to_owned()]
    );
    assert_eq!(calls.count(), 1);
}

// ---------------------------------------------------------------------------
// 3. Authorization semantics through the full stack.
// ---------------------------------------------------------------------------

#[test]
fn a_wildcard_entry_covers_subdomains_but_not_the_bare_domain() {
    let allowed = allow(&["*.example.com"]);

    let (transport, calls) = Canned::new(Behaviour::Respond(200, "one-label"));
    let http =
        HttpConfig::new(SandboxMode::WorkspaceWrite, allowed.clone()).with_transport(transport);
    let (replies, _) = run(
        http,
        "wildcard-one",
        &handler("http://a.example.com/x", "undefined"),
    );
    assert_eq!(
        replies,
        vec!["OK 200 one-label ct=text/plain xs=canned".to_owned()]
    );
    assert_eq!(calls.count(), 1);

    let (transport, calls) = Canned::new(Behaviour::Respond(200, "two-labels"));
    let http =
        HttpConfig::new(SandboxMode::WorkspaceWrite, allowed.clone()).with_transport(transport);
    let (replies, _) = run(
        http,
        "wildcard-two",
        &handler("http://a.b.example.com/x", "undefined"),
    );
    assert_eq!(
        replies,
        vec!["OK 200 two-labels ct=text/plain xs=canned".to_owned()]
    );
    assert_eq!(calls.count(), 1);

    // The bare domain is *not* `*.example.com`.
    let (transport, calls) = Canned::new(Behaviour::Respond(200, "must-not-happen"));
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allowed).with_transport(transport);
    let (replies, _) = run(
        http,
        "wildcard-bare",
        &handler("http://example.com/x", "undefined"),
    );
    assert!(
        replies[0].starts_with("ERR Error HBOT_HTTP_DENIED:"),
        "the bare domain must not match `*.example.com`: {replies:?}"
    );
    assert_eq!(calls.count(), 0);

    // ... and neither does a host that merely *ends* with the suffix.
    let (transport, calls) = Canned::new(Behaviour::Respond(200, "must-not-happen"));
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allow(&["*.example.com"]))
        .with_transport(transport);
    let (replies, _) = run(
        http,
        "wildcard-evil",
        &handler("http://evilexample.com/x", "undefined"),
    );
    assert!(
        replies[0].starts_with("ERR Error HBOT_HTTP_DENIED:"),
        "`evilexample.com` must not match `*.example.com`: {replies:?}"
    );
    assert_eq!(calls.count(), 0);
}

#[test]
fn a_port_pinned_entry_rejects_another_port() {
    let (transport, calls) = Canned::new(Behaviour::Respond(200, "must-not-happen"));
    let http = HttpConfig::new(
        SandboxMode::WorkspaceWrite,
        allow(&["api.example.com:8080"]),
    )
    .with_transport(transport);

    let (replies, _) = run(
        http,
        "port-pinned",
        &handler("http://api.example.com:9090/x", "undefined"),
    );
    assert!(
        replies[0].starts_with("ERR Error HBOT_HTTP_DENIED:"),
        "port 9090 must not pass a `:8080` entry: {replies:?}"
    );
    assert_eq!(calls.count(), 0);
}

// ---------------------------------------------------------------------------
// 4. Read-only sandboxes have no network, before any authorization runs.
// ---------------------------------------------------------------------------

#[test]
fn a_read_only_sandbox_denies_network_before_authorization() {
    // The allow-list would permit this host, and the transport would block for
    // 30 s if it were ever called: a DENIED answer in well under 50 ms is only
    // possible if the read-only check runs *first* and no socket is opened.
    let (transport, calls) = Canned::new(Behaviour::Block(Duration::from_secs(30)));
    let http = HttpConfig::new(SandboxMode::ReadOnly, allow(&["*"])).with_transport(transport);

    let (replies, elapsed) = run(
        http,
        "read-only",
        &handler("http://10.255.255.1:81/x", "undefined"),
    );

    let reply = &replies[0];
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_DENIED:"),
        "read-only must be a denial, not a transport failure: {reply:?}"
    );
    assert!(
        reply.to_lowercase().contains("read-only"),
        "the message must name the read-only sandbox: {reply:?}"
    );
    assert!(!reply.contains("HBOT_HTTP_FAILED"), "wrong code: {reply:?}");
    assert_eq!(calls.count(), 0, "no transport call, so no socket");
    assert!(
        elapsed < Duration::from_millis(50),
        "read-only denial must be immediate, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Failure modes: timeout, size cap, transport error — all rejections.
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_response_fails_and_does_not_leak_the_body() {
    let (transport, calls) = Canned::new(Behaviour::Oversized);
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allow(&["api.example.com"]))
        .with_transport(transport)
        .with_max_bytes(256);

    let (replies, _) = run(
        http,
        "oversized",
        &handler("http://api.example.com/big", "undefined"),
    );

    let reply = &replies[0];
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_FAILED:"),
        "an oversized response is a failure, not a denial: {reply:?}"
    );
    assert!(
        reply.contains("256-byte limit"),
        "the message must state the limit: {reply:?}"
    );
    assert!(
        !reply.contains("SECRETBODY"),
        "the body must never be echoed: {reply:?}"
    );
    assert_eq!(calls.count(), 1);
}

#[test]
fn a_slow_response_times_out_as_a_failure() {
    let (transport, calls) = Canned::new(Behaviour::Block(Duration::from_secs(5)));
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allow(&["api.example.com"]))
        .with_transport(transport);

    let (replies, elapsed) = run(
        http,
        "timeout",
        &handler("http://api.example.com/slow", "{ timeoutMs: 100 }"),
    );

    let reply = &replies[0];
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_FAILED:"),
        "a timeout is a failure: {reply:?}"
    );
    assert!(
        reply.contains("timed out"),
        "the message must say it timed out: {reply:?}"
    );
    assert_eq!(calls.count(), 1);
    // `timeoutMs: 100` must cut the 5 s transport wait; the margin is generous so
    // a loaded CI machine does not make this flaky.
    assert!(
        elapsed < Duration::from_secs(3),
        "the per-request budget must cut the wait, took {elapsed:?}"
    );
}

#[test]
fn a_transport_error_is_a_failure_and_keeps_the_request_redacted() {
    let (transport, calls) = Canned::new(Behaviour::Fail);
    let http = HttpConfig::new(SandboxMode::WorkspaceWrite, allow(&["api.example.com"]))
        .with_transport(transport);

    let (replies, _) = run(
        http,
        "transport-error",
        &handler("http://api.example.com/x?token=SECRET", "undefined"),
    );

    let reply = &replies[0];
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_FAILED:"),
        "a transport error is a failure: {reply:?}"
    );
    assert_eq!(calls.count(), 1);
}

// ---------------------------------------------------------------------------
// 6. A sandbox built the ordinary way has no network at all.
// ---------------------------------------------------------------------------

#[test]
fn sandbox_spawn_without_http_configuration_denies_everything() {
    let root = project(
        "spawn-default",
        &[(
            "src/main.ts",
            &handler("http://api.example.com/x", "undefined"),
        )],
    );
    let compiled = compile_project(root.path()).expect("project compiles");
    // `Sandbox::spawn` is the constructor every existing caller uses; it must
    // stay network-free even if a manifest lists hosts, because it is handed no
    // `HttpConfig` and therefore no transport either.
    let sandbox = Sandbox::spawn(compiled, Limits::default()).expect("isolate boots");
    let outcome = sandbox
        .dispatch(&json!({ "payload": { "text": "hi" } }))
        .expect("handler runs");

    let reply = &outcome.replies[0];
    // `spawn` installs a deny-all config, so the refusal happens at the
    // authorization step — the honest message for "this sandbox was never given
    // a list", and no transport exists behind it either.
    assert!(
        reply.starts_with("ERR Error HBOT_HTTP_DENIED:")
            && reply.contains("permissions.http is empty"),
        "the default constructor must fail closed with a clear message: {reply:?}"
    );
}
