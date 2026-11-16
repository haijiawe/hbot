//! QQ 官方适配器测试用的**进程内假平台**：一个真 HTTPS/HTTP 端点 + 一个真 WebSocket 监听器。
//!
//! 这个文件同时被 `tests/qq_*.rs` 用 `#[path = "qq_support.rs"] mod support;` 包含。
//! 它自己也会被当成一个（空的）测试目标编译，所以整体 `#![allow(dead_code)]`：
//! 每个测试只用到其中一部分 helper。
//!
//! 刻意**不 mock 被测的连接代码**：假平台起的是真的 `TcpListener` + 真的
//! `tokio_tungstenite::accept_async`，客户端走的是与生产同一条 HTTP/WS 代码路径
//! （只有端点地址和自签证书不同）。
//!
//! 服务形态：
//!
//! ```text
//! HTTP 监听器（127.0.0.1:随机端口）
//!   POST /token                      → {"access_token": "…", "expires_in": 7200}
//!   GET  /gateway                    → {"url": "ws://127.0.0.1:<另一端口>/websocket"}
//!   POST /v2/{groups|users}/{id}/messages → {"id": "MSG-OK"}
//!
//! WS 监听器（127.0.0.1:随机端口）
//!   每次握手成功 → 通过 channel 交给测试，由测试逐帧驱动
//! ```
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{WebSocketStream, accept_async};

use hbot_adapters::onebot::{Backoff, NoJitter};
use hbot_adapters::qq::QqConfig;

/// 测试用的 `client_secret`：任何 Debug / 错误串里出现它就是泄露。
pub const TEST_SECRET: &str = "sk-test-DEADBEEF";
/// 测试用的 `app_id`。
pub const TEST_APP_ID: &str = "10240001";
/// 假平台第一次发出的 access token。
pub const TOKEN_ONE: &str = "test-access-token-1";
/// 假平台续期后发出的 access token。
pub const TOKEN_TWO: &str = "test-access-token-2";
/// 真实群消息帧里的 `group_openid` 形状（32 位 HEX，不是 QQ 号）。
pub const GROUP_OPENID: &str = "196AAC0A1E7A06AC0D5EEB7FCF778303";
/// 真实私聊帧里的 `user_openid` 形状。
pub const USER_OPENID: &str = "9A8B7C6D5E4F30211203F4E5D6C7B8A9";
/// 真实群消息帧里的 `id`（官方消息 ID，很长，含 `.`/`-`/`!`）。
pub const GROUP_MESSAGE_ID: &str = "ROBOT1.0_mPc6ExampleMessageId-!_";
/// 帧里 `message_scene.ext` 的 `auth_token` 值：当密钥处理，不许出现在 `raw` 里。
pub const FRAME_AUTH_TOKEN: &str = "SECRET-AUTH-TOKEN";

/// 假平台的行为开关。
#[derive(Clone, Debug)]
pub struct Options {
    /// 每次取 token 依次返回的值（用完后重复最后一个）。
    pub tokens: Vec<String>,
    /// 第几次（1-based）`/v2/...` 请求强制回 401。
    pub fail_401_at: Option<usize>,
    /// 所有 `/v2/...` 请求都回 401（用来断言「不无限重试」）。
    pub always_401: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tokens: vec![TOKEN_ONE.to_owned()],
            fail_401_at: None,
            always_401: false,
        }
    }
}

impl Options {
    /// 设置 token 序列（第一次、第二次…）。
    pub fn with_tokens(mut self, tokens: &[&str]) -> Self {
        self.tokens = tokens.iter().map(|token| (*token).to_owned()).collect();
        self
    }

    /// 让第 `index` 次（1-based）发送请求回 401。
    pub fn fail_401_at(mut self, index: usize) -> Self {
        self.fail_401_at = Some(index);
        self
    }

    /// 让所有发送请求都回 401。
    pub fn always_401(mut self) -> Self {
        self.always_401 = true;
        self
    }
}

