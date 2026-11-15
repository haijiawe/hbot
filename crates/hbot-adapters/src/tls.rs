//! TLS 后端：`wss://`（OneBot 正向 WS）与 `https://`（[`crate::http::http_fetch`]）
//! 共用的一份 rustls 客户端配置。
//!
//! 三条硬约束：
//!
//! 1. **证书校验始终开启**。没有 `insecure` / `danger_accept_invalid_certs` 之类的
//!    开关，也没有"校验失败就退回明文"的分支 —— 那等于把中间人攻击合法化。
//! 2. 信任根 = Mozilla 打包的公共根（`webpki-roots`）**加上**调用方显式追加的根证书
//!    （私有 CA / 自签部署，见 `OneBotConfig::with_extra_root_certificates` 与
//!    `FetchOptions::extra_roots`）。追加是"多信任一张"，不是"少校验一张"。
//! 3. 只用 `ring` 一个加密后端：`Cargo.toml` 里 `rustls` 关掉默认 feature 只留 `ring`。
//!    rustls 0.23 在同时启用 `ring` 与 `aws-lc-rs` 时 `CryptoProvider::get_default()`
//!    会 panic（"multiple providers"），而 workspace 锁文件里现有的 `rustls` 只带 `ring`。
//!
//! 为什么默认配置缓存在 `OnceLock` 里：`RootCertStore` 要解析约 150 张根证书，
//! 而 OneBot 的重连循环可能几十秒就连一次，每次重建纯属白烧 CPU。

use std::io;
use std::sync::{Arc, OnceLock};

use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};

use crate::error::AdapterError;

/// 默认（只用公共根）的客户端配置；进程内只构建一次。
pub(crate) fn default_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(public_roots())
                .with_no_client_auth(),
        )
    }))
}

/// 公共根 + `extra_roots`（DER 编码）构建客户端配置。
///
/// `extra_roots` 为空时直接复用缓存的那份默认配置。
pub(crate) fn client_config(extra_roots: &[Vec<u8>]) -> Result<Arc<ClientConfig>, AdapterError> {
    if extra_roots.is_empty() {
        return Ok(default_client_config());
    }
    let mut roots = public_roots();
    for (index, der) in extra_roots.iter().enumerate() {
        // 追加根证书解析失败必须报错：静默忽略会让部署方以为"已经信任私有 CA 了"，
        // 实际每次连接都失败，而且要到连接阶段才暴露（比启动时报错难查得多）。
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(|err| {
                AdapterError::tls(format!(
                    "extra root certificate #{index} is not a valid DER certificate: {err}"
                ))
            })?;
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// Mozilla 打包的公共根证书。
fn public_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// 把 `io::Error` 里的 rustls 具体原因挖出来。
///
/// tokio-rustls 把 [`rustls::Error`] 包在 `io::Error` 的 source 里，直接打印外层
/// 只剩 `Kind(InvalidData)` 之类的信息 —— 证书类问题（`UnknownIssuer`、
/// `NotValidForName`）必须原样透出，否则排查方向全错。
///
/// 返回 `Some` 表示这是 TLS 层失败；`None` 表示是普通 IO 失败（连接被重置等）。
pub(crate) fn classify_tls_failure(context: &str, err: &io::Error) -> Option<AdapterError> {
    let inner = err.get_ref()?.downcast_ref::<rustls::Error>()?;
    Some(AdapterError::tls(format!("{context}: {inner}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_cached_and_verifies() {
        let first = default_client_config();
        let second = default_client_config();
        // 同一个 `Arc`：重连循环里不该反复解析 ~150 张根证书。
        assert!(Arc::ptr_eq(&first, &second));
        // 信任根非空 = 校验真的在跑（空 store 会让所有连接失败，而不是跳过校验）。
        assert!(!public_roots().is_empty());
    }

    #[test]
    fn extra_roots_reject_garbage_without_panicking() {
        let err = client_config(&[vec![0u8; 16]]).expect_err("garbage DER must be rejected");
        assert!(
            err.to_string().contains("not a valid DER certificate"),
            "{err}"
        );
    }

    #[test]
    fn empty_extra_roots_share_the_default_config() {
        let with_empty = client_config(&[]).expect("empty extra roots are fine");
        assert!(Arc::ptr_eq(&with_empty, &default_client_config()));
    }

    #[test]
    fn plain_io_errors_are_not_misreported_as_tls() {
        let err = io::Error::new(io::ErrorKind::ConnectionReset, "peer went away");
        assert!(classify_tls_failure("handshake", &err).is_none());
    }
}
