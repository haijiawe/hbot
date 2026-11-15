//! `hbot check <project-dir>`：把项目从磁盘一路"编译到可运行"再回答。
//!
//! 检查顺序就是启动顺序，第一个失败点即停止后续步骤（级联错误对作者没用）：
//! 目录可用 → `hbot.json` 能解析且合规（含 `entry` 存在）→ 整个 `src/**` 能过
//! SWC 去类型并链接 import 图 → 项目里的 `hbot.d.ts` 与核心生成的一致。
//!
//! 输出仍是机器可读的 `{"ok":…,"errors":[…]}`（[`CheckReport`] 的形状是 CLI
//! 契约，`tests/cli.rs` 精确断言它，不因为"加了检查项"而改形状）；每一项的
//! `ok / error` 明细经 `tracing` 打到 stderr，stdout 只留一行 JSON。
//!
//! 只读：不写 `.hbot/`、不碰数据库、不启动 isolate。

use std::path::Path;

use anyhow::Context as _;
use hbot_code::CodeError;
use hbot_code::compile;
use hbot_code::dts;
use hbot_code::manifest;
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

/// 对一个项目目录做体检。
///
/// 目录不存在时只报一条（不再级联报 `hbot.json` 缺失，避免噪音）。
#[must_use]
pub fn check_project(dir: &Path) -> CheckReport {
    let mut errors = Vec::new();

    match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            errors.push(format!("项目路径不是目录：{}", dir.display()));
            return CheckReport { ok: false, errors };
        }
        Err(error) => {
            errors.push(format!("项目目录不可用：{}（{error}）", dir.display()));
            return CheckReport { ok: false, errors };
        }
    }
    tracing::info!(check = "项目目录", ok = true, path = %dir.display());

    // 1. 清单：字段、取值范围、entry 是否在项目内且存在，全在 `manifest::load` 里。
    let loaded = match manifest::load(dir) {
        Ok(manifest) => {
            tracing::info!(
                check = "hbot.json",
                ok = true,
                name = %manifest.name,
                entry = %manifest.entry_name(),
                platform = manifest.platform.kind.as_str(),
            );
            Some(manifest)
        }
        Err(error) => {
            tracing::error!(
                check = "hbot.json",
                ok = false,
                code = error.code(),
                detail = %error,
            );
            errors.push(error.to_string());
            None
        }
    };

    // 2. 编译 + 链接。编译器一次只返回第一个错误（`docs/02-architecture.md` §6.2:
    //    逐文件报，但 `compile_project` 是 fail-fast），所以这里只可能多一条。
    if let Some(manifest) = &loaded {
        let entry = manifest.entry_name();
        match compile::compile_project_with_entry(dir, &entry) {
            Ok(project) => {
                let stats = project.stats();
                tracing::info!(
                    check = "编译",
                    ok = true,
                    entry = %project.entry(),
                    files = stats.files,
                    emitted_bytes = stats.emitted_bytes,
                );
            }
            Err(error) => {
                report_compile_error(&error);
                errors.push(error.to_string());
            }
        }
    }

    // 3. `hbot.d.ts` 必须与核心生成的契约一致；手改过的 d.ts 会让 Monaco 里的
    //    补全与运行时行为对不上，这属于"会骗 Agent"的错误，必须报。
    if loaded.is_some() {
        match dts::is_host_dts_current(dir) {
            Ok(true) => tracing::info!(check = "hbot.d.ts", ok = true),
            Ok(false) => {
                let message = format!(
                    "{} 缺失或与核心生成的契约不一致，请用 hbot-code 重新生成",
                    dts::HOST_DTS_FILE
                );
                tracing::error!(check = "hbot.d.ts", ok = false, detail = %message);
                errors.push(message);
            }
            Err(error) => {
                tracing::error!(check = "hbot.d.ts", ok = false, detail = %error);
                errors.push(error.to_string());
            }
        }
    }

    CheckReport {
        ok: errors.is_empty(),
        errors,
    }
}

/// 编译失败时把文件/行列单独打一条，方便 IDE 抓取（`HBOT_*` 前缀已经在 Display 里）。
fn report_compile_error(error: &CodeError) {
    let position = error.position();
    tracing::error!(
        check = "编译",
        ok = false,
        code = error.code(),
        file = position.as_ref().map(|pos| pos.file.as_str()).unwrap_or_default(),
        line = position.as_ref().map(|pos| pos.line).unwrap_or_default(),
        column = position.as_ref().map(|pos| pos.column).unwrap_or_default(),
        detail = %error,
    );
}
