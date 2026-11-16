//! crate 依赖方向闸门。
//!
//! 判据只有一条：**每一条内部依赖边都必须在白名单里**（`ALLOWED`）。
//! 没登记的边就是违规，不区分“看起来合理”。新增依赖必须同时改这里和
//! `docs/03-directory.md` §4，让架构约束变成一次显式决定。
//!
//! 为什么不按“禁止反向边”写：反向边只覆盖已知的坏方向，新 crate 一加进来
//! 就成了默认放行。白名单反过来——默认拒绝。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 每个 crate 允许依赖的内部 crate。`[]` = 不许依赖任何内部 crate。
///
/// 顺序无关，只用于报错时提示“这个 crate 本来该依赖谁”。
const ALLOWED: &[(&str, &[&str])] = &[
    // 最底层：领域类型与端口。它不认识任何其它 crate。
    ("hbot-core", &[]),
    // 纯存储：只懂 core 的类型与行级 DTO。
    ("hbot-store", &["hbot-core"]),
    // 生成代码编译与 QuickJS 执行。
    ("hbot-code", &["hbot-core"]),
    // 项目文件访问、路径守卫、搜索。
    ("hbot-workspace", &["hbot-core"]),
    ("hbot-command", &["hbot-core"]), // 受控命令执行：agent → command → core，不许反向
    // 设备后端（C6）：Win32 SendInput / 截图 / 前台窗口。只认 core 的
    // DeviceBackend trait，不认识 bot/agent/rpc（否则闸就会被绕）。
    ("hbot-device", &["hbot-core"]),
    // 平台协议适配（OneBot / Webhook），不含业务规则。
    ("hbot-adapters", &["hbot-core"]),
    // Bot 运行时：会话队列、outbox、supervisor。handler 由外部注入，
    // 所以它不认识 hbot-agent / hbot-code。
    ("hbot-bot", &["hbot-core", "hbot-store"]),
    // Agent：单循环 + ToolGateway + provider。不许依赖 bot/rpc/desktop。
    (
        "hbot-agent",
        &["hbot-core", "hbot-workspace", "hbot-code", "hbot-command"],
    ),
    // RPC：唯一编排层，把上面所有实现 crate 拼起来。
    (
        "hbot-rpc",
        &[
            "hbot-core",
            "hbot-store",
            "hbot-code",
            "hbot-workspace",
            "hbot-adapters",
            "hbot-bot",
            "hbot-agent",
            "hbot-command",
            "hbot-device",
        ],
    ),
    // CLI：组合根之一。
    (
        "hbot-cli",
        &[
            "hbot-core",
            "hbot-rpc",
            "hbot-store",
            "hbot-code",
            "hbot-workspace",
            "hbot-adapters",
            "hbot-bot",
            "hbot-agent",
        ],
    ),
    // 桌面壳：只碰契约与 dispatch。
    ("hbot-desktop", &["hbot-core", "hbot-rpc"]),
    // 跨 crate 集成回归套件：只含测试，必须能同时看见所有实现 crate 的接缝。
    (
        "hbot-parity",
        &[
            "hbot-core",
            "hbot-store",
            "hbot-code",
            "hbot-bot",
            "hbot-agent",
            "hbot-workspace",
            "hbot-adapters",
            "hbot-rpc",
        ],
    ),
    // 红队用例集：只含测试，故意能同时看见所有安全边界（路径、命令、沙箱、密钥）。
    (
        "hbot-redteam",
        &[
            "hbot-core",
            "hbot-store",
            "hbot-code",
            "hbot-bot",
            "hbot-agent",
            "hbot-workspace",
            "hbot-adapters",
            "hbot-command",
            "hbot-rpc",
        ],
    ),
    // 性能基准：只量已实现的接缝，不许反向依赖 rpc/desktop。
    (
        "hbot-bench",
        &[
            "hbot-core",
            "hbot-code",
            "hbot-workspace",
            "hbot-store",
            "hbot-adapters",
        ],
    ),
    // 开发期工具。
    ("xtask", &["hbot-rpc", "hbot-core"]),
];

