//! `http_fetch` 的安全边界：**拒绝路径**必须真的拒绝，不能靠上游兜底。

use std::time::Duration;

use hbot_adapters::AdapterError;
use hbot_adapters::http::{FetchOptions, http_fetch, http_fetch_with};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TIMEOUT: Duration = Duration::from_millis(300);
const LIMIT: usize = 4096;

#[tokio::test]
async fn rejects_loopback_and_private_targets() {
    for url in [
        "http://127.0.0.1/",
        "http://127.0.0.1:8080/admin",
        "http://localhost:3001/",
        "http://10.0.0.5/",
        "http://172.16.3.4/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/",
        "http://metadata.google.internal/computeMetadata/v1/",
        // `https://` 现在真的会握手，但**静态守卫必须在握手之前**就拦住内网。
        "https://127.0.0.1/",
        "https://192.168.1.1/",
        "https://localhost/",
    ] {
        let error = match http_fetch(url, TIMEOUT, LIMIT).await {
            Ok(body) => panic!("{url} must be rejected, got {} bytes instead", body.len()),
            Err(error) => error,
        };
        match error {
            AdapterError::Blocked { .. } => {}
            other => panic!("{url} must be Blocked, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn rejects_non_http_schemes() {
    for url in [
        "file:///etc/passwd",
        "file://C:/Windows/win.ini",
        "ftp://example.com/a.txt",
        "data:text/plain,hello",
    ] {
        let error = match http_fetch(url, TIMEOUT, LIMIT).await {
            Ok(body) => panic!("{url} must be rejected, got {} bytes instead", body.len()),
            Err(error) => error,
        };
        assert!(
            matches!(error, AdapterError::Blocked { .. }),
            "{url} must be Blocked, got {error:?}"
        );
    }
}

/// `https://` 必须是**真 TLS**，不能悄悄退回明文。
///
/// 做法：在回环起一个**明文** HTTP 服务器，用 `https://` 连它。
/// - 如果实现降级成明文，就会拿到 200 与正文 `ok`（测试直接红）；
/// - 正确实现会在最前面发出 TLS 握手记录（首字节 `0x16`），然后因为对端不是
///   TLS 而失败，错误必须是 `Tls`（证书/握手）或 `Transport`（IO），
///   绝不是成功。
///
/// 这里用 `FetchOptions::allowed_hosts` 精确放行 `127.0.0.1` —— 生产默认是空
/// 名单（见 `FetchOptions` 文档与 `rejects_loopback_and_private_targets`）。
#[tokio::test]
async fn https_is_not_a_plaintext_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut head = [0u8; 8];
        let _ = stream.read(&mut head).await;
        // 故意回一个合法的明文响应：降级实现会把它当成功读出来。
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await;
        head
    });

    let options = FetchOptions {
        allowed_hosts: vec!["127.0.0.1".to_string()],
        ..FetchOptions::default()
    };
    let url = format!("https://127.0.0.1:{}/", addr.port());
    let error = http_fetch_with(&url, TIMEOUT, LIMIT, &options)
        .await
        .expect_err("https must never fall back to plaintext");
    assert!(
        matches!(
            error,
            AdapterError::Tls { .. } | AdapterError::Transport { .. }
        ),
        "want a TLS/transport failure, got {error:?}"
    );

    let head = server.await.expect("server task");
    // TLS 记录类型 0x16 = handshake（TLS 1.x ClientHello 的首字节）。
    assert_eq!(
        head[0], 0x16,
        "client must open with a TLS handshake record, got {head:?}"
    );
}

#[tokio::test]
async fn invalid_limits_and_timeouts_are_rejected() {
    let zero_limit = http_fetch("http://example.com/", TIMEOUT, 0)
        .await
        .expect_err("max_bytes 0 must be rejected");
    assert!(matches!(zero_limit, AdapterError::Unsupported { .. }));

    let zero_timeout = http_fetch("http://example.com/", Duration::ZERO, LIMIT)
        .await
        .expect_err("zero timeout must be rejected");
    assert!(
        matches!(zero_timeout, AdapterError::Timeout { .. }),
        "{zero_timeout:?}"
    );

    let huge_url = format!("http://example.com/{}", "a".repeat(9000));
    let error = http_fetch(&huge_url, TIMEOUT, LIMIT)
        .await
        .expect_err("over-long url must be rejected");
    assert!(matches!(error, AdapterError::Limit { .. }), "{error:?}");
}
