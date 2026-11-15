//! 存储调用的桥：把同步的 `hbot-store` 放进 `spawn_blocking`，并统一错误转换。
//!
//! `hbot-store` 的公开 API 全是同步阻塞的（`rusqlite` 没有 async 接口），直接在下文
//! 的 async 任务里调用会占住 tokio worker（表现为 handler 停顿、心跳超时）。所以
//! **本 crate 里每一次 store 调用都经 [`run_store`]**，没有例外。
//!
//! 错误映射：`hbot-store` 的错误没有领域语义（SQLite 文本、写线程已退出等），统一折成
//! [`HbotError::Internal`] 并保留原始 `Display` 文本；只有会话插入的 UNIQUE 冲突是
//! “已经存在”而非失败，见 [`is_conversation_conflict`]。

use hbot_core::HbotError;
use hbot_store::{Result as StoreResult, StoreError};

/// 把一次同步存储操作包进 `spawn_blocking`。
///
/// `context` 只用于错误信息（例如 `"insert_inbound"`），失败时不会静默吞掉。
pub(crate) async fn run_store<T, F>(context: &'static str, operation: F) -> Result<T, HbotError>
where
    T: Send + 'static,
    F: FnOnce() -> StoreResult<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(store_error(context, error)),
        // 阻塞任务 panic：存储层是 `deny(unsafe_code)` 的纯 Rust，走到这里说明 bug，
        // 按内部错误上报，不再往下猜。
        Err(join_error) => Err(HbotError::internal(format!(
            "store {context} blocking task failed: {join_error}"
        ))),
    }
}

/// 把 [`StoreError`] 折成领域错误。原始文本保留，便于 UI 与日志定位。
pub(crate) fn store_error(context: &'static str, error: StoreError) -> HbotError {
    HbotError::internal(format!("store {context} failed: {error}"))
}

/// 是否是“会话已存在”造成的插入冲突（折成 [`HbotError`] 之后判定）。
///
/// 存储层没有“按 `(bot_id, external_id)` 读会话”的 API，所以这里依赖 `insert_conversation`
/// 的错误文本：必须同时命中 UNIQUE 约束与 `conversations` 表名，避免把 inbound/outbound
/// 的冲突误判成“会话已存在”（那会吞掉真正的 bug）。
pub(crate) fn is_conversation_conflict(error: &HbotError) -> bool {
    match error {
        HbotError::Internal { message } => {
            message.contains("insert_conversation")
                && message.contains("UNIQUE constraint failed")
                && message.contains("conversations")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_conflict_requires_table_and_constraint() {
        let conflict = HbotError::internal(
            "store insert_conversation failed: sqlite error: UNIQUE constraint failed: conversations.id",
        );
        assert!(is_conversation_conflict(&conflict));

        // 没有 `insert_conversation` 上下文 → 不是会话冲突（比如 inbound 行重复）。
        let inbound = HbotError::internal(
            "store insert_inbound_if_new failed: sqlite error: UNIQUE constraint failed: inbound.bot_id, inbound.external_id",
        );
        assert!(!is_conversation_conflict(&inbound));

        let other = HbotError::internal("store insert_conversation failed: disk full");
        assert!(!is_conversation_conflict(&other));
    }

    #[test]
    fn store_error_keeps_original_text() {
        let error = StoreError::WriterStopped;
        let mapped = store_error("append_log", error);
        assert!(matches!(mapped, HbotError::Internal { .. }));
        assert!(mapped.to_string().contains("append_log"));
    }

    #[tokio::test]
    async fn run_store_returns_value() {
        let value = run_store("sqrt", || Ok(41_u32 + 1)).await;
        assert_eq!(value.expect("value"), 42);
    }
}