/// 假平台记下来的一次 HTTP 请求。
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    /// HTTP 方法（大写）。
    pub method: String,
    /// 请求路径（含 query，如果有）。
    pub path: String,
    /// 头（名字统一小写）。
    pub headers: Vec<(String, String)>,
    /// 原始请求体。
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// 取一个头（名字大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// 把请求体当 JSON 解析。
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// 客户端连上来的一条 WebSocket 连接（由测试驱动）。
pub struct FakeConnection {
    /// 第几条连接（1-based）：用来断言「重连确实发生了」。
    pub index: usize,
    stream: WebSocketStream<TcpStream>,
}

impl FakeConnection {
    /// 发一帧 JSON 文本。
    pub async fn send_json(&mut self, value: &Value) {
        self.stream
            .send(WsMessage::text(value.to_string()))
            .await
            .expect("send frame to the client");
    }

    /// 发一帧**故意坏掉**的文本（测试「坏帧不杀连接」）。
    pub async fn send_raw_text(&mut self, text: &str) {
        self.stream
            .send(WsMessage::text(text.to_owned()))
            .await
            .expect("send raw text to the client");
    }

    /// 收一帧 JSON（默认 5 秒超时）。
    pub async fn recv_json(&mut self) -> Value {
        self.recv_json_timeout(Duration::from_secs(5))
            .await
            .expect("receive a frame from the client")
    }

    /// 收一帧 JSON（带超时）。
    pub async fn recv_json_timeout(&mut self, limit: Duration) -> Result<Value, String> {
        use futures_util::StreamExt;

        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {limit:?} waiting for a frame from the client"
                ));
            }
            match tokio::time::timeout(remaining, self.stream.next()).await {
                Err(_) => {
                    return Err(format!(
                        "timed out after {limit:?} waiting for a frame from the client"
                    ));
                }
                Ok(None) => return Err("the client closed the websocket".to_owned()),
                Ok(Some(Err(err))) => return Err(format!("websocket error: {err}")),
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    return serde_json::from_str(text.as_str())
                        .map_err(|err| format!("client frame is not JSON: {err}"));
                }
                Ok(Some(Ok(WsMessage::Binary(bytes)))) => {
                    return serde_json::from_slice(&bytes)
                        .map_err(|err| format!("client binary frame is not JSON: {err}"));
                }
                // 控制帧：跳过，继续等数据帧。
                Ok(Some(Ok(WsMessage::Ping(_))))
                | Ok(Some(Ok(WsMessage::Pong(_))))
                | Ok(Some(Ok(WsMessage::Frame(_)))) => continue,
                Ok(Some(Ok(WsMessage::Close(_)))) => {
                    return Err("the client closed the websocket".to_owned());
                }
            }
        }
    }

    /// 主动断开（模拟网关掉线）。
    pub async fn close(&mut self) {
        let _ = self.stream.close(None).await;
    }
}

/// HTTP + WS 的共享状态。
struct State {
    requests: Mutex<Vec<RecordedRequest>>,
    token_calls: AtomicUsize,
    api_calls: AtomicUsize,
    options: Options,
}

/// 一个跑在 127.0.0.1 上的假 QQ 开放平台。
pub struct FakePlatform {
    /// HTTP 基址（`http://127.0.0.1:<port>`）。
    pub http_base: String,
    /// 取 token 的完整 URL。
    pub token_url: String,
    /// 网关地址（指向假 WS 监听器）。
    pub ws_url: String,
    state: Arc<State>,
    connections: mpsc::Receiver<FakeConnection>,
}

