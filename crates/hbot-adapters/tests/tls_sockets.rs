//! 真 socket 的 TLS 集成测试：`wss://` 与 `https://` 都跑**真实 TLS 握手**。
//!
//! 证书在**进程内**用 `rcgen` 生成（自签 CA + 服务端证书），不读磁盘、不出网：
//! 只有回环 socket 通信。这里钉住四件事：
//!
//! 1. `wss://` 能握手 → 收一条事件 → 发一个 `send_group_msg` → 服务端主动断开
//!    → 客户端按既有退避**自动重连**，且动作帧的 JSON 形状逐字段固定；
//! 2. 证书名字不匹配 / 链到未知 CA 时**报 `AdapterError::Tls`**，不挂住、
//!    不退回明文（这是"默认校验证书"的直接证据）；
//! 3. 连接阶段有超时闸：对端收下 TCP 却不推进 TLS 握手时报 `Timeout`，不永久挂住；
//! 4. `https://` 走同一套 rustls 栈拿到正文，而 SSRF 守卫的**生产默认仍然拒绝回环**。
//!
//! 为什么测试要 `FetchOptions::allowed_hosts`：本机可绑定的地址只有回环/私网，
//! 而生产默认必须拦回环，所以放行口径做成"显式点名一个主机"的注入参数
//! （详见 `FetchOptions` 的文档）。生产默认是**空名单**，本文件之外没有地方填它。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use hbot_adapters::AdapterError;
use hbot_adapters::http::{FetchOptions, http_fetch, http_fetch_with};
use hbot_adapters::platforms::onebot::backoff::Backoff;
use hbot_adapters::platforms::onebot::{OneBotClient, OneBotConfig, probe_connection};

/// 一次测试用的 PKI：自签 CA + 一张给 `san` 签的服务端证书。
struct TestPki {
    /// CA 证书的 DER 字节，注入客户端信任根用（`with_extra_root_certificates`）。
    ca_der: Vec<u8>,
    /// 服务端 rustls 配置（证书链 + 私钥）。
    server_config: Arc<ServerConfig>,
}

/// 生成 `san`（DNS 名或 IP 字面量）对应的自签 PKI。
///
/// 用 P-256 + ring 后端：与生产依赖走同一个加密后端，避免"测试里是另一套
/// 实现"的假通过。
fn test_pki(san: &str) -> TestPki {
    let ca_key = KeyPair::generate().expect("ca key pair");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "hbot-adapters test CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca self-signed");

    let leaf_key = KeyPair::generate().expect("leaf key pair");
    let mut leaf_params = CertificateParams::new(vec![san.to_string()]).expect("leaf params");
    leaf_params.distinguished_name.push(DnType::CommonName, san);
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf signed by ca");

    let chain = vec![leaf.der().clone(), ca_cert.der().clone()];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("server config");

    TestPki {
        ca_der: ca_cert.der().to_vec(),
        server_config: Arc::new(server_config),
    }
}

/// 一条群消息事件（字段形状与真实 OneBot 11 一致）。
fn group_event(text: &str) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1731652800,
        "self_id": 10000,
        "message_id": 4321,
        "user_id": 20001,
        "group_id": 30001,
        "raw_message": text,
        "font": 0,
        "sender": { "user_id": 20001, "nickname": "阿远" },
        "message": [{ "type": "text", "data": { "text": text } }]
    })
    .to_string()
}

/// 在 `wss://` 的服务端做 WS 握手，并把 `Authorization` 头记到 `seen_auth` 里。
// `result_large_err` 由 tungstenite 握手回调的签名决定（`Err` 是 `ErrorResponse`）。
#[allow(clippy::result_large_err)]
async fn accept_ws<S>(stream: S, seen_auth: Arc<Mutex<Vec<Option<String>>>>) -> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio_tungstenite::accept_hdr_async(stream, move |request: &Request, response: Response| {
        let value = request
            .headers()
            .get("authorization")
            .and_then(|raw| raw.to_str().ok())
            .map(str::to_owned);
        seen_auth.lock().expect("lock seen_auth").push(value);
        Ok(response)
    })
    .await
    .expect("ws handshake over tls")
}

/// 读下一个文本帧（跳过二进一类的其它帧）；带超时，避免测试挂死。
async fn next_text<S>(ws: &mut WebSocketStream<S>) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("frame arrives in time")
            .expect("stream still open")
            .expect("frame is not an error");
        if let Ok(text) = frame.to_text() {
            return text.to_string();
        }
    }
}

