//! [`http_fetch_full`] / [`http_fetch_full_with`] 的集成测试：真 loopback 监听器，
//! 真 SSRF 守卫，**不联网**。
//!
//! 这个文件钉的是 `http.get` 的传输层契约（宿主 API 那边由 `hbot-rpc` 的
//! `AdaptersHttpTransport` 转调）：
//!
//! 1. status / headers / body 三样都拿得到，且额外请求头真的发出去了；
//! 2. 正文超 `max_bytes` 时**截断**并置 `truncated`（不是报错、也不是悄悄截断）；
//! 3. 内网/回环地址**仍然**被拒（`Blocked`），而且是在建 socket 之前就拒；
//! 4. 4xx 算响应（状态码原样交出），而老的 `http_fetch*` 仍然把它当错误 —— 既有语义没被改掉；
//! 5. 老接口在正文超限时仍然报 `Limit`（这次改动没有放宽任何一条限制）。
//!
//! 为什么用 `FetchOptions::allowed_hosts`：本机只能绑回环地址，而生产默认拒绝回环
//! （见 `FetchOptions` 的文档与 `tests/http_guard.rs`）。这个名单只放行**写明的**
//! 主机名，不是关掉 SSRF 校验；第 3 条用例证明默认（空名单）仍会拒绝。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hbot_adapters::error::AdapterError;
use hbot_adapters::http::{
    FetchOptions, FullResponse, http_fetch_full, http_fetch_full_with, http_fetch_with,
};

const TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 一次响应的脚本
// ---------------------------------------------------------------------------

struct Reply {
    status_line: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(body: &[u8]) -> Self {
        Self {
            status_line: "200 OK",
            headers: vec![("Content-Type", "text/plain"), ("X-Server", "loopback")],
            body: body.to_vec(),
        }
    }

