//! 其它平台（含 macOS）：没有实现，一律拒绝。
//!
//! macOS 上唯一现成的候选是 `sandbox-exec`，它已被 Apple 标记弃用
//! （`docs/02-architecture.md` §11.2 已写明不要用它）。没有可信机制时，
//! 正确的答案是 [`hbot_core::CommandDenied::IsolationUnavailable`]，不是"先跑再说"。
//!
//! 资源上限同理：这一档连进程树隔离都没有，`create` 直接失败，所以 `usage` /
//! `violation` 永远不会被真的用到——它们返回"没测量 / 没超"，而不是假装有限制。

use crate::child::Spawned;
use crate::limits::LimitViolation;
use hbot_core::{ResourceLimits, ResourceUsage};
use std::io;
use std::process::Command;

pub(crate) struct Containment;

impl Containment {
    pub(crate) fn create(_limits: &ResourceLimits) -> Result<Self, String> {
        Err(format!(
            "平台 {} 未提供命令隔离实现（macOS 的 sandbox-exec 已弃用，未采用）",
            hbot_core::Platform::current()
        ))
    }

    pub(crate) fn configure(&self, _command: &mut Command) {}

    pub(crate) fn attach(&self, _child: &Spawned) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "平台未提供命令隔离实现",
        ))
    }

    pub(crate) fn kill_tree(&self, child: &mut Spawned) -> io::Result<()> {
        // 没有隔离范围可杀，退回只杀直接子进程（调用方实际上到不了这里）。
        child.kill()
    }

    pub(crate) fn usage(&self) -> ResourceUsage {
        // 平台没有资源记账可读：0 = 未测量（不是"用了 0"）。
        ResourceUsage::default()
    }

    pub(crate) fn violation(&self, _limits: &ResourceLimits) -> Option<LimitViolation> {
        // `create` 已经失败，这里到不了；真到了也不能凭空报"超限"。
        None
    }
}

pub(crate) fn probe() -> Result<(), String> {
    Err(format!(
        "平台 {} 未提供命令隔离实现（macOS 的 sandbox-exec 已弃用，未采用）",
        hbot_core::Platform::current()
    ))
}