/// 读到请求头结束（`\r\n\r\n`）为止，返回整段请求文本。
async fn read_request<S>(stream: &mut S) -> String
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).await.expect("read http request");
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn wss_receives_event_sends_action_and_reconnects() {
    let pki = test_pki("127.0.0.1");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let acceptor = TlsAcceptor::from(pki.server_config.clone());
    let seen_auth = Arc::new(Mutex::new(Vec::<Option<String>>::new()));

    let server = {
        let acceptor = acceptor.clone();
        let seen_auth = seen_auth.clone();
        tokio::spawn(async move {
            // ── 第一条连接：握手 → 推事件 → 收一个动作 → 回响应 → 主动断开。
            let (tcp, _peer) = listener.accept().await.expect("first tcp accept");
            let tls = acceptor.accept(tcp).await.expect("first tls accept");
            let mut ws = accept_ws(tls, seen_auth.clone()).await;
            ws.send(WsMessage::text(group_event("TLS 上的第一条消息")))
                .await
                .expect("send event over tls");

            let action = next_text(&mut ws).await;
            let value: Value = serde_json::from_str(&action).expect("action is json");
            let echo = value
                .get("echo")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let response = json!({
                "status": "ok",
                "retcode": 0,
                "data": { "message_id": 424242 },
                "echo": echo,
            });
            ws.send(WsMessage::text(response.to_string()))
                .await
                .expect("send action response");

            // 服务端主动断开：客户端必须按既有退避自动重连，而不是停在断开的连接上。
            ws.close(None).await.expect("server initiated close");
            drop(ws);

            // ── 第二条连接：推送第二条事件，证明重连真的发生了。
            let (tcp, _peer) = listener.accept().await.expect("second tcp accept");
            let tls = acceptor.accept(tcp).await.expect("second tls accept");
            let mut ws = accept_ws(tls, seen_auth).await;
            ws.send(WsMessage::text(group_event("重连之后的消息")))
                .await
                .expect("send event after reconnect");
            // 保持连接直到客户端停机（断开时 `next` 会结束或报错）。
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(frame) = ws.next().await {
                    if frame.is_err() {
                        break;
                    }
                }
            })
            .await;
            action
        })
    };

    let mut client = OneBotClient::start(
        OneBotConfig::new(format!("wss://{address}"))
            .with_token("tls-token")
            .with_heartbeat_timeout(None)
            .with_backoff(Backoff::new(
                Duration::from_millis(5),
                Duration::from_millis(20),
            ))
            .with_capacities(16, 8)
            .with_extra_root_certificates(vec![pki.ca_der.clone()])
            .with_connect_timeout(Some(Duration::from_secs(10))),
    )
    .expect("client starts on wss://");

    // ① 入站事件（真 TLS 上收到）。
    let first = tokio::time::timeout(Duration::from_secs(10), client.next_event())
        .await
        .expect("first event arrives in time")
        .expect("event channel open");
    assert_eq!(first.text, "TLS 上的第一条消息");
    assert_eq!(first.external_id, "4321");

    // ② 出站动作（真 TLS 上发出，并拿到平台回应）。
    let data = client
        .send_group_msg(30001, "TLS 上的回执", None)
        .await
        .expect("send_group_msg over tls");
    assert_eq!(data.get("message_id").and_then(Value::as_i64), Some(424242));

    // ③ 服务端断开后自动重连，重连后的事件照常进来。
    let second = tokio::time::timeout(Duration::from_secs(10), client.next_event())
        .await
        .expect("event after reconnect arrives in time")
        .expect("event channel open");
    assert_eq!(second.text, "重连之后的消息");

    client.shutdown().await;
    let action = server.await.expect("server task");

    // ④ 动作帧 JSON 形状**逐字段**固定：action / params / echo 三段，
    //    `message` 是无引用时的字符串形式（OneBot 11 的合法写法）。
    let frame: Value = serde_json::from_str(&action).expect("action frame is json");
    assert_eq!(
        frame,
        json!({
            "action": "send_group_msg",
            "params": {
                "group_id": 30001,
                "message": "TLS 上的回执",
            },
            "echo": "hbot-0",
        }),
        "动作帧形状必须与 OneBot 11 完全一致；实际帧: {action}"
    );

    // ⑤ `wss://` 与 `ws://` 一样要带 Bearer 头，而且**每次**连接（含重连）都带。
    let auth = seen_auth.lock().expect("lock seen_auth").clone();
    assert_eq!(auth.len(), 2, "两条连接都应记录 Authorization: {auth:?}");
    assert_eq!(auth[0].as_deref(), Some("Bearer tls-token"));
    assert_eq!(auth[1].as_deref(), Some("Bearer tls-token"));
}

#[tokio::test]
async fn wss_rejects_certificate_that_does_not_match_the_host() {
    // 证书是给 127.0.0.2 签的，客户端连的是 127.0.0.1：链可信但**名字不匹配**。
    let pki = test_pki("127.0.0.2");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let acceptor = TlsAcceptor::from(pki.server_config.clone());
    tokio::spawn(async move {
        if let Ok((tcp, _peer)) = listener.accept().await {
            // 客户端校验失败会中止握手，这里报错是预期的。
            let _ = acceptor.accept(tcp).await;
        }
    });

    let config = OneBotConfig::new(format!("wss://{address}"))
        // 信任根是**对的** CA：只剩"名字不匹配"这一个原因。
        .with_extra_root_certificates(vec![pki.ca_der.clone()])
        .with_connect_timeout(Some(Duration::from_secs(5)));
    let error = tokio::time::timeout(Duration::from_secs(10), probe_connection(&config))
        .await
        .expect("certificate failures must not hang")
        .expect_err("a mismatched certificate must be rejected");

    assert!(
        matches!(error, AdapterError::Tls { .. }),
        "want AdapterError::Tls, got {error:?}"
    );
    // 错误信息要能定位到证书/名字，而不是一句笼统的"连不上"。
    let text = error.to_string();
    assert!(text.starts_with("tls error: "), "{text}");
    assert!(
        text.contains("certificate") || text.contains("name"),
        "tls error should mention the certificate or the name: {text}"
    );
}

