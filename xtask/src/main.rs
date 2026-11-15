//! 仓库维护命令。`cargo xtask <cmd>`。
//!
//! 只放开发期工具：代码生成、依赖方向校验、基准。运行时逻辑不放这里。

mod deps;

use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    match cmd.as_str() {
        "deps" => deps::run(),
        "gen" => generate(args.any(|a| a == "--check")),
        _ => {
            println!("usage: cargo xtask <deps|gen [--check]>");
            Ok(())
        }
    }
}

/// 生成 UI 契约（`apps/web/src/generated/`）。
///
/// `--check` 只比对不写盘，供 CI 用：代码改了但没重新生成就必须红。
fn generate(check: bool) -> Result<()> {
    let dir = workspace_root()
        .join("apps")
        .join("web")
        .join("src")
        .join("generated");

    if check {
        let count = hbot_rpc::ts::check_typescript(&dir)
            .context("生成的 TypeScript 契约与 Rust 契约不一致")?;
        println!("gen --check: {count} 个文件一致（{}）", dir.display());
        return Ok(());
    }

    let count = hbot_rpc::ts::export_typescript(&dir)
        .with_context(|| format!("生成 TypeScript 契约到 {} 失败", dir.display()))?;
    println!("gen: 生成 {count} 个文件到 {}", dir.display());
    Ok(())
}

/// 仓库根目录（`xtask` 的上一级）。
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
