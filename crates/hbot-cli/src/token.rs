//! `hbot token`：生成认证 token，或把它写成 systemd 的 `EnvironmentFile`。
//!
//! 为什么不让人自己 `openssl rand` 一句：token 的形状（长度、字符集）只有一处真源，
//! 而这句真源就在 [`crate::serve::generate_token`]（`hbot serve` 自己生成 token 时用的
//! 也是它）。写在文档里的 `openssl` 命令迟早会和代码漂移。
//!
//! 输出约定（与其它子命令一致）：
//!
//! - 不给 `--write`：stdout 只打 token 本身（一行），方便 `HBOT_TOKEN=$(hbot token)`；
//! - 给了 `--write`：token 写进文件、**stdout 只打文件路径** —— token 不该留在终端
//!   回滚与 CI 日志里。文件内容是 `HBOT_TOKEN=<token>` 一行，systemd 的
//!   `EnvironmentFile=` 直接就能读。

use std::path::Path;

use anyhow::Context as _;
use tracing::info;

use crate::Outcome;
use crate::cli::TokenArgs;
use crate::serve::generate_token;

/// 环境变量名（`hbot serve` 读的就是它）。
pub const TOKEN_ENV: &str = "HBOT_TOKEN";

/// 执行 `hbot token`。
pub fn run(args: &TokenArgs) -> anyhow::Result<Outcome> {
    let token = generate_token();

    match &args.write {
        None => {
            crate::write_line(&token)?;
        }
        Some(path) => {
            write_env_file(path, &token)?;
            // 路径走 stdout（可被脚本消费）；token 只进文件。
            crate::write_line(&path.display().to_string())?;
        }
    }

    Ok(Outcome::Success)
}

/// 把 token 写成 `HBOT_TOKEN=…` 一行。
///
/// 覆盖写是刻意的：轮换 token 的动作就是"再跑一次 `hbot token --write`"。
/// 文件权限交给调用方（systemd 的 `EnvironmentFile` 通常由管理员用 0600 放置）；
/// 这里至少**不**把 token 打到日志里。
pub fn write_env_file(path: &Path, token: &str) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }

    let contents = format!("{TOKEN_ENV}={token}\n");
    std::fs::write(path, contents)
        .with_context(|| format!("写 token 文件失败：{}", path.display()))?;

    info!(path = %path.display(), "token 已写入 EnvironmentFile");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hbot-token-{label}-{}-{}.env",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn the_written_file_is_exactly_what_systemd_reads() {
        let path = temp_path("env");
        let token = generate_token();
        write_env_file(&path, &token).expect("写 token 文件");

        let text = std::fs::read_to_string(&path).expect("读回 token 文件");
        assert_eq!(
            text,
            format!("HBOT_TOKEN={token}\n"),
            "文件形状必须是 EnvironmentFile 的一行"
        );
        assert!(
            !text.contains("\r"),
            "不能有 CR：systemd 会把 \\r 当成 token 的一部分"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_creates_the_parent_directory() {
        let dir = std::env::temp_dir().join(format!(
            "hbot-token-dir-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let path = dir.join("etc").join("token.env");

        write_env_file(&path, "abc").expect("写 token 文件");
        assert!(path.exists(), "父目录必须被创建出来");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
