//! 适配器层错误。
//!
//! 与 `hbot_core::HbotError` 的分工：
//!
//! - [`AdapterError`] 表达**具体失败原因**（协议字段坏了、URL 被判为内网、能力缺口），
//!   由适配器内部使用，信息量足够定位问题；
//! - [`HbotError`] 是对外契约（RPC 错误码 `-32001..=-32006`），边界处用
//!   [`From<AdapterError> for HbotError`] 转换，映射关系与 `hbot_core::errors`
//!   的表格一致。
//!
//! 约束：
//!
//! - 错误消息**不得包含密钥**（access token、Authorization 头等）；
//! - 不吞错：调用方要么重试、要么上报、要么显式降级（`AGENTS.md` §0.8）。

use std::time::Duration;

use hbot_core::HbotError;

/// 适配器层错误。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// 平台协议不符合预期（字段缺失、类型错误、返回 retcode 非 0）。
    #[error("protocol error: {message}")]
    Protocol {
        /// 具体原因（可含平台返回的原始 retcode / msg，不含密钥）。
        message: String,
    },

    /// 传输层失败（连接断开、DNS 失败、写帧失败）。
    #[error("transport error: {message}")]
    Transport {
        /// 具体原因。
        message: String,
    },

    /// 被安全策略拒绝（内网地址、非 http/https scheme、重定向）。
    #[error("blocked: {message}")]
    Blocked {
        /// 拒绝原因。
        message: String,
    },

    /// 当前构建不具备该能力（如 https 需要 TLS 后端）。
    ///
    /// 这是**能力缺口**，不是用户输入错误；调用方应显式降级并记录，
    /// 不要静默忽略（`AGENTS.md` §0.8）。
    #[error("unsupported: {message}")]
    Unsupported {
        /// 缺哪项能力、为什么。
        message: String,
    },

    /// 超出上限（响应体大小、URL 长度、请求头大小）。
    #[error("limit exceeded: {message}")]
    Limit {
        /// 超了哪个上限、上限是多少。
        message: String,
    },

    /// 超过截止时间。
    #[error("timeout after {timeout_ms}ms: {operation}")]
    Timeout {
        /// 被中断的操作名（如 `http_fetch`、`connect`）。
        operation: String,
        /// 超时阈值（毫秒）。
        timeout_ms: u64,
    },
}

impl AdapterError {
    /// 构造 [`AdapterError::Protocol`]。
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }

    /// 构造 [`AdapterError::Transport`]。
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }

    /// 构造 [`AdapterError::Blocked`]。
    pub fn blocked(message: impl Into<String>) -> Self {
        Self::Blocked {
            message: message.into(),
        }
    }

    /// 构造 [`AdapterError::Unsupported`]。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported {
            message: message.into(),
        }
    }

    /// 构造 [`AdapterError::Limit`]。
    pub fn limit(message: impl Into<String>) -> Self {
        Self::Limit {
            message: message.into(),
        }
    }

    /// 构造 [`AdapterError::Timeout`]；`timeout` 只用于取毫秒数。
    pub fn timeout(operation: impl Into<String>, timeout: Duration) -> Self {
        Self::Timeout {
            operation: operation.into(),
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

impl From<AdapterError> for HbotError {
    /// 边界转换：适配器错误 → RPC 错误码。
    ///
    /// | [`AdapterError`] | [`HbotError`] | 码 |
    /// |---|---|---|
    /// | `Protocol` / `Limit` | `Invalid` | `-32002` |
    /// | `Transport` | `Internal` | `-32006` |
    /// | `Blocked` / `Unsupported` | `Denied` | `-32005` |
    /// | `Timeout` | `Timeout` | `-32004` |
    fn from(err: AdapterError) -> Self {
        match err {
            AdapterError::Protocol { message } => HbotError::invalid(message),
            AdapterError::Limit { message } => HbotError::invalid(message),
            AdapterError::Transport { message } => HbotError::internal(message),
            AdapterError::Blocked { message } => HbotError::denied(message),
            AdapterError::Unsupported { message } => HbotError::denied(message),
            AdapterError::Timeout {
                operation,
                timeout_ms,
            } => HbotError::Timeout {
                operation,
                timeout_ms,
            },
        }
    }
}

impl From<serde_json::Error> for AdapterError {
    fn from(err: serde_json::Error) -> Self {
        Self::protocol(format!("invalid json: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_to_rpc_error_codes() {
        let cases = [
            (AdapterError::protocol("x"), -32002),
            (AdapterError::limit("x"), -32002),
            (AdapterError::transport("x"), -32006),
            (AdapterError::blocked("x"), -32005),
            (AdapterError::unsupported("x"), -32005),
            (
                AdapterError::timeout("op", Duration::from_millis(5)),
                -32004,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(HbotError::from(err).code(), expected);
        }
    }
}
