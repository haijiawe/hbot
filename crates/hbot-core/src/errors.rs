//! 领域 / 应用层错误，以及到 RPC 错误码的唯一映射。
//!
//! 错误码是 UI 契约的一部分（`docs/02-architecture.md` §9）。文档目前只给了
//! "`-32001` 等"的区间，没有逐项定义；本文件是码值的**唯一真源**：
//!
//! | 变体 | RPC 码 | 语义 |
//! |---|---|---|
//! | [`HbotError::NotFound`] | `-32001` | 资源不存在 |
//! | [`HbotError::Invalid`] | `-32002` | 参数/状态非法 |
//! | [`HbotError::Conflict`] | `-32003` | 与当前状态冲突 |
//! | [`HbotError::Timeout`] | `-32004` | 操作超时 |
//! | [`HbotError::Denied`] | `-32005` | 权限 / 沙箱 / 审批拒绝 |
//! | [`HbotError::Internal`] | `-32006` | 内部错误 |
//!
//! JSON-RPC 2.0 把 `-32000..=-32099` 留给服务端自定义；传输层标准错误
//! （`-32700` 解析失败、`-32600` 非法请求、`-32601` 方法不存在、`-32602` 参数非法、
//! `-32603` 内部错误）由传输层使用，domain 层不产出这些码。
//!
//! 边界转换：`hbot_rpc::ApiError` 由 `From<HbotError>` 实现生成（`hbot-core` 不能
//! 依赖 `hbot-rpc`，所以方向只能是 `core → rpc`）。

use thiserror::Error;

/// 领域与应用层错误。
///
/// 每个变体都带足够定位问题的字段；`message` 用于直接展示，不得包含密钥
/// （脱敏规则见 `docs/02-architecture.md` §11.3）。
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum HbotError {
    /// 目标资源不存在（项目、bot、会话、文件、审批请求等）。
    #[error("not found: {entity} `{id}`")]
    NotFound {
        /// 资源类别，如 `project` / `bot` / `conversation` / `file`。
        entity: String,
        /// 资源标识（ID 或路径）。
        id: String,
    },

    /// 参数或状态非法（路径逃逸、未知枚举值、格式错误、空输入）。
    #[error("invalid: {message}")]
    Invalid {
        /// 具体原因（可直接展示，不含敏感信息）。
        message: String,
    },

    /// 与当前状态冲突（重复创建、版本/哈希不匹配、并发修改）。
    #[error("conflict: {message}")]
    Conflict {
        /// 冲突说明。
        message: String,
    },

    /// 操作超时；调用方可以重试，但不得自动无限重试。
    #[error("timeout after {timeout_ms}ms: {operation}")]
    Timeout {
        /// 被取消的操作名（如 `run_command`、`llm_request`）。
        operation: String,
        /// 超时阈值（毫秒）。
        timeout_ms: u64,
    },

    /// 权限 / 沙箱档位 / 审批拒绝。
    #[error("denied: {message}")]
    Denied {
        /// 拒绝原因（哪条规则、哪个档位）。
        message: String,
    },

    /// 内部错误：bug、依赖失败、invariant 破坏。**不代表用户输入有问题。**
    #[error("internal: {message}")]
    Internal {
        /// 诊断信息。
        message: String,
    },
}

impl HbotError {
    /// 映射到 RPC 错误码（见模块级表格，`-32001..=-32006`）。
    pub const fn code(&self) -> i32 {
        match self {
            Self::NotFound { .. } => -32001,
            Self::Invalid { .. } => -32002,
            Self::Conflict { .. } => -32003,
            Self::Timeout { .. } => -32004,
            Self::Denied { .. } => -32005,
            Self::Internal { .. } => -32006,
        }
    }

    /// 构造 [`HbotError::NotFound`]。
    pub fn not_found(entity: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            entity: entity.into(),
            id: id.into(),
        }
    }

    /// 构造 [`HbotError::Invalid`]。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    /// 构造 [`HbotError::Conflict`]。
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    /// 构造 [`HbotError::Timeout`]。
    pub fn timeout(operation: impl Into<String>, timeout_ms: u64) -> Self {
        Self::Timeout {
            operation: operation.into(),
            timeout_ms,
        }
    }

    /// 构造 [`HbotError::Denied`]。
    pub fn denied(message: impl Into<String>) -> Self {
        Self::Denied {
            message: message.into(),
        }
    }

    /// 构造 [`HbotError::Internal`]。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}