#[tokio::test]
async fn wss_rejects_certificate_from_unknown_authority() {
    // 冒名服务器：名字对得上，但证书链到**另一个** CA，客户端只信自己那把 CA。
    let server_pki = test_pki("127.0.0.1");
    let trusted_pki = test_pki("127.0.0.1");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let acceptor = TlsAcceptor::from(server_pki.server_config.clone());
    tokio::spawn(async move {
        if let Ok((tcp, _peer)) = listener.accept().await {
            let _ = acceptor.accept(tcp).await;
        }
    });

    let config = OneBotConfig::new(format!("wss://{address}"))
        .with_extra_root_certificates(vec![trusted_pki.ca_der.clone()])
        .with_connect_timeout(Some(Duration::from_secs(5)));
    let error = tokio::time::timeout(Duration::from_secs(10), probe_connection(&config))
        .await
        .expect("unknown CA must not hang")
        .expect_err("a certificate from an unknown authority must be rejected");

    assert!(
        matches!(error, AdapterError::Tls { .. }),
        "want AdapterError::Tls, got {error:?}"
    );
}

#[tokio::test]
async fn wss_connect_timeout_fires_when_the_tls_handshake_stalls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Ok((tcp, _peer)) = listener.accept().await {
            // 只 accept、一个字节都不发：TLS 握手会停在这里。
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(tcp);
        }
    });

    let config = OneBotConfig::new(format!("wss://{address}"))
        .with_connect_timeout(Some(Duration::from_millis(150)));
    let started = Instant::now();
    let error = probe_connection(&config)
        .await
        .expect_err("a stalled tls handshake must time out");
    let elapsed = started.elapsed();

    assert!(
        matches!(error, AdapterError::Timeout { .. }),
        "want AdapterError::Timeout, got {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "connect timeout did not fire promptly: {elapsed:?}"
    );
}

#[tokio::test]
async fn https_fetch_returns_body_over_tls() {
    let pki = test_pki("127.0.0.1");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let acceptor = TlsAcceptor::from(pki.server_config.clone());

    let server = tokio::spawn(async move {
        let (tcp, _peer) = listener.accept().await.expect("accept");
        let mut tls = acceptor.accept(tcp).await.expect("tls accept");
        let request = read_request(&mut tls).await;
        tls.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 14\r\n\r\nhello over tls",
        )
        .await
        .expect("write response");
        tls.flush().await.expect("flush response");
        request
    });

    let options = FetchOptions {
        extra_roots: vec![pki.ca_der.clone()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        headers: Vec::new(),
    };
    let body = http_fetch_with(
        &format!("https://{address}/hello"),
        Duration::from_secs(10),
        4096,
        &options,
    )
    .await
    .expect("https fetch over a real tls socket");
    assert_eq!(body, "hello over tls");

    let request = server.await.expect("server task");
    assert!(
        request.starts_with("GET /hello HTTP/1.1\r\n"),
        "请求行不对: {request}"
    );
    // 非默认端口必须写进 Host 头，否则对端虚拟主机路由会错。
    assert!(
        request.contains(&format!("Host: {address}")),
        "Host 头应带端口: {request}"
    );
}

#[tokio::test]
async fn https_fetch_rejects_untrusted_certificate() {
    let pki = test_pki("127.0.0.1");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let acceptor = TlsAcceptor::from(pki.server_config.clone());
    tokio::spawn(async move {
        if let Ok((tcp, _peer)) = listener.accept().await {
            let _ = acceptor.accept(tcp).await;
        }
    });

    // 同一台回环地址，但客户端**不**追加信任根：公共根验不过自签证书。
    let options = FetchOptions {
        extra_roots: Vec::new(),
        allowed_hosts: vec!["127.0.0.1".to_string()],
        headers: Vec::new(),
    };
    let error = http_fetch_with(
        &format!("https://{address}/"),
        Duration::from_secs(10),
        4096,
        &options,
    )
    .await
    .expect_err("a self-signed certificate must not be trusted by default");

    assert!(
        matches!(error, AdapterError::Tls { .. }),
        "want AdapterError::Tls, got {error:?}"
    );
}

#[tokio::test]
async fn https_fetch_still_blocks_loopback_with_production_defaults() {
    // 监听器是**活的**：证明拒绝来自 SSRF 守卫，而不是"连不上"。
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");

    let error = http_fetch(
        &format!("https://{address}/"),
        Duration::from_millis(300),
        4096,
    )
    .await
    .expect_err("production defaults must still block loopback over https");
    assert!(
        matches!(error, AdapterError::Blocked { .. }),
        "want AdapterError::Blocked, got {error:?}"
    );
}
