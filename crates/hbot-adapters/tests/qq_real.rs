//! 真实网关冒烟测试（默认 `#[ignore]`）。
//!
//! 只从环境变量读凭据：
//!
//! ```powershell
//! $env:HBOT_QQ_APP_ID = '<appId>'
//! $env:HBOT_QQ_APP_SECRET = '<clientSecret>'
//! cargo test -p hbot-adapters --test qq_real -- --ignored --nocapture
//! ```
//!
//! 凭据**绝不写进仓库**：默认忽略，缺环境变量时直接跳过且不打印任何凭据；
//! 结果只报「拿到了 / 长度 N / expires_in N」，永远不打印 token 与 secret 的值。

use hbot_adapters::qq::{QqConfig, probe_access_token};

#[tokio::test]
#[ignore = "需要真凭据与真网关"]
async fn the_official_token_endpoint_hands_out_an_access_token() {
    let config = match QqConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            // 只报缺了哪个环境变量，不回显凭据。
            println!("skipping: {error}");
            return;
        }
    };

    match probe_access_token(&config).await {
        Ok(probe) => {
            // 刻意只报长度与有效期：token 值本身不落任何输出。
            println!(
                "access_token obtained: length={} expires_in={}s",
                probe.token_len,
                probe.expires_in.as_secs()
            );
            assert!(probe.token_len > 0, "the token must not be empty");
            assert!(
                probe.expires_in.as_secs() > 0,
                "expires_in must be positive"
            );
        }
        Err(error) => {
            // 错误串已在上游脱敏（只带 HTTP 状态与平台数字 code）。
            panic!("token probe failed: {error}");
        }
    }
}
