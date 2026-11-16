//! `http.get` 的真传输层：把 [`hbot_code::HttpTransport`] 接到 `hbot-adapters` 的
//! HTTP/1.1 客户端上。
//!
//! ## 为什么这一层在 `hbot-rpc`
//!
//! 宿主 API 的契约与授权判据都在 `hbot-code`（`permissions::authorize` 是**唯一**一份
//! 判据），但 `hbot-code` 只依赖 `hbot-core`。真正发请求的客户端在 `hbot-adapters`，
//! 而 `hbot-rpc` 是唯一同时依赖两者的组合根（见 `xtask/src/deps.rs` 的允许边）。
//! 所以**依赖边一条都没加**：传输实现落在这里，`hbot-code` 仍然看不见 `hbot-adapters`。
//!
//! ## 两次判定，但只有一份判据
//!
//! | 层 | 判据 | 拒绝时给模型看到的东西 |
//! |---|---|---|
//! | `hbot-code` | 清单的 `permissions.http`（空表 = 全禁），只读档直接拒 | `HBOT_HTTP_DENIED: …` |
//! | `hbot-adapters` | 协议安全：只 http/https、拒 userinfo、拒 CR/LF、拒 3xx 跟随、长度/超时上限 | 这里映射回 [`TransportError`]，`Blocked` 仍是 `HBOT_HTTP_DENIED` |
//!
//! **地址位置（回环/内网/云元数据）不再由适配层判**——这是 2026-11-16 的产品决策
//! （策略 **C**，见 `docs/02-architecture.md` §6.6）：网络范围完全由清单决定。
//! 所以 [`AdaptersHttpTransport::new`] 把 [`FetchOptions::allow_private`] 置真，清单写了
//! `127.0.0.1:11434` 就真能连上，写 `*` 就什么都能连（**含云元数据地址**，代价见该字段文档）。
//!
//! ## 阻塞契约与线程
//!
//! [`hbot_code::HttpTransport::get`] 是**阻塞**的（调用方在 `spawn_blocking` 里执行它），而
//! `hbot-adapters` 的抓取函数是 `async`。这里不能直接 `Handle::block_on`：调用线程本身
//! 已经在 isolate 的 tokio runtime 里，嵌套 `block_on` 会 panic。所以每次抓取开一个短命的
//! `hbot-http` 线程、自带 `current_thread` runtime 跑完就退出 —— 与 isolate 线程同一套思路，
//! 代价是一次线程启动（bot 的 `http.get` 是人的节奏，不是每秒几万次）。
//!
//! ## 超限语义
//!
//! 适配层在 `http_fetch_full*` 里**截断**并把 `truncated` 置真；这里把截断映射成
//! [`TransportError::TooLarge`]，于是 `hbot-code` 仍然按既有语义**报错**（
//! `HBOT_HTTP_FAILED: … the transport refused a response body above its limit`）而不是把
//! 半截正文当成功交给模型代码。截断的正文永远不会越过这一层。

use std::path::Path;

use hbot_adapters::error::AdapterError;
use hbot_adapters::http::{FetchOptions, FullResponse, http_fetch_full_with};
use hbot_code::{HttpConfig, HttpRequest, HttpResponse, HttpTransport, TransportError};
use hbot_core::SandboxMode;

/// bot 代码的沙箱档位。
///
/// bot 运行时目前**没有**档位旋钮（`BotConfig` 里没有这个字段），历史行为等价于
/// `workspace-write`；`read-only` 的判定在 `hbot-code` 里（授权判断之前就拒），
/// 这里只是把档位如实传下去。将来有了旋钮，改的就是这一个实参。
const BOT_SANDBOX_MODE: SandboxMode = SandboxMode::WorkspaceWrite;

