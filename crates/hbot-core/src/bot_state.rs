//! Bot 侧状态能力（C2）的契约：`kv` 与 `history`。
//!
//! 设计真源是 `docs/07-bot-runtime-blueprint.md` §5。这里只放**类型与端口 trait**，
//! 实现分两处：
//!
//! - `hbot-store` 提供 SQL 与迁移（`bot_kv` 表；history 复用 `inbound` / `outbound`）；
//! - `hbot-rpc` 把 store + 时钟拼成 [`BotState`] / [`HistoryReader`] 的实现，
//!   并在那里做**清单声明**这一层闸（它才有 manifest）。
//!
//! `hbot-code` 只依赖 `hbot-core`（见 `xtask/src/deps.rs`），所以它拿到的是 trait
//! 对象；沙箱在 `spawn_blocking` 里调，和 [`crate::ports::CommandRunner`] 一样是
//! **同步阻塞**端口。
//!
//! ## 为什么 kv 的写要能"多键原子"
//!
//! 生成的 bot 代码最常见的一段是"读计数器 → 加一 → 写回"。如果 `setMany` 不是
//! 原子事务，进程在中途被杀就会留下写了一半的状态，而且这种坏状态**没有任何
//! 报错**，只会在几天后表现为"数据对不上"。所以：
//!
//! - 单键 `set` 是一次事务；
//! - `setMany` 是**一次**事务（全成或全不成），宿主实现不许循环调用单键写；
//! - 值上限、键上限都在这一层定义成常量，两层实现都读同一份，不各抄一个数字。
//!
//! ## 为什么 history 是只读的
//!
//! 历史来自 `inbound` / `outbound` 两张表，它们是运行时写的账本。bot 代码能读
//! 才能做上下文相关的回复（"他刚才问了什么"），但**不能改**：一旦允许改写，
//! 账本就失去审计意义，回放与影子对比也会对不上。

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::Timestamp;
use crate::ids::{BotId, ConversationId};

/// 一次 `kv.setMany` 里的一个条目。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvEntry {
    /// 键名；长度上限见 [`MAX_KV_KEY_BYTES`]，不许是空串。
    pub key: String,
    /// 任意 JSON 值；序列化后的字节上限见 [`MAX_KV_VALUE_BYTES`]。
    pub value: serde_json::Value,
}

impl KvEntry {
    /// 便捷构造。
    pub fn new(key: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }
}

/// 键名长度上限（UTF-8 字节）。
///
/// 128 是"够长到能表达层级前缀（`user:123:step`），短到能建索引"的折中；
/// 上限存在的真实理由是**避免 handler 用 1 MB 长的键把索引撑爆**。
pub const MAX_KV_KEY_BYTES: usize = 128;

/// 单个值的字节上限（JSON 序列化之后）。
///
/// 64 KiB：bot 的 kv 是"状态"不是"文件存储"。要放大块数据应该用 `ctx.file`
/// （C3，尚未实现）或项目内文件，而不是把 kv 当 blob 表用。
pub const MAX_KV_VALUE_BYTES: usize = 64 * 1024;

/// 一次 `setMany` 的条目数上限。
pub const MAX_KV_BATCH: usize = 100;

/// `kv.list` 一次返回的键数上限。
pub const MAX_KV_LIST: usize = 1000;

/// TTL 上限：30 天。
///
/// 更长的"过期"没有意义（没人会等 30 天），却会让清理逻辑被绕过；需要长期
/// 数据就表明显式存更好。
pub const MAX_KV_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `kv.set` / `kv.setMany` 的可选参数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvWriteOptions {
    /// 存活时间；`None` = 不过期。超过 [`MAX_KV_TTL`] 的请求**报错**，不静默截断。
    pub ttl: Option<Duration>,
}

/// 一条历史消息的方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    /// 别人发进来、bot 被唤醒的那条。
    Inbound,
    /// bot 自己发出去的（`ctx.reply` 之后落库的行）。
    Outbound,
}

