//! webhook 的声明读取：清单里的路由表。
//!
//! 与 `declared_jobs` / `declared_egress` 同一口径：清单是项目文件，改它除了指纹没有别的
//! 信号，所以每次装配/派发时现读一遍，不在构造时快照（快照会让「改了清单但服务还拿着旧路由」
//! 变成静默失效）。

use std::path::Path;

use hbot_core::WebhookRoute;

/// 清单里声明的 webhook 路由；读不到清单就是空表。
#[must_use]
pub fn declared_webhook(root: &Path) -> Vec<WebhookRoute> {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return Vec::new();
    };
    manifest.capabilities.webhook().to_vec()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_project_without_a_manifest_declares_no_routes() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(declared_webhook(&root).is_empty());
    }
}
