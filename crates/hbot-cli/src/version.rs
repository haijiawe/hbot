//! `hbot version`：版本、平台与能力位。
//!
//! 输出是给人和脚本看的稳定文本，不依赖竞态信息（时间、路径）。

/// 版本信息的结构化形态（`--version` 之外的详细输出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionInfo {
    /// 二进制名（恒为 `hbot`）。
    pub name: &'static str,
    /// cargo 版本号。
    pub version: &'static str,
    /// 操作系统（`std::env::consts::OS`）。
    pub os: &'static str,
    /// CPU 架构（`std::env::consts::ARCH`）。
    pub arch: &'static str,
}

/// 当前构建的版本信息。
pub fn version_info() -> VersionInfo {
    VersionInfo {
        name: "hbot",
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

/// 能力位。
///
/// 两件事要分清，之前写成 `tls=disabled` 让读取这个输出的人（含 Agent）以为
/// 「平台连接也不支持 TLS」：
///
/// * `rpc-tls=disabled`——**后台服务的 RPC 监听**没有 TLS，UI 只能连本机
///   `ws://127.0.0.1:PORT`（有 token + Origin 白名单，见 §9.3）。启用 RPC TLS 时改这一处。
/// * `platform-tls=rustls`——**平台适配器**的 `wss://`/`https://` 走 rustls + 公共根证书，
///   校验始终开启（`crates/hbot-adapters/src/tls.rs`）。
pub fn capabilities() -> &'static [&'static str] {
    &[
        "rpc-ws",
        "headless",
        "rpc-tls=disabled",
        "platform-tls=rustls",
    ]
}

/// `hbot version` 的完整输出（多行，无尾换行）。
pub fn version_text() -> String {
    let info = version_info();
    format!(
        "{} {}\nplatform: {} ({})\ncapabilities: {}",
        info.name,
        info.version,
        info.os,
        info.arch,
        capabilities().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_text_starts_with_name_and_version() {
        let text = version_text();
        let first = text.lines().next().unwrap_or_default();
        assert_eq!(first, format!("hbot {}", env!("CARGO_PKG_VERSION")));
        assert!(text.contains("platform: "), "输出：{text}");
        assert!(text.contains("capabilities: rpc-ws"), "输出：{text}");
        // RPC 监听没有 TLS；平台适配器有。两者别再混成一个 `tls=disabled`。
        assert!(text.contains("rpc-tls=disabled"), "输出：{text}");
        assert!(text.contains("platform-tls=rustls"), "输出：{text}");
    }
}
