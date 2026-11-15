//! 取消令牌。
//!
//! 取消要贯穿 provider / 工具 / HTTP / 命令（`AGENTS.md` §4.2），且**取消后不得
//! 再发起新请求**。这里用 `Arc<AtomicBool>` + [`tokio::sync::Notify`]：
//!
//! - 同步路径（工具执行前、网关每一步）用 [`CancellationToken::check`]；
//! - 异步等待用 [`CancellationToken::cancelled`]；
//! - 需要交给 `hbot_workspace` 搜索（它要 `Arc<AtomicBool>`）时用
//!   [`CancellationToken::flag`] 拿共享句柄。
//!
//! 不用 `tokio_util::sync::CancellationToken`：workspace 没有这个依赖，自己实现
//! 反而少一个传递依赖。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hbot_core::HbotError;
use tokio::sync::Notify;

/// 取消后返回的错误。
///
/// `HbotError` 没有 `Cancelled` 变体（领域错误码固定为 -32001..-32006），
/// 用 `Denied` 表达"动作被主动中止"；调用方按 [`CancellationToken::is_cancelled`]
/// 区分"用户取消"与"策略拒绝"，不要靠错误文本。
pub fn cancelled_error() -> HbotError {
    HbotError::denied("操作已取消：取消后不再发起新的请求")
}

/// 可克隆的取消令牌；克隆共享同一状态。
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    /// 新建未取消的令牌。
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记取消并唤醒所有等待者。重复调用是幂等的。
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 是否已取消。
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 共享的原子标志（给只需要 `Arc<AtomicBool>` 的接口，如 workspace 搜索）。
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// 已取消则返回 [`cancelled_error`]。
    pub fn check(&self) -> Result<(), HbotError> {
        if self.is_cancelled() {
            Err(cancelled_error())
        } else {
            Ok(())
        }
    }

    /// 等到取消发生；已取消时立即返回。
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            // 先注册再检查，避免"检查后、等待前"丢唤醒。
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        assert!(token.check().is_ok());
    }

    #[test]
    fn cancel_is_sticky_and_shared_across_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
        assert!(token.check().is_err());
        assert!(token.flag().load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_wakes_waiters() {
        let token = CancellationToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        token.cancel();
        handle.await.expect("wait task should finish");
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancelled().await;
    }
}
