//! 能力探测：这台机器上到底能不能建立某一层隔离。
//!
//! 为什么单独抽一个 trait：产品原则是"装不上就拒绝"，而"装不上"必须能被**测**。
//! 把探测做成可注入的依赖（[`IsolationProbe`]），测试就能构造一台"什么隔离都没有"
//! 的机器，断言得到 [`hbot_core::CommandDenied::IsolationUnavailable`] 而不是放行
//! （`crates/hbot-command/tests/isolation.rs` 的 `isolation_unavailable_is_denied_not_allowed`）。

use std::fmt;

use hbot_core::{Platform, SandboxMode};
use serde::{Deserialize, Serialize};

use crate::{confinement, containment};

/// 隔离层级。
///
/// 分成两层是刻意的：它们的代价、可用平台和"挡得住什么"完全不同，混成一个布尔值
/// 会让调用方以为"有隔离"就等于"写也安全了"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// 进程树 + 资源约束：杀掉整棵树、限制活跃进程数与内存。
    ///
    /// 挡不住写、网络、注册表。Windows 用作业对象；Linux 用 `setsid` + `killpg`。
    Containment,

    /// 写隔离：被跑起来的程序只能写显式放行的目录。
    ///
    /// Windows 上是受限令牌（`WRITE_RESTRICTED` + 由工作区路径派生的 write SID + 该目录上的
    /// ACE）；Linux 上是 Landlock（**只编译过，未在 Linux 上运行**）；macOS 上的
    /// `sandbox-exec` 已弃用，不作为候选，探测恒失败。
    WriteConfinement,
}

impl IsolationLevel {
    /// 稳定字符串（日志与能力矩阵用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Containment => "containment",
            Self::WriteConfinement => "write_confinement",
        }
    }
}

impl fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一层的探测结果：可用与否 + 一句人能读的说明。
///
/// 说明文字会被拼进给用户的拒绝消息，所以它必须写"为什么"和"差什么"，
/// 不能是 `EPERM` 这种只有开发者看得懂的原文（系统错误原文另外附在括号里）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    /// 这一层是否可用。
    pub available: bool,
    /// 说明（中文，短句）。
    pub detail: String,
}

impl ProbeOutcome {
    /// 可用。
    pub fn available(detail: impl Into<String>) -> Self {
        Self {
            available: true,
            detail: detail.into(),
        }
    }

    /// 不可用。
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            available: false,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ProbeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.available {
            "可用"
        } else {
            "不可用"
        })?;
        write!(f, "：{}", self.detail)
    }
}

/// 隔离能力探测端口（注入点）。
///
/// 实现必须是**不自作主张的探测**：不许为探测去跑调用方要求的命令、不许改系统设置
/// （临时目录里的 ACE 往返可以，但必须当场收拾干净）、不许拿探测结果去"降级放行"。
///
/// 唯一被允许起进程的情形是**验证令牌本身**：Windows 上 `CreateRestrictedToken` 成功
/// 并不代表 `CreateProcessAsUserW` 起得来（缺 `SeAssignPrimaryTokenPrivilege` 时要
/// 1314），而这一点只有真起一个立刻退出的系统程序才知道。所以写隔离的探测确实会起
/// `cmd.exe /c exit 0` —— 它不碰用户数据、不写工作区，是整个 crate 里唯一一条
/// "探测期间起进程"的路径。
///
/// 返回 `Err(reason)` 表示该层不可用，`reason` 直接进 [`hbot_core::CommandDenied::IsolationUnavailable`]。
pub trait IsolationProbe: Send + Sync {
    /// 探测某一层是否可用；`Err` 是给用户看的原因。
    fn probe(&self, level: IsolationLevel) -> Result<(), String>;
}

/// 真实探测：按当前平台问操作系统"这套机制能不能建起来"。
///
/// Windows 的 Containment 探测会真的建一个空作业对象并设上限再关掉；WriteConfinement 探测
/// 会建一个受限令牌、在一处临时目录上跑一遍 ACE 往返（写完立刻删）并用它起一个
/// `cmd.exe /c exit 0` —— 加起来仍是毫秒级，不跑 `powershell`（旧项目每条命令多花 1.5 s
/// 的地板价就是那么来的，`AGENTS.md` §5 第 9 条第①点）。结果按进程缓存。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProbe;

impl IsolationProbe for SystemProbe {
    fn probe(&self, level: IsolationLevel) -> Result<(), String> {
        probe_platform(Platform::current(), level)
    }
}