impl HistoryDirection {
    /// 对外的字面量（`"inbound"` / `"outbound"`），JS 侧直接看到它。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

impl fmt::Display for HistoryDirection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `ctx.history(n)` 返回的一条。
///
/// **不含 `payload`**：原始平台事件可能有几十 KB，把 n 条塞进上下文会把 handler
/// 的 8 MiB 捕获上限之外的东西也撑爆（更没有理由让它进模型）。要原始事件就
/// 回放/日志里看。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 方向。
    pub direction: HistoryDirection,
    /// 本仓库的消息 id（`inbound.id` / `outbound.id`），不是平台 id。
    pub message_id: String,
    /// 平台侧消息 id；平台没给时是本地兜底键（语义见 `hbot.d.ts` 的 `externalId`）。
    pub external_id: String,
    /// 发送者平台 id；出站行是 bot 自己（可能是空串，取决平台）。
    pub sender_id: String,
    /// 已归一化的纯文本。没有文本的消息是空串，不是 `null`。
    pub text: String,
    /// 时间（Unix 毫秒）。
    pub at: Timestamp,
}

/// `history(n)` 的条数上限。
///
/// 50 条已经能覆盖"翻一眼前文"；再往上调用方要的其实是检索，那是 `ctx.search`
/// 或 `ctx.sql` 的事，不该用 history 把上下文灌满。
pub const MAX_HISTORY: u32 = 50;

/// 状态能力的错误。
///
/// 与 [`crate::device::DeviceError`] 同构：每个变体一个稳定的 `HBOT_STATE_*`
/// 前缀，因为生成代码只能靠 message 前缀区分失败原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BotStateError {
    /// 清单没声明这项能力（`capabilities.state.kv` / `.history`）。
    NotDeclared {
        /// 能力名，例如 `"kv"`。
        capability: &'static str,
    },
    /// 宿主没装配状态后端（无头/测试装配缺失）。
    Unavailable {
        /// 人话原因。
        reason: String,
    },
    /// 参数不合法（键太长、值太大、批量超限、TTL 超限……）。
    Invalid {
        /// 人话原因，会原样进 JS 异常 message。
        message: String,
    },
    /// 存储层失败（磁盘满、库损坏、锁中毒）。
    Failed {
        /// 人话原因。
        message: String,
    },
}

impl BotStateError {
    /// 稳定的错误前缀。JS 侧 `String(err)` 以它开头。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_STATE_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_STATE_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_STATE_INVALID",
            Self::Failed { .. } => "HBOT_STATE_FAILED",
        }
    }
}

impl fmt::Display for BotStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::NotDeclared { capability } => write!(
                formatter,
                "{code}: 项目没有声明能力 `{capability}`（在 hbot.json 的 capabilities.state 里声明）"
            ),
            Self::Unavailable { reason } => write!(formatter, "{code}: {reason}"),
            Self::Invalid { message } => write!(formatter, "{code}: {message}"),
            Self::Failed { message } => write!(formatter, "{code}: {message}"),
        }
    }
}

impl std::error::Error for BotStateError {}

/// Bot 级键值状态（C2.kv）。
///
/// 语义：
///
/// - **按 `BotId` 隔离**：同一个项目里的两个 bot 不共享命名空间；
/// - 值是任意 JSON，读出来是**解析后的值**，不是字符串；
/// - TTL 到期后 `get` 返回 `None`、`list` 不再列出，**但行不一定立刻被删**
///   （实现可以在写路径上顺手清理，不许假设"查不到就是没存过"）；
/// - `setMany` 是一次事务。
///
/// 同步阻塞端口的理由见模块文档：实现是 SQLite 单 writer 线程，async 调用方
/// 必须放进 `spawn_blocking`。
pub trait BotState: Send + Sync {
    /// 读一个键。不存在或已过期返回 `Ok(None)`。
    fn kv_get(&self, bot_id: &BotId, key: &str)
    -> Result<Option<serde_json::Value>, BotStateError>;

