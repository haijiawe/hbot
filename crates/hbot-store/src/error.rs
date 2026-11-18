//! 错误类型。
//!
//! 库代码不使用 `unwrap()`/`expect()`（`AGENTS.md` §5）：所有失败都以 [`StoreError`] 返回，
//! 由 `hbot-rpc` 在边界处映射成 `ApiError`。内部不使用 `anyhow`，保持可匹配的错误变体。

use std::result::Result as StdResult;

/// hbot-store 的统一错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite 返回的错误。对调用方属于“存储层失败”，不做吞掉处理。
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// 打开数据库文件、创建父目录等 IO 失败。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 单 writer 线程已经退出（通常是 `Store` 关闭后再写入）。
    #[error("store writer thread is not available")]
    WriterStopped,

    /// 数据库实际 journal_mode 不是 `wal`；WAL 是本层的硬前提，不静默降级。
    #[error("expected journal_mode=wal, got {actual:?}")]
    JournalMode { actual: String },

    /// 磁盘上的 schema 版本高于本二进制认识的版本：拒绝启动，避免降级写坏数据。
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },

    /// 已发布的迁移被改过（版本号相同、名字不同）。只增不改是硬规则。
    #[error("migration {version} ({name}) does not match recorded migration {recorded:?}")]
    MigrationMismatch {
        version: u32,
        name: &'static str,
        recorded: String,
    },

    /// 迁移执行失败；带版本号便于定位是哪个文件。
    #[error("migration {version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },

    /// 主键冲突：同一 id 已属于另一组唯一键（幂等插入无法用“已存在”解释）。
    #[error("{table} row with id {id:?} collides with a different row")]
    IdCollision { table: &'static str, id: String },

    /// 调用方给了当前列约束不接受的值（例如非法的 `bots.status`）。
    ///
    /// 存储层**不**替调用方挑一个“差不多”的值落库：非法值一律拒绝，由调用方修正后重试。
    #[error("invalid {field} value {value:?} (allowed: {allowed:?})")]
    Invalid {
        field: &'static str,
        value: String,
        allowed: &'static [&'static str],
    },

    /// 这个会话已经有一条未结等待（`bot_waits_conv_pending` 部分唯一索引拒了这次注册）。
    ///
    /// **不是 SQL 错误**：它是 §5.1 规则 4 的业务判据，装配层要据此报 `HBOT_SESSION_PENDING`
    /// 并把挡路那条的键写给用户（否则用户只知道「开不了新的」，不知道该先回哪个）。
    #[error("conversation already has a pending wait ({key:?})")]
    WaitPending {
        /// 挡路那条等待的键。
        key: String,
    },

    /// 内部 Mutex 中毒（持有者 panic）。恢复后继续用会读到不一致状态，因此直接上报。
    #[error("store lock poisoned: {0}")]
    Poisoned(&'static str),
}

/// 本 crate 的结果别名。
pub type Result<T, E = StoreError> = StdResult<T, E>;
