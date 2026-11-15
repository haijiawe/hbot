//! 打印本机的命令隔离能力矩阵（`cargo run -p hbot-command --example capabilities`）。
//!
//! 这就是"能力矩阵必须显式、启动时探测"那条要求的可执行形态：
//! 组合根在启动时拿到同样的 `CapabilityReport`，据此决定命令工具是否出现在工具表里，
//! 而不是等用户点了才发现跑不了。

use hbot_command::PlatformRunner;
use hbot_core::SandboxMode;

fn main() {
    let runner = PlatformRunner::system();
    let caps = runner.capabilities();

    println!("平台：{}", caps.platform);
    println!("{}", caps.summary_zh());
    println!();
    println!("档位 → 能否跑命令：");
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::Danger,
    ] {
        let verdict = if caps.supports(mode) {
            "可以"
        } else {
            "拒绝"
        };
        println!("  {:<16} {verdict}", mode.as_str());
    }
}