fn probe_platform(platform: Platform, level: IsolationLevel) -> Result<(), String> {
    match (platform, level) {
        (Platform::Windows, IsolationLevel::Containment) => containment::probe(),
        (Platform::Windows, IsolationLevel::WriteConfinement) => confinement::probe(),
        (Platform::Linux, IsolationLevel::Containment) => containment::probe(),
        (Platform::Linux, IsolationLevel::WriteConfinement) => confinement::probe(),
        // macOS：两层都没有实现（`sandbox-exec` 已弃用），探测如实失败。
        (Platform::MacOs, _) => {
            Err("macOS 上 sandbox-exec 已弃用，本 crate 不提供命令隔离实现".to_owned())
        }
        (Platform::Other, _) => Err(format!("平台 {platform} 未提供命令隔离实现")),
    }
}

/// 三平台能力矩阵（可序列化，给 RPC / UI / 审计用）。
///
/// 这张表就是产品对用户的承诺：某一档能不能跑。**不要在 UI 里现算**，否则
/// "能点但会失败"的按钮会回来（`docs/02-architecture.md` §11.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityReport {
    /// 探测时的平台。
    pub platform: Platform,
    /// 进程树 + 资源约束。
    pub containment: ProbeOutcome,
    /// 写隔离。
    pub write_confinement: ProbeOutcome,
}

impl CapabilityReport {
    /// 用给定探测实现生成一份矩阵。
    pub fn probe_with(probe: &dyn IsolationProbe, platform: Platform) -> Self {
        Self {
            platform,
            containment: outcome_of(probe.probe(IsolationLevel::Containment)),
            write_confinement: outcome_of(probe.probe(IsolationLevel::WriteConfinement)),
        }
    }

    /// 用真实平台探测生成矩阵。
    pub fn system() -> Self {
        Self::probe_with(&SystemProbe, Platform::current())
    }

    /// 这一档沙箱能不能跑命令。
    ///
    /// `read-only` 恒为 `false`：只读档下命令连"跑起来"都不该发生
    /// （`AGENTS.md` §7 第 8 条那条"只读档硬拒"）。
    pub fn supports(&self, mode: SandboxMode) -> bool {
        match mode {
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self.write_confinement.available,
            SandboxMode::Danger => self.containment.available,
        }
    }

    /// 这一档需要哪一层隔离。
    pub fn required_level(mode: SandboxMode) -> Option<IsolationLevel> {
        match mode {
            SandboxMode::ReadOnly => None,
            SandboxMode::WorkspaceWrite => Some(IsolationLevel::WriteConfinement),
            SandboxMode::Danger => Some(IsolationLevel::Containment),
        }
    }

    /// 给用户看的一句中文总结（能力矩阵面板 / 拒绝横幅用）。
    ///
    /// 形如「命令隔离能力（windows）：进程树与资源上限可用；写隔离不可用（…）」。
    pub fn summary_zh(&self) -> String {
        format!(
            "命令隔离能力（{}）：进程树与资源上限{}；写隔离{}",
            self.platform,
            phrase(&self.containment),
            phrase(&self.write_confinement)
        )
    }
}

/// 把探测结果拼成一句话：可用就只说"可用"，不可用要带上原因。
fn phrase(outcome: &ProbeOutcome) -> String {
    if outcome.available {
        "可用".to_owned()
    } else {
        format!("不可用（{}）", outcome.detail)
    }
}

fn outcome_of(result: Result<(), String>) -> ProbeOutcome {
    match result {
        Ok(()) => ProbeOutcome::available("可用"),
        Err(reason) => ProbeOutcome::unavailable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DenyAll;

    impl IsolationProbe for DenyAll {
        fn probe(&self, level: IsolationLevel) -> Result<(), String> {
            Err(format!("{level} 在测试里被注入为不可用"))
        }
    }

    #[test]
    fn report_reflects_injected_probe() {
        let report = CapabilityReport::probe_with(&DenyAll, Platform::Linux);
        assert!(!report.containment.available);
        assert!(!report.write_confinement.available);
        assert!(!report.supports(SandboxMode::Danger));
        assert!(!report.supports(SandboxMode::WorkspaceWrite));
        assert!(!report.supports(SandboxMode::ReadOnly));
        assert!(report.summary_zh().contains("不可用"));
    }

    #[test]
    fn read_only_never_supports_commands() {
        let report = CapabilityReport {
            platform: Platform::Windows,
            containment: ProbeOutcome::available("ok"),
            write_confinement: ProbeOutcome::available("ok"),
        };
        assert!(
            !report.supports(SandboxMode::ReadOnly),
            "只读档恒不放行命令"
        );
        assert!(report.supports(SandboxMode::Danger));
        assert_eq!(
            CapabilityReport::required_level(SandboxMode::ReadOnly),
            None
        );
    }

    #[test]
    fn probe_outcome_display_is_readable() {
        let available = ProbeOutcome::available("作业对象可用");
        assert_eq!(available.to_string(), "可用：作业对象可用");
        let unavailable = ProbeOutcome::unavailable("landlock 未接入");
        assert!(unavailable.to_string().starts_with("不可用"));
    }
}