/// 真传输：转调 `hbot-adapters` 的 `http_fetch_full_with`。
///
/// 生产构造用 [`AdaptersHttpTransport::new`]。
#[derive(Debug, Clone)]
pub struct AdaptersHttpTransport {
    /// 交给 [`FetchOptions::allowed_hosts`] 的**写明的**主机名。
    ///
    /// 生产永远为空 —— 见 [`AdaptersHttpTransport::with_allowed_hosts`]。
    allowed_hosts: Vec<String>,
    /// 是否放开地址位置判定。生产恒 `true`（策略 C：网络范围由清单决定）。
    allow_private: bool,
}

impl AdaptersHttpTransport {
    /// 生产用的构造。
    ///
    /// `allow_private: true` 不是随手写的开关：授权判据只有一份，在
    /// `hbot_code::permissions::authorize`（清单的 `permissions.http`）。适配层若再按地址位置
    /// 拒一次，就会出现"清单写了 `127.0.0.1:11434` 却连不上"——两道判据各说一套，最终解释权
    /// 却在第一道。代价（清单写 `*` 时可访问云元数据地址）见 [`FetchOptions::allow_private`]。
    pub const fn new() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            allow_private: true,
        }
    }

    /// **只给测试用**：把写明的回环主机名交给适配层的那条既有逃生口
    /// （[`FetchOptions::allowed_hosts`]，`hbot-adapters` 的 loopback/TLS 测试同样用它）。
    ///
    /// 端到端测试必须打本机的 loopback server；这里仍然显式点名"这一个主机"，
    /// 不依赖生产构造的 `allow_private`，免得测试掩盖"点名名单"那条路走不通。
    #[doc(hidden)]
    pub fn with_allowed_hosts(hosts: Vec<String>) -> Self {
        Self {
            allowed_hosts: hosts,
            allow_private: true,
        }
    }

    fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let url = request.url.clone();
        let timeout = request.timeout;
        let max_bytes = request.max_bytes;
        let options = FetchOptions {
            headers: request.headers.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
            allow_private: self.allow_private,
            ..FetchOptions::default()
        };

        // 见模块文档：不能嵌套 `block_on`，所以自己开线程 + 自己的 runtime。
        let worker = std::thread::Builder::new()
            .name("hbot-http".to_owned())
            .spawn(move || -> Result<FullResponse, AdapterError> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        AdapterError::transport(format!("http runtime 建立失败：{error}"))
                    })?;
                runtime.block_on(http_fetch_full_with(&url, timeout, max_bytes, &options))
            })
            .map_err(|error| TransportError::Failed(format!("http 线程建立失败：{error}")))?;

        let joined = worker
            .join()
            .map_err(|_| TransportError::Failed("http 线程 panic".to_owned()))?;
        let response = joined.map_err(map_adapter_error)?;
        if response.truncated {
            // 适配层截断了正文：按 `hbot-code` 的既有语义报超限，不把半截正文交出去。
            return Err(TransportError::TooLarge);
        }
        Ok(HttpResponse {
            status: response.status,
            // `HttpResponse::body` 是 `String`（引擎侧就是 JS 字符串）；二进制正文按
            // lossy 解码，与老的 `http_fetch` 完全一致。
            body: String::from_utf8_lossy(&response.body).into_owned(),
            headers: response.headers,
        })
    }
}

impl HttpTransport for AdaptersHttpTransport {
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.fetch(request)
    }
}

/// 与 [`AdaptersHttpTransport::new`] 完全一致。
///
/// 刻意**不**用 `#[derive(Default)]`：派生出来的 `allow_private` 会是 `false`（严格口径），
/// 与生产构造不一致就会变成"默认构造悄悄更严、热路径行为不同"的陷阱。默认必须是生产语义。
impl Default for AdaptersHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// 把适配层的错误映射回引擎的 [`TransportError`]。
///
/// **`Blocked` 必须留在 `Blocked`**：它是"判据拒绝"（内网/回环/非 http），
/// 映射成泛泛失败会让模型代码与日志把"不允许"看成"网络抖动"，重试反而放大问题。
fn map_adapter_error(error: AdapterError) -> TransportError {
    match error {
        AdapterError::Blocked { message } => TransportError::Blocked(message),
        AdapterError::Limit { message } => TransportError::Failed(message),
        AdapterError::Timeout { .. } => TransportError::Timeout,
        AdapterError::Unsupported { message }
        | AdapterError::Protocol { message }
        | AdapterError::Transport { message } => TransportError::Failed(message),
        AdapterError::Tls { message } => TransportError::Failed(format!("tls: {message}")),
    }
}