    /// 写一个键（覆盖）。`options.ttl` 到点后视为不存在。
    fn kv_set(
        &self,
        bot_id: &BotId,
        key: &str,
        value: serde_json::Value,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError>;

    /// 删一个键。返回**本次是否真的删掉了一行**（过期未清理的行也算删掉）。
    fn kv_delete(&self, bot_id: &BotId, key: &str) -> Result<bool, BotStateError>;

    /// 列键名（按字典序），可选前缀过滤。不返回已过期的键，最多
    /// [`MAX_KV_LIST`] 个。
    fn kv_list(&self, bot_id: &BotId, prefix: Option<&str>) -> Result<Vec<String>, BotStateError>;

    /// 一次事务写多个键（全成或全不成）。
    ///
    /// 同一批次里重复的键**后者覆盖前者**（不报错：批量写的意图是"把这几个键设成
    /// 这些值"，重复键是调用方的写法问题，不值得让整批失败）。
    fn kv_set_many(
        &self,
        bot_id: &BotId,
        entries: Vec<KvEntry>,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError>;
}

/// Bot 的会话历史（C2.history），只读。
pub trait HistoryReader: Send + Sync {
    /// 某个会话最近 `limit` 条消息，**时间升序**（最老的在前），含 bot 自己发出去的。
    ///
    /// `limit` 超过 [`MAX_HISTORY`] 按上限算。会话不存在返回空表（不是错误）。
    fn history(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, BotStateError>;
}

/// 参数闸：键名。空串、超长都拒。
///
/// 放在这里而不是各层各写一份，理由和 `hbot-core::command` 的参数类型一样：
/// **判据只能有一份**。生成代码侧（JS 解析之后）与端口实现侧（被直接调用的测试、
/// 未来的第二宿主）都要过同一个函数，否则"某一层忘了查"就是一条静默的越界路径。
pub fn validate_key(key: &str) -> Result<(), BotStateError> {
    if key.is_empty() {
        return Err(BotStateError::Invalid {
            message: String::from("kv 键不能是空串"),
        });
    }
    if key.len() > MAX_KV_KEY_BYTES {
        return Err(BotStateError::Invalid {
            message: format!("kv 键太长：{} 字节（上限 {MAX_KV_KEY_BYTES}）", key.len()),
        });
    }
    Ok(())
}

/// 参数闸：值。序列化后超过 [`MAX_KV_VALUE_BYTES`] 就拒。
pub fn validate_value(value: &serde_json::Value) -> Result<(), BotStateError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| BotStateError::Invalid {
            message: format!("kv 值无法序列化成 JSON：{error}"),
        })?
        .len();
    if bytes > MAX_KV_VALUE_BYTES {
        return Err(BotStateError::Invalid {
            message: format!(
                "kv 值太大：{bytes} 字节（上限 {MAX_KV_VALUE_BYTES}）；大块数据请放项目文件"
            ),
        });
    }
    Ok(())
}

/// 参数闸：一次批量的条目数。
pub fn validate_batch(len: usize) -> Result<(), BotStateError> {
    if len > MAX_KV_BATCH {
        return Err(BotStateError::Invalid {
            message: format!("kv.setMany 一次最多 {MAX_KV_BATCH} 条，收到 {len} 条"),
        });
    }
    Ok(())
}

/// 参数闸：TTL。超过 [`MAX_KV_TTL`] **报错**，不静默截断。
pub fn validate_ttl(ttl: Option<Duration>) -> Result<(), BotStateError> {
    if let Some(ttl) = ttl
        && ttl > MAX_KV_TTL
    {
        return Err(BotStateError::Invalid {
            message: format!(
                "kv TTL 太长：{} 秒（上限 {} 秒）",
                ttl.as_secs(),
                MAX_KV_TTL.as_secs()
            ),
        });
    }
    Ok(())
}

/// `history(n)` 的条数落进 `1..=MAX_HISTORY`。
///
/// 与 kv 的 TTL 不同，这里**夹住而不是报错**：`history(1000)` 的意图明确
/// （"尽量多给我点"），而 `history(0)` 没有意义 → 取 1。理由写在这里，是因为
/// "两种错误处理为什么不一致"是最容易被后人"统一"掉的东西。
#[must_use]
pub fn history_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_HISTORY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_stable_code_prefix() {
        let cases = [
            (
                BotStateError::NotDeclared { capability: "kv" },
                "HBOT_STATE_NOT_DECLARED",
            ),
            (
                BotStateError::Unavailable {
                    reason: String::from("没装"),
                },
                "HBOT_STATE_UNAVAILABLE",
            ),
            (
                BotStateError::Invalid {
                    message: String::from("键太长"),
                },
                "HBOT_STATE_INVALID",
            ),
            (
                BotStateError::Failed {
                    message: String::from("磁盘满"),
                },
                "HBOT_STATE_FAILED",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert!(
                error.to_string().starts_with(code),
                "Display 必须以错误码开头，否则生成代码无法区分失败原因：{error}"
            );
        }
    }

    #[test]
    fn direction_spellings_are_the_ones_in_the_dts() {
        assert_eq!(HistoryDirection::Inbound.as_str(), "inbound");
        assert_eq!(HistoryDirection::Outbound.as_str(), "outbound");
        // 序列化必须与 as_str 一致，否则 wire 与 d.ts 会各说一套。
        assert_eq!(
            serde_json::to_string(&HistoryDirection::Outbound).expect("序列化"),
            "\"outbound\""
        );
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        // 这几个数字同时出现在 hbot.d.ts 与 docs/07 §5，改一处就要改三处。
        assert_eq!(MAX_KV_KEY_BYTES, 128);
        assert_eq!(MAX_KV_VALUE_BYTES, 65_536);
        assert_eq!(MAX_KV_BATCH, 100);
        assert_eq!(MAX_KV_LIST, 1_000);
        assert_eq!(MAX_HISTORY, 50);
        assert_eq!(MAX_KV_TTL, Duration::from_secs(2_592_000));
    }

    #[test]
    fn an_empty_or_oversized_key_is_refused() {
        assert!(validate_key("").is_err());
        assert!(validate_key("a").is_ok());
        assert!(validate_key(&"k".repeat(MAX_KV_KEY_BYTES)).is_ok());
        let error = validate_key(&"k".repeat(MAX_KV_KEY_BYTES + 1)).expect_err("超长应拒");
        assert_eq!(error.code(), "HBOT_STATE_INVALID");
    }

    #[test]
    fn an_oversized_value_is_refused_after_serialisation() {
        assert!(validate_value(&serde_json::json!({"a": 1})).is_ok());
        // 每个字符 1 字节 + 两个引号，所以 65535 个字符刚好在上限内。
        let ok = serde_json::Value::String("x".repeat(MAX_KV_VALUE_BYTES - 2));
        assert!(validate_value(&ok).is_ok());
        let too_big = serde_json::Value::String("x".repeat(MAX_KV_VALUE_BYTES));
        let error = validate_value(&too_big).expect_err("超限应拒");
        assert_eq!(error.code(), "HBOT_STATE_INVALID");
    }

    #[test]
    fn the_batch_and_the_ttl_are_bounded() {
        assert!(validate_batch(MAX_KV_BATCH).is_ok());
        assert!(validate_batch(MAX_KV_BATCH + 1).is_err());
        assert!(validate_ttl(None).is_ok());
        assert!(validate_ttl(Some(MAX_KV_TTL)).is_ok());
        // 超上限**报错**，不静默截到 30 天：截断会让调用方以为它设的值生效了。
        let error = validate_ttl(Some(MAX_KV_TTL + Duration::from_secs(1))).expect_err("应拒");
        assert_eq!(error.code(), "HBOT_STATE_INVALID");
    }

    #[test]
    fn the_history_limit_is_clamped_not_refused() {
        // 与 TTL 故意不同：意图明确，只要夹住。
        assert_eq!(history_limit(0), 1);
        assert_eq!(history_limit(10), 10);
        assert_eq!(history_limit(MAX_HISTORY), MAX_HISTORY);
        assert_eq!(history_limit(MAX_HISTORY + 1), MAX_HISTORY);
        assert_eq!(history_limit(u32::MAX), MAX_HISTORY);
    }
}
