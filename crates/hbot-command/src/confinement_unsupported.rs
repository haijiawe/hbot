//! macOS 等平台的写隔离：**明确不实现**。
//!
//! macOS 上的进程写隔离曾经只有 `sandbox-exec`（Seatbelt）一条路，而它已被 Apple
//! 标记弃用、没有等价的公开替代 API。拿一个随时可能消失、且没有稳定语义的机制
//! 当安全边界，等于给用户一个"看起来被隔离了"的假象 —— 所以这里**一律返回失败**，
//! `WorkspaceWrite` 档在这些平台上保持拒绝（fail-closed，见 ADR 0003 §4）。
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// 占位类型：本平台永远不会成功构造。
pub(crate) struct WriteConfinement;

impl WriteConfinement {
    pub(crate) fn create(_workspace_root: &Path) -> Result<Self, String> {
        Err(reason())
    }

    pub(crate) fn configure(&self, _command: &mut Command) {}
}

pub(crate) fn probe() -> Result<(), String> {
    Err(reason())
}

fn reason() -> String {
    format!(
        "平台 {} 未提供写隔离实现（macOS 的 sandbox-exec 已弃用，未采用）",
        std::env::consts::OS
    )
}