/// 一条内部依赖边。
struct Edge {
    from: String,
    to: String,
    manifest: PathBuf,
}

pub fn run() -> Result<()> {
    let root = super::workspace_root();
    let edges = collect(&root).context("读取 crate 依赖失败")?;

    let allow: BTreeMap<&str, &[&str]> = ALLOWED.iter().copied().collect();
    let mut violations = Vec::new();

    for edge in &edges {
        let Some(allowed) = allow.get(edge.from.as_str()) else {
            violations.push(format!(
                "{}: 未在 xtask/src/deps.rs 的 ALLOWED 表里登记（依赖 {}）",
                edge.from, edge.to
            ));
            continue;
        };
        if !allowed.contains(&edge.to.as_str()) {
            let hint = if allowed.is_empty() {
                "这个 crate 不许依赖任何内部 crate".to_string()
            } else {
                format!("它只允许依赖 {}", allowed.join(", "))
            };
            violations.push(format!(
                "{} -> {}: 违反依赖方向（{}；改 {}）",
                edge.from,
                edge.to,
                hint,
                edge.manifest.display()
            ));
        }
    }

    if violations.is_empty() {
        println!(
            "deps: 0 violation(s)（{} 个 crate，{} 条内部边）",
            allow.len(),
            edges.len()
        );
        return Ok(());
    }

    for v in &violations {
        println!("deps: {v}");
    }
    anyhow::bail!("deps: {} violation(s)", violations.len());
}

/// 扫描 `crates/*/Cargo.toml` 与 `xtask/Cargo.toml`，抽出指向内部 crate 的依赖。
///
/// 只解析 `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` 里的
/// 键名，不看版本、不看 feature —— 方向闸门只需要“谁依赖谁”。
fn collect(root: &Path) -> Result<Vec<Edge>> {
    let mut manifests = Vec::new();
    let crates_dir = root.join("crates");
    for entry in std::fs::read_dir(&crates_dir)
        .with_context(|| format!("列不出 {}", crates_dir.display()))?
    {
        let entry = entry?;
        let manifest = entry.path().join("Cargo.toml");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    let xtask = root.join("xtask").join("Cargo.toml");
    if xtask.is_file() {
        manifests.push(xtask);
    }

    let mut edges = Vec::new();
    for manifest in manifests {
        let text = std::fs::read_to_string(&manifest)?;
        let from = package_name(&text).unwrap_or_else(|| "unknown".to_string());
        for dep in internal_deps(&text) {
            edges.push(Edge {
                from: from.clone(),
                to: dep,
                manifest: manifest.clone(),
            });
        }
    }
    Ok(edges)
}

fn package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// 抽 `[dependencies*]` 段里以 `hbot-` 开头的键。
fn internal_deps(text: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed.starts_with("[dependencies")
                || trimmed.starts_with("[dev-dependencies")
                || trimmed.starts_with("[build-dependencies");
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim().trim_matches('"');
            if key.starts_with("hbot-") {
                deps.push(key.to_string());
            }
        }
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_is_read() {
        let text = "[package]\nname = \"hbot-core\"\nversion = \"0.1.0\"\n";
        assert_eq!(package_name(text).as_deref(), Some("hbot-core"));
    }

    #[test]
    fn internal_deps_only_from_dependency_sections() {
        let text = r#"
[package]
name = "hbot-rpc"

[dependencies]
hbot-core = { path = "../hbot-core" }
serde = "1"

[dev-dependencies]
hbot-workspace = { path = "../hbot-workspace" }

[features]
default = []
"#;
        assert_eq!(
            internal_deps(text),
            vec!["hbot-core".to_string(), "hbot-workspace".to_string()]
        );
    }

    #[test]
    fn every_allowed_target_is_a_real_crate() {
        // ALLOWED 里出现拼错的 crate 名会静默放行不了任何边 —— 这里钉住它。
        let known: Vec<&str> = ALLOWED.iter().map(|(name, _)| *name).collect();
        for (_, allowed) in ALLOWED {
            for dep in *allowed {
                assert!(known.contains(dep), "{dep} 不在 ALLOWED 的 crate 列表里");
            }
        }
    }
}
