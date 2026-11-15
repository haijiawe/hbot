//! 环境变量白名单：默认清空，只传显式点名的变量。
//!
//! 为什么默认**清空**而不是"继承后删几个"：继承是"默认开放"，
//! 每加一个危险变量就要补一条黑名单，漏一个就是一次凭据外泄
//! （父进程环境里通常有 API key、token、代理口令）。这里是白名单：
//! 没点名就没有。
//!
//! 为什么还要单独拦加载器注入类变量：`LD_PRELOAD` / `DYLD_INSERT_LIBRARIES`
//! 一类可以让"被约束的程序"在自己的进程里加载任意代码——即便只做资源约束，
//! 它们也会把隔离层的假设打穿（比如注入一个改 `argv` 的 shim）。

use hbot_core::{CommandDenied, CommandSpec};

/// 加载器注入类变量（大小写不敏感比较，因为 Windows 的环境变量名不区分大小写）。
const LOADER_INJECTION_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
];

/// 是否加载器注入类变量。
pub(crate) fn is_loader_injection(name: &str) -> bool {
    LOADER_INJECTION_VARS
        .iter()
        .any(|forbidden| forbidden.eq_ignore_ascii_case(name))
}

/// 构造要传给子进程的环境变量（键值对，顺序：白名单继承在前、显式值在后覆盖）。
pub(crate) fn build_env(spec: &CommandSpec) -> Result<Vec<(String, String)>, CommandDenied> {
    if spec.inherit_entire_env {
        return Err(CommandDenied::InheritEnvRejected {
            vars: "*".to_owned(),
        });
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    for name in &spec.inherit_env {
        if is_loader_injection(name) {
            return Err(CommandDenied::InheritEnvRejected { vars: name.clone() });
        }
        // 父进程里没有这个变量就跳过（不报错）：白名单是"允许"，不是"要求"。
        if let Ok(value) = std::env::var(name) {
            pairs.push((name.clone(), value));
        }
    }
    for (name, value) in &spec.env {
        if is_loader_injection(name) {
            return Err(CommandDenied::InheritEnvRejected { vars: name.clone() });
        }
        pairs.push((name.clone(), value.clone()));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_injection_names_are_rejected() {
        assert!(is_loader_injection("LD_PRELOAD"));
        assert!(is_loader_injection("ld_preload"));
        assert!(is_loader_injection("DYLD_INSERT_LIBRARIES"));
        assert!(!is_loader_injection("PATH"));
    }

    #[test]
    fn inherit_entire_env_is_rejected() {
        let mut spec = CommandSpec::new("cargo", "C:\\proj");
        spec.inherit_entire_env = true;
        let denied = build_env(&spec).expect_err("整份继承必须被拒绝");
        assert!(matches!(
            denied,
            CommandDenied::InheritEnvRejected { ref vars } if vars == "*"
        ));
    }

    #[test]
    fn explicit_loader_injection_is_rejected() {
        let mut spec = CommandSpec::new("cargo", "C:\\proj");
        spec.env = vec![("LD_PRELOAD".to_owned(), "/tmp/evil.so".to_owned())];
        assert!(matches!(
            build_env(&spec),
            Err(CommandDenied::InheritEnvRejected { .. })
        ));
    }

    #[test]
    fn empty_spec_yields_empty_env() {
        let spec = CommandSpec::new("cargo", "C:\\proj");
        assert!(
            build_env(&spec).expect("空规格").is_empty(),
            "默认不继承任何变量"
        );
    }

    #[test]
    fn explicit_values_come_after_inherited_ones() {
        // 父进程里一定有 PATH（Windows/Unix 都有）；显式给一个同名值应排在后面覆盖它。
        let mut spec = CommandSpec::new("cargo", "C:\\proj");
        spec.inherit_env = vec!["PATH".to_owned()];
        spec.env = vec![("PATH".to_owned(), "C:\\fixed".to_owned())];
        let pairs = build_env(&spec).expect("白名单 + 显式值");
        let last_path = pairs
            .iter()
            .rfind(|(name, _)| name == "PATH")
            .expect("至少一条 PATH");
        assert_eq!(last_path.1, "C:\\fixed", "显式值必须覆盖继承值");
    }
}