    fn with_status(status_line: &'static str, body: &[u8]) -> Self {
        Self {
            status_line,
            headers: vec![("Content-Type", "text/plain")],
            body: body.to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// 回环监听器：记请求、计请求数、按脚本回一次
// ---------------------------------------------------------------------------

struct Loopback {
    addr: SocketAddr,
    count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Loopback {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let count = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::clone(&count);
        let archive = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                hits.fetch_add(1, Ordering::SeqCst);
                let mut buffer: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                archive
                    .lock()
                    .expect("requests")
                    .push(String::from_utf8_lossy(&buffer).into_owned());

                let mut response = Vec::new();
                response
                    .extend_from_slice(format!("HTTP/1.1 {}\r\n", reply.status_line).as_bytes());
                for (name, value) in &reply.headers {
                    response.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
                }
                response.extend_from_slice(
                    format!("Content-Length: {}\r\n\r\n", reply.body.len()).as_bytes(),
                );
                response.extend_from_slice(&reply.body);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        Self {
            addr,
            count,
            requests,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// 只放行这一个回环主机名的抓取策略（见文件头：生产默认是空名单）。
    fn options(&self) -> FetchOptions {
        FetchOptions {
            allowed_hosts: vec![self.addr.ip().to_string()],
            headers: Vec::new(),
            ..FetchOptions::default()
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> String {
        self.requests
            .lock()
            .expect("requests")
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// 1. 三样都拿得到，额外请求头真的发出去
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_response_carries_status_headers_and_body() {
    let server = Loopback::start(Reply::ok(b"hello from loopback"));
    let mut options = server.options();
    options.headers = vec![("X-Test".to_string(), "yes".to_string())];

    let response: FullResponse = http_fetch_full_with(&server.url("/x"), TIMEOUT, 4096, &options)
        .await
        .expect("full response");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello from loopback");
    assert!(!response.truncated, "正文没超限，不应标 truncated");
    // 名字已小写（大小写不敏感地按小写存），值原样。
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "text/plain"),
        "缺少 content-type: {:?}",
        response.headers
    );
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name == "x-server" && value == "loopback"),
        "缺少 x-server: {:?}",
        response.headers
    );

    assert_eq!(server.count(), 1);
    let request = server.last_request();
    assert!(
        request.starts_with("GET /x HTTP/1.1\r\n"),
        "请求行不对: {request:?}"
    );
    assert!(
        request.contains("X-Test: yes\r\n"),
        "额外请求头没有发出去: {request:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. 超 max_bytes：截断 + truncated，而不是报错
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_response_truncates_oversized_body_and_flags_it() {
    let body = vec![b'x'; 64];
    let server = Loopback::start(Reply::ok(&body));

    let response = http_fetch_full_with(&server.url("/big"), TIMEOUT, 16, &server.options())
        .await
        .expect("超限应截断，不应报错");

    assert_eq!(response.status, 200);
    assert_eq!(response.body.len(), 16, "截断到 max_bytes");
    assert_eq!(response.body, vec![b'x'; 16]);
    assert!(response.truncated, "截断必须标出来");
}

// ---------------------------------------------------------------------------
// 3. 内网/回环仍然被拒，而且不建 socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_response_still_blocks_loopback_without_allow_list() {
    let server = Loopback::start(Reply::ok(b"never sent"));

    // 生产默认（空 allowed_hosts）：回环地址必须被拒。
    let error = http_fetch_full(&server.url("/x"), TIMEOUT, 4096)
        .await
        .expect_err("回环地址必须被拒");
    assert!(matches!(error, AdapterError::Blocked { .. }), "{error:?}");

    let error = http_fetch_full("http://10.0.0.5/x", TIMEOUT, 4096)
        .await
        .expect_err("内网地址必须被拒");
    assert!(matches!(error, AdapterError::Blocked { .. }), "{error:?}");

    assert_eq!(server.count(), 0, "被拒的请求不该在 server 上留下痕迹");
}

// ---------------------------------------------------------------------------
// 4. 4xx 是响应；老接口仍然把它当错误
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_response_returns_error_statuses_while_legacy_still_fails() {
    let server = Loopback::start(Reply::with_status("404 Not Found", b"missing"));

    let response = http_fetch_full_with(&server.url("/gone"), TIMEOUT, 4096, &server.options())
        .await
        .expect("404 是响应，不该当传输失败");
    assert_eq!(response.status, 404);
    assert_eq!(response.body, b"missing");
    assert!(!response.truncated);

    // 同一个服务端、同一个 URL，老接口必须仍然报错（既有语义没动）。
    let error = http_fetch_with(&server.url("/gone"), TIMEOUT, 4096, &server.options())
        .await
        .expect_err("老接口把非 2xx 当错误");
    assert!(matches!(error, AdapterError::Protocol { .. }), "{error:?}");
}

// ---------------------------------------------------------------------------
// 5. 老接口仍然按 Limit 拒绝超限正文（这次没有放宽任何限制）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_fetch_still_refuses_oversized_body() {
    let body = vec![b'y'; 64];
    let server = Loopback::start(Reply::ok(&body));

    let error = http_fetch_with(&server.url("/big"), TIMEOUT, 16, &server.options())
        .await
        .expect_err("老接口超限仍是错误");
    assert!(matches!(error, AdapterError::Limit { .. }), "{error:?}");
}

// ---------------------------------------------------------------------------
// 6. 额外头的注入尝试在连之前就被拒
// ---------------------------------------------------------------------------

#[tokio::test]
async fn header_injection_is_refused_before_connecting() {
    let server = Loopback::start(Reply::ok(b"never sent"));
    let mut options = server.options();
    options.headers = vec![("x-evil".to_string(), "a\r\nX-Injected: 1".to_string())];

    let error = http_fetch_full_with(&server.url("/x"), TIMEOUT, 4096, &options)
        .await
        .expect_err("CRLF 注入必须被拒");
    assert!(
        matches!(error, AdapterError::Unsupported { .. }),
        "{error:?}"
    );
    assert_eq!(server.count(), 0, "参数错就不该建连接");
}
