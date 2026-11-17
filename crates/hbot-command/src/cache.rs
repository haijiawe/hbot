//! 写隔离档下给**包管理器**用的私有缓存根。
//!
//! 为什么必须有这一层：`workspace-write` 档只允许写工作区（外加这一个缓存根）。
//! 而 `npm` / `pnpm` / `corepack` / `yarn` 一启动就要往 `~/.npm`、`~/.cache` 之类的
//! 家目录路径写缓存；那些路径不在可写范围内，命令会以"文件系统只读"的样子失败。
//! 用户看到的是 `EACCES`/`EROFS`，很容易误判成"隔离坏了"，然后手工改配置绕过去 ——
//! 那才是真正的安全缺口。
//!
//! 做法与 Windows 侧 `acl/confine.ts` 同一套语义：把这一组环境变量指向**同一个**
//! 私有目录，Landlock 规则也只放行这一个目录。两边用的是同一个 [`root`]，
//! 所以"放行的路径"与"告诉包管理器的路径"不可能漂移。
//!
//! 三条例外必须记住：
//!
//! 1. **显式指定的值优先。** 用户在 `CommandSpec.env` 里写了 `npm_config_cache`，
//!    就照用户写的走（他可能故意指到工作区里），本模块不去覆盖；
//! 2. **不设 `CARGO_HOME`。** 它同时装工具链二进制与凭据，指到临时目录会让
//!    `cargo` 重新下载工具链，反而更糟；
//! 3. 这里只影响 `workspace-write` 档。`danger` 档不注入任何东西。

// 产品路径只有 Linux 写隔离用（Windows 的受限令牌在本 crate 里不给缓存目录加 ACE）；
// 非 Linux 上保留编译是为了让纯逻辑单测在本机（Windows）也能跑。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// 指向私有缓存根的环境变量（与 Windows 侧 `confine.ts` 那张表同名）。
pub(crate) const ENV_VARS: &[&str] = &[
    "npm_config_cache",
    "COREPACK_HOME",
    "PNPM_HOME",
    "YARN_CACHE_FOLDER",
    "XDG_CACHE_HOME",
];

/// 私有缓存根的目录名（放在系统临时目录下）。
const ROOT_DIR_NAME: &str = "hbot-command-write-cache";

/// 写隔离档的可写缓存根。
///
/// `<tmpdir>/hbot-command-write-cache`：按用户隔离（`tmpdir` 本身是每用户/每会话的），
/// 不落在工作区里，所以它必须被 [`crate::landlock::plan`] 单独列成一条规则。
pub(crate) fn root() -> PathBuf {
    std::env::temp_dir().join(ROOT_DIR_NAME)
}

/// 一组"把包管理器缓存指到 `root`"的环境变量。
///
/// 注意 `PNPM_HOME` 与 `YARN_CACHE_FOLDER` 的语义并不真的都是"缓存"（前者是 pnpm 的
/// 全局 bin 目录），但受限环境里它们都必须可写，所以一并指到私有根 —— 与 Windows 侧
/// 的做法一致，别在这里单独"修正"其中一个，那会让两个平台的可见行为分叉。
pub(crate) fn env_defaults(root: &Path) -> Vec<(String, String)> {
    ENV_VARS
        .iter()
        .map(|name| ((*name).to_owned(), root.display().to_string()))
        .collect()
}

/// 把缓存默认值**前置**到已有环境变量表里（缺哪个补哪个）。
///
/// 为什么是前置而不是追加：`runner` 最终用 `Command::envs()` 逐对设置，同名后者覆盖前者。
/// 前置让「继承的」与「显式指定的」值仍然赢 —— 用户/宿主环境说了算，本模块只填空白。
/// 这样即使上层宿主已经导出了 `XDG_CACHE_HOME`，我们也不会把它改掉（那可能让别的
/// 工具链找不到自己的缓存），Landlock 那边则仍然按真实值之外的路径拒绝写入。
pub(crate) fn apply_write_cache_defaults(env: &mut Vec<(String, String)>, root: &Path) {
    let mut defaults = env_defaults(root)
        .into_iter()
        .filter(|(name, _)| !env.iter().any(|(existing, _)| existing == name))
        .collect::<Vec<_>>();
    if defaults.is_empty() {
        return;
    }
    defaults.append(env);
    *env = defaults;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_package_manager_variable() {
        let defaults = env_defaults(Path::new("/tmp/hbot-command-write-cache"));
        let names: Vec<&str> = defaults.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ENV_VARS);
        assert!(names.contains(&"npm_config_cache"));
        assert!(names.contains(&"COREPACK_HOME"));
        assert!(names.contains(&"PNPM_HOME"));
        assert!(names.contains(&"YARN_CACHE_FOLDER"));
        assert!(names.contains(&"XDG_CACHE_HOME"));
        // 一个都不许指到家目录：那正是被 Landlock 挡住的地方。
        assert!(!names.contains(&"CARGO_HOME"), "CARGO_HOME 不该被重定向");
        for (_, value) in &defaults {
            assert_eq!(value, "/tmp/hbot-command-write-cache");
        }
    }

    #[test]
    fn explicit_values_win_over_defaults() {
        let mut env = vec![(
            "npm_config_cache".to_owned(),
            "/proj/workspace/.npm".to_owned(),
        )];
        apply_write_cache_defaults(&mut env, Path::new("/tmp/hbot-command-write-cache"));
        let npm = env
            .iter()
            .filter(|(name, _)| name == "npm_config_cache")
            .collect::<Vec<_>>();
        assert_eq!(npm.len(), 1, "同名变量不能出现两次");
        assert_eq!(npm[0].1, "/proj/workspace/.npm", "用户显式给的值优先");
        // 其余变量被补上（顺序在前，但 `.envs()` 里没有同名冲突）。
        assert!(env.iter().any(|(name, _)| name == "COREPACK_HOME"));
    }

    #[test]
    fn inherited_values_still_win_and_are_not_duplicated() {
        // 继承来的 `XDG_CACHE_HOME` 已经在表里（`env.rs` 把继承项放在前面），
        // 那就不该再补一条同名默认值。
        let mut env = vec![
            ("XDG_CACHE_HOME".to_owned(), "/home/me/.cache".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ];
        apply_write_cache_defaults(&mut env, Path::new("/tmp/hbot-command-write-cache"));
        let xdg = env
            .iter()
            .filter(|(name, _)| name == "XDG_CACHE_HOME")
            .collect::<Vec<_>>();
        assert_eq!(xdg.len(), 1);
        assert_eq!(xdg[0].1, "/home/me/.cache");
        assert!(env.iter().any(|(name, _)| name == "PATH"), "原有变量不能丢");
        assert!(env.iter().any(|(name, _)| name == "PNPM_HOME"));
    }

    #[test]
    fn empty_env_gets_only_the_defaults() {
        let mut env: Vec<(String, String)> = Vec::new();
        apply_write_cache_defaults(&mut env, Path::new("/tmp/hbot-command-write-cache"));
        assert_eq!(env.len(), ENV_VARS.len());
        assert_eq!(env[0].0, "npm_config_cache", "缺省项排在最前面");
    }

    #[test]
    fn root_is_absolute_and_named_after_the_crate() {
        let root = root();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(
            root.display().to_string().contains(ROOT_DIR_NAME),
            "{}",
            root.display()
        );
        assert_eq!(
            root.file_name().and_then(|s| s.to_str()),
            Some(ROOT_DIR_NAME)
        );
    }
}
