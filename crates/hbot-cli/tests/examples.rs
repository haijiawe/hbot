//! 示例项目必须一直能跑：`examples/` 下的每个项目都要通过体检。
//!
//! 它钉的是 M7 的出口标准「三类示例项目各一个，全部能在桌面跑通」里**可自动化**
//! 那一半：清单合规、入口能编译、`hbot.d.ts` 与核心契约一致。真起 bot 与平台账号
//! 无关（示例统一用 `platform.kind = local`），但端到端跑消息依赖宿主，留给 e2e。
//!
//! 为什么这条要独立成测试：示例是**用户第一份可抄的代码**，它们漂移的代价不是
//! 编译红，而是新克隆的人照着抄到一份过期用法。`cargo xtask gen --check` 只管
//! `hbot.d.ts`，管不了清单字段与 handler 是否还能编过。

use std::path::{Path, PathBuf};

use hbot_cli::check_project;

/// 仓库根目录（`crates/hbot-cli` 的上两级）。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/hbot-cli 之上应当有仓库根")
        .to_path_buf()
}

/// 每个示例目录：按名字排序，只认含 `hbot.json` 的子目录。
fn example_dirs() -> Vec<PathBuf> {
    let root = repo_root().join("examples");
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(&root).expect("examples/ 应当存在") {
        let path = entry.expect("读取示例目录项").path();
        if path.is_dir() && path.join("hbot.json").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs
}

#[test]
fn the_three_examples_are_present_and_named() {
    let names: Vec<String> = example_dirs()
        .iter()
        .map(|dir| {
            dir.file_name()
                .expect("示例目录应当有名字")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // 名字写死是刻意的：删掉一个示例就必须改这里，免得「三类示例」悄悄变成两类。
    assert_eq!(
        names,
        vec!["chat", "game", "support"],
        "examples/ 下应当是这三类示例"
    );
}

#[test]
fn every_example_passes_the_check() {
    for dir in example_dirs() {
        let report = check_project(&dir);
        assert!(
            report.ok,
            "{} 体检不通过：{:?}",
            dir.display(),
            report.errors
        );
        assert!(
            report.warnings.is_empty(),
            "{} 体检有告警：{:?}",
            dir.display(),
            report.warnings
        );
    }
}

#[test]
fn examples_carry_no_platform_credentials() {
    for dir in example_dirs() {
        // `bots.json` 里有平台 `token` / `clientSecret`，属用户数据（仓库根的
        // `projects/` 正因如此整目录忽略）。示例一旦带上它，凭据就进了版本库。
        assert!(
            !dir.join("bots.json").exists(),
            "{} 不应入库 bots.json（里面有平台凭据）",
            dir.display()
        );
        let manifest = std::fs::read_to_string(dir.join("hbot.json")).expect("读 hbot.json");
        assert!(
            manifest.contains("\"local\""),
            "{} 应当用本地平台，示例不该要求账号",
            dir.display()
        );
    }
}
