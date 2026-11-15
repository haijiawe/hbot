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
/// 后台服务的传输层是 `tokio-tungstenite`（无 TLS feature），所以这里显式写
/// `tls=disabled`：UI 只能连本机 `ws://127.0.0.1:PORT`。启用 TLS 时改这一处。
pub fn capabilities() -> &'static [&'static str] {
    &["rpc-ws", "headless", "tls=disabled"]
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
        assert!(text.contains("tls=disabled"), "输出：{text}");
    }
}