impl FakePlatform {
    /// 起两个监听器（HTTP 与 WS），返回可直接用的假平台。
    pub async fn start(options: Options) -> Self {
        let http_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake http listener");
        let ws_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake ws listener");
        let http_port = http_listener.local_addr().expect("http addr").port();
        let ws_port = ws_listener.local_addr().expect("ws addr").port();
        let http_base = format!("http://127.0.0.1:{http_port}");
        let token_url = format!("{http_base}/token");
        let ws_url = format!("ws://127.0.0.1:{ws_port}/websocket");

        let state = Arc::new(State {
            requests: Mutex::new(Vec::new()),
            token_calls: AtomicUsize::new(0),
            api_calls: AtomicUsize::new(0),
            options,
        });

        let http_state = Arc::clone(&state);
        let gateway_answer = ws_url.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = http_listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&http_state);
                let gateway_answer = gateway_answer.clone();
                tokio::spawn(async move {
                    handle_http(stream, state, gateway_answer).await;
                });
            }
        });

        let (sender, connections) = mpsc::channel(16);
        tokio::spawn(async move {
            let mut index = 0usize;
            loop {
                let Ok((stream, _)) = ws_listener.accept().await else {
                    return;
                };
                index += 1;
                let sender = sender.clone();
                tokio::spawn(async move {
                    match accept_async(stream).await {
                        Ok(stream) => {
                            let _ = sender.send(FakeConnection { index, stream }).await;
                        }
                        Err(err) => eprintln!("fake ws handshake failed: {err}"),
                    }
                });
            }
        });

        Self {
            http_base,
            token_url,
            ws_url,
            state,
            connections,
        }
    }

    /// 等客户端建立起下一条 WS 连接（默认 5 秒超时）。
    pub async fn next_connection(&mut self) -> FakeConnection {
        tokio::time::timeout(Duration::from_secs(5), self.connections.recv())
            .await
            .expect("the client did not connect to the fake gateway in time")
            .expect("the fake gateway stopped accepting connections")
    }

    /// 断言在 `limit` 内**没有**新连接（用来验证「没有触发重连」）。
    pub async fn expect_no_connection(&mut self, limit: Duration) {
        let result = tokio::time::timeout(limit, self.connections.recv()).await;
        assert!(
            result.is_err(),
            "expected no new websocket connection, but the client connected again"
        );
    }

    /// 目前收到的全部 HTTP 请求。
    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().await.clone()
    }

    /// 发往 `/v2/...` 的请求（按到达顺序）。
    pub async fn send_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .await
            .into_iter()
            .filter(|request| request.path.starts_with("/v2/"))
            .collect()
    }

    /// 取 token 端点被调用的次数。
    pub fn token_calls(&self) -> usize {
        self.state.token_calls.load(Ordering::SeqCst)
    }

    /// `/v2/...` 端点被调用的次数。
    pub fn api_calls(&self) -> usize {
        self.state.api_calls.load(Ordering::SeqCst)
    }

    /// 指向本地假平台的客户端配置（不覆盖心跳相关设置）。
    pub fn config(&self) -> QqConfig {
        QqConfig::new(TEST_APP_ID, TEST_SECRET)
            .with_token_url(self.token_url.clone())
            .with_api_base(self.http_base.clone())
            .with_connect_timeout(Duration::from_secs(3))
            .with_request_timeout(Duration::from_secs(3))
            .with_event_capacity(8)
            .with_msg_seq_capacity(4)
            .with_backoff(Backoff::new(
                Duration::from_millis(20),
                Duration::from_millis(80),
            ))
            .with_jitter(Arc::new(NoJitter))
    }
}

