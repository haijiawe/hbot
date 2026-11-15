//! `hbot check <project-dir>`：**占位**体检。
//!
//! 只回答两件事：目录在不在、`hbot.json` 在不在。不读数据库、不碰 agent/bot crate，
//! 输出是机器可读的 `{"ok":…,"errors":[…]}`。

use std::path::Path;

use anyhow::Context as _;
use serde::Serialize;

/// 体检结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    /// 全部检查通过。
    pub ok: bool,
    /// 失败原因（人可读，已排序稳定：按检查顺序）。
    pub errors: Vec<String>,
}

impl CheckReport {
    /// 序列化成单行 JSON。
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).context("序列化 check 结果失败")
    }
}

/// 对项目目录做占位体检。
///
/// 目录不存在时只报一条（不再级联报 `hbot.json` 缺失，避免噪音）。
pub fn check_project(dir: &Path) -> CheckReport {
    let mut errors = Vec::new();

    match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {
            let manifest = dir.join("hbot.json");
            match std::fs::metadata(&manifest) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => errors.push(format!("hbot.json 不是文件：{}", manifest.display())),
                Err(error) => {
                    errors.push(format!("缺少 hbot.json：{}（{error}）", manifest.display()))
                }
            }
        }
        Ok(_) => errors.push(format!("项目路径不是目录：{}", dir.display())),
        Err(error) => errors.push(format!("项目目录不可用：{}（{error}）", dir.display())),
    }

    CheckReport {
        ok: errors.is_empty(),
        errors,
    }
}
