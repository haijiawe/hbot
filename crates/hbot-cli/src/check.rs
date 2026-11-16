//! `hbot check <project-dir>`：把项目从磁盘一路"编译到可运行"再回答。
//!
//! 检查顺序就是启动顺序，第一个失败点即停止后续步骤（级联错误对作者没用）：
//! 目录可用 → `hbot.json` 能解析且合规（含 `entry` 存在）→ 整个 `src/**` 能过
//! SWC 去类型并链接 import 图 → 项目里的 `hbot.d.ts` 与核心生成的一致。
//!
//! 输出是机器可读的 `{"ok":…,"errors":[…],"warnings":[…]}`（[`CheckReport`] 的
//! 形状是 CLI 契约，`tests/cli.rs` 精确断言它）；**`ok` 只看 `errors`**，`warnings`
//! 是"能跑、但作者该知道"的提示（平台取值未被本版本识别、或在不需要 `ws` 的平台
//! 上写了 `ws`），不改变退出码。每一项的 `ok / error` 明细经 `tracing` 打到
//! stderr，stdout 只留一行 JSON。
//!
//! 只读：不写 `.hbot/`、不碰数据库、不启动 isolate。

use std::path::Path;

use anyhow::Context as _;
use hbot_code::CodeError;
use hbot_code::compile;
use hbot_code::dts;
use hbot_code::manifest::{self, PlatformSpec};
use serde::Serialize;

/// 本版本认识、且 `bots.json` 的 `platform` 可以写的平台标识。
///
/// 这份名单**只属于 CLI**：`hbot-code` 里 `platform.kind` 是开放字符串，核心库
/// 不知道产品上有哪些平台（每加一个平台就改一次核心库，依赖方向就反了）。识别
/// 哪些平台是面向用户的入口——也就是 `hbot check`——的职责。
pub const KNOWN_PLATFORMS: [&str; 2] = [manifest::ONE_BOT_KIND, "qq_official"];

/// 体检结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    /// 全部检查通过（只看 `errors`）。
    pub ok: bool,
    /// 失败原因（人可读，已排序稳定：按检查顺序）。
    pub errors: Vec<String>,
    /// 非致命提示（不影响 `ok` 与退出码）。
    pub warnings: Vec<String>,
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
    let mut warnings = Vec::new();

    match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            errors.push(format!("项目路径不是目录：{}", dir.display()));
            return CheckReport {
                ok: false,
                errors,
                warnings,
            };
        }
        Err(error) => {
            errors.push(format!("项目目录不可用：{}（{error}）", dir.display()));
            return CheckReport {
                ok: false,
                errors,
                warnings,
            };
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
                platform = %manifest.platform.kind,
                ws = manifest.platform.ws.as_deref().unwrap_or("-"),
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

    // 2. 平台块的语义告警：**不是错误**。`kind` 在核心侧是开放字符串（清单只是
    //    元数据，真正的连接配置在 `bots.json`），所以"本版本不认识这个平台"只值得
    //    一句提示；非 `onebot11` 平台上的 `ws` 是手写草稿的残留，会被忽略，也不该
    //    让体检失败。`onebot11` 缺 `ws` 是硬错误，由 `manifest::load` 在上一段就
    //    拦下来了——这里不需要第二份判据。
    if let Some(manifest) = &loaded {
        for warning in platform_warnings(&manifest.platform) {
            tracing::warn!(check = "平台", detail = %warning);
            warnings.push(warning);
        }
    }

    // 3. 编译 + 链接。编译器一次只返回第一个错误（`docs/02-architecture.md` §6.2:
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

    // 4. `hbot.d.ts` 必须与核心生成的契约一致；手改过的 d.ts 会让 Monaco 里的
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
        warnings,
    }
}

/// 平台块的语义告警（都不改变 `ok` 与退出码）。
///
/// * `kind` 不在 [`KNOWN_PLATFORMS`] 里：清单能解析、项目能编译，只是**本版本**
///   不认识这个平台。`bots.json` 里的 `platform` 才是运行时认的那个字段，所以
///   这里直接给出合法取值，别让作者猜。
/// * 非 `onebot11` 平台写了 `ws`：该字段会被忽略（QQ 官方的地址由 `/gateway`
///   发现）。手写草稿允许带着它，提示一句即可。
fn platform_warnings(platform: &PlatformSpec) -> Vec<String> {
    let mut warnings = Vec::new();

    if !KNOWN_PLATFORMS.contains(&platform.kind.as_str()) {
        warnings.push(format!(
            "平台 \"{}\" 未被本版本识别；bots.json 里的 platform 必须写 \"onebot11\" 或 \"qq_official\"",
            platform.kind
        ));
    }

    if platform.kind != manifest::ONE_BOT_KIND && platform.ws.is_some() {
        if platform.kind == "qq_official" {
            warnings.push(
                "平台 \"qq_official\" 不需要 ws（地址由 /gateway 发现），该字段会被忽略".to_owned(),
            );
        } else {
            warnings.push(format!(
                "平台 \"{}\" 不需要 ws，该字段会被忽略",
                platform.kind
            ));
        }
    }

    warnings
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