/// 处理一次 HTTP 请求（一个请求一个连接，客户端带 `Connection: close`）。
async fn handle_http(mut stream: TcpStream, state: Arc<State>, gateway_url: String) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let route = (
        request.method.as_str(),
        request.path.split('?').next().unwrap_or_default(),
    );
    let (status, payload) = match route {
        ("POST", "/token") => {
            let index = state.token_calls.fetch_add(1, Ordering::SeqCst);
            let tokens = &state.options.tokens;
            let token = tokens
                .get(index)
                .or_else(|| tokens.last())
                .cloned()
                .unwrap_or_else(|| TOKEN_ONE.to_owned());
            (200, json!({"access_token": token, "expires_in": 7200}))
        }
        ("GET", "/gateway") => (200, json!({"url": gateway_url})),
        ("POST", path) if path.starts_with("/v2/") => {
            let call = state.api_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if state.options.always_401 || state.options.fail_401_at == Some(call) {
                (401, json!({"code": 40001, "message": "invalid token"}))
            } else {
                (200, json!({"id": "MSG-OK", "timestamp": 1}))
            }
        }
        _ => (404, json!({"code": 404, "message": "not found"})),
    };
    state.requests.lock().await.push(request);

    let body = payload.to_string();
    let status_line = match status {
        200 => "200 OK",
        401 => "401 Unauthorized",
        _ => "404 Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// 读一次 HTTP/1.1 请求（头 + `content-length` 正文）。
async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(index) = find_subsequence(&buffer, b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// 在字节切片里找一个子序列的位置。
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `op:10` Hello。
pub fn hello_frame(interval: Duration) -> Value {
    json!({
        "op": 10,
        "d": {
            "heartbeat_interval": u64::try_from(interval.as_millis()).unwrap_or(45_000),
        }
    })
}

/// `op:0` `READY`（`session_id` 用于之后的 Resume）。
pub fn ready_frame(session_id: &str, seq: i64) -> Value {
    json!({
        "op": 0,
        "s": seq,
        "t": "READY",
        "d": {
            "version": 1,
            "session_id": session_id,
            "user": {"id": "10240001", "username": "hbot", "bot": true},
            "shard": [0, 1],
        }
    })
}

/// `op:0` `RESUMED`。
pub fn resumed_frame(seq: i64) -> Value {
    json!({"op": 0, "s": seq, "t": "RESUMED", "d": {}})
}

/// 真实形状的群 `@` 消息事件（`GROUP_MESSAGE_CREATE`）。
pub fn group_message_frame(seq: i64, message_id: &str, content: &str) -> Value {
    json!({
        "op": 0,
        "s": seq,
        "t": "GROUP_MESSAGE_CREATE",
        "d": {
            "id": message_id,
            "content": content,
            "group_id": GROUP_OPENID,
            "group_openid": GROUP_OPENID,
            "message_type": 0,
            "timestamp": "2026-11-16T08:06:39+08:00",
            "author": {
                "id": "9A8B7C6D5E4F30211203F4E5D6C7B8A9",
                "member_openid": "9A8B7C6D5E4F30211203F4E5D6C7B8A9",
                "member_nickname": "测试用户",
                "member_role": "member",
                "union_openid": "",
                "username": "",
                "bot": false,
            },
            "mentions": [{
                "bot": true,
                "id": "10240001",
                "is_you": true,
                "member_openid": "196AAC0A1E7A06AC0D5EEB7FCF778303",
                "member_role": "member",
                "scope": "single",
            }],
            "message_scene": {
                "ext": ["msg_idx=REFIDX_EXAMPLE", format!("auth_token={FRAME_AUTH_TOKEN}")],
                "source": "default",
            },
        }
    })
}

/// 真实形状的单聊消息事件（`C2C_MESSAGE_CREATE`）。
pub fn private_message_frame(seq: i64, message_id: &str, content: &str) -> Value {
    json!({
        "op": 0,
        "s": seq,
        "t": "C2C_MESSAGE_CREATE",
        "d": {
            "id": message_id,
            "content": content,
            "message_type": 0,
            "timestamp": "2026-11-16T08:06:39+08:00",
            "author": {
                "id": USER_OPENID,
                "union_openid": "",
                "user_openid": USER_OPENID,
                "username": "",
                "bot": false,
            },
            "message_scene": {"ext": [], "source": "default"},
        }
    })
}

/// `op:11` 心跳 ACK。
pub fn heartbeat_ack_frame() -> Value {
    json!({"op": 11})
}

/// `op:9` invalid session（服务端拒绝 Resume）。
pub fn invalid_session_frame() -> Value {
    json!({"op": 9, "d": false})
}

/// `op:7` 要求客户端重连。
pub fn reconnect_frame() -> Value {
    json!({"op": 7})
}

/// `op:1` 服务端主动要求心跳。
pub fn server_heartbeat_request() -> Value {
    json!({"op": 1, "d": null})
}