/// 为一个项目组装 `http.get` 的配置：清单的 `permissions.http` + 真传输。
///
/// 清单读不出来（缺失/非法）时**不会**让沙箱起不来：退回
/// [`HttpConfig::deny_all`]，也就是"清单不成立 = 全禁"。这条是产品安全承诺：
/// 没有可解析的授权面就不发请求，而不是悄悄放开。
///
/// 只在 worker（isolate）创建时调用一次，不在消息热路径上。
pub fn http_config(project_root: &Path) -> HttpConfig {
    match hbot_code::manifest::load(project_root) {
        Ok(manifest) => http_config_with(&manifest, BOT_SANDBOX_MODE, AdaptersHttpTransport::new()),
        Err(error) => {
            tracing::debug!(
                root = %project_root.display(),
                error = %error,
                "hbot.json 不可用，http.get 退回全禁"
            );
            HttpConfig::deny_all()
        }
    }
}

/// [`http_config`] 的组装段：清单 → 判据 + 传输。
///
/// 生产路径传 [`AdaptersHttpTransport::new`]（空名单）。抽出来是为了让端到端测试走
/// **同一段** 接线 —— 测试若自己另写一遍 `.with_transport(...)`，"传输真的被装上"
/// 这条断言就只证明了测试自己。
#[doc(hidden)]
pub fn http_config_with(
    manifest: &hbot_code::Manifest,
    mode: SandboxMode,
    transport: AdaptersHttpTransport,
) -> HttpConfig {
    HttpConfig::from_manifest(manifest, mode).with_transport(transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn blocked_refusals_stay_blocked() {
        let mapped = map_adapter_error(AdapterError::blocked("loopback address 127.0.0.1"));
        assert!(
            matches!(mapped, TransportError::Blocked(ref reason) if reason.contains("127.0.0.1")),
            "{mapped:?}"
        );
    }

    #[test]
    fn timeouts_and_limits_map_to_their_own_variants() {
        let timeout =
            map_adapter_error(AdapterError::timeout("http fetch", Duration::from_secs(1)));
        assert!(matches!(timeout, TransportError::Timeout), "{timeout:?}");

        // 头部超限等 `Limit` 保留原文，别被折成"正文太大"。
        let limit = map_adapter_error(AdapterError::limit(
            "http response headers exceed 65536 bytes",
        ));
        assert!(
            matches!(limit, TransportError::Failed(ref reason) if reason.contains("headers")),
            "{limit:?}"
        );
    }

    #[test]
    fn other_failures_keep_their_message() {
        let protocol = map_adapter_error(AdapterError::protocol(
            "http request failed with status 500",
        ));
        assert!(
            matches!(protocol, TransportError::Failed(ref reason) if reason.contains("500")),
            "{protocol:?}"
        );

        let tls = map_adapter_error(AdapterError::tls("certificate verify failed"));
        assert!(
            matches!(tls, TransportError::Failed(ref reason) if reason.starts_with("tls: ")),
            "{tls:?}"
        );
    }

    #[test]
    fn a_missing_manifest_denies_everything() {
        let dir = std::env::temp_dir().join("hbot-net-no-manifest");
        let _ = std::fs::create_dir_all(&dir);
        let config = http_config(&dir);
        assert!(config.allowed().is_empty(), "缺清单必须退回全禁");
        assert_eq!(config.mode(), SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn production_transport_has_no_allow_list_but_trusts_the_manifest() {
        let transport = AdaptersHttpTransport::new();
        // 名单空：没有"点名放行某个主机"这种绕过授权判据的后门。
        assert!(transport.allowed_hosts.is_empty());
        // 但位置判定放开：授权判据在 hbot-code，适配层不再二次拒绝（策略 C）。
        assert!(transport.allow_private);
    }
}
