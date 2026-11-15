//! `http_fetch` 的安全边界：**拒绝路径**必须真的拒绝，不能靠上游兜底。

use std::time::Duration;

use hbot_adapters::AdapterError;
use hbot_adapters::http::http_fetch;

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

#[tokio::test]
async fn https_reports_missing_tls_instead_of_plaintext_fallback() {
    let error = http_fetch("https://example.com/", TIMEOUT, LIMIT)
        .await
        .expect_err("https must not silently downgrade");
    assert!(
        matches!(error, AdapterError::Unsupported { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("TLS"));
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
