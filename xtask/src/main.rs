//! 仓库维护命令。`cargo xtask <cmd>`。
//!
//! 只放开发期工具：代码生成、依赖方向校验、基准。运行时逻辑不放这里。

mod deps;

use std::path::{Path, PathBuf};

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

/// 生成两类产物：UI 契约（`apps/web/src/generated/`）与示例项目的 `hbot.d.ts`。
///
/// `--check` 只比对不写盘，供 CI 用：代码改了但没重新生成就必须红。
///
/// 示例项目也走这一条：`examples/*/hbot.d.ts` 是**每个项目自己**的副本，
/// 没有代码生成就会随宿主 API 漂移，而漂移的后果是 Monaco 里的补全与运行时
/// 行为对不上（`hbot check` 在用户打开项目时才报，太晚）。
fn generate(check: bool) -> Result<()> {
    let root = workspace_root();
    let dir = root.join("apps").join("web").join("src").join("generated");

    if check {
        let count = hbot_rpc::ts::check_typescript(&dir)
            .context("生成的 TypeScript 契约与 Rust 契约不一致")?;
        println!("gen --check: {count} 个文件一致（{}）", dir.display());
        check_example_dts(&root)?;
        return Ok(());
    }

    let count = hbot_rpc::ts::export_typescript(&dir)
        .with_context(|| format!("生成 TypeScript 契约到 {} 失败", dir.display()))?;
    println!("gen: 生成 {count} 个文件到 {}", dir.display());
    write_example_dts(&root)?;
    Ok(())
}

/// `examples/` 下的项目目录（只认含 `hbot.json` 的那几个）。
///
/// 不写死三个名字：加一个示例不该需要改代码生成器。目录不存在时返回空 ——
/// 有人把示例删光也不该让闸门崩。
fn example_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let examples = root.join("examples");
    let entries = match std::fs::read_dir(&examples) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取 {} 失败", examples.display()));
        }
    };

    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry.context("读取示例目录项失败")?.path();
        if path.is_dir() && path.join("hbot.json").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// 刷新每个示例项目的 `hbot.d.ts`。
fn write_example_dts(root: &Path) -> Result<()> {
    for dir in example_dirs(root)? {
        let written = hbot_code::dts::write_host_dts(&dir)
            .with_context(|| format!("写 {} 失败", dir.display()))?;
        println!("gen: 生成 {}", written.display());
    }
    Ok(())
}

/// 比对每个示例项目的 `hbot.d.ts` 与核心契约。
fn check_example_dts(root: &Path) -> Result<()> {
    let mut stale = Vec::new();
    let mut count = 0usize;
    for dir in example_dirs(root)? {
        count += 1;
        let current = hbot_code::dts::is_host_dts_current(&dir)
            .with_context(|| format!("读 {} 失败", dir.display()))?;
        if !current {
            stale.push(dir.join(hbot_code::dts::HOST_DTS_FILE));
        }
    }

    if !stale.is_empty() {
        let list = stale
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("、");
        anyhow::bail!("{list} 与核心契约不一致，请跑 `cargo xtask gen`");
    }
    println!("gen --check: {count} 个示例的 hbot.d.ts 一致");
    Ok(())
}
/// 仓库根目录（`xtask` 的上一级）。
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
