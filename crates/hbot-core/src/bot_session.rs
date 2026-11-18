//! 会话能力（`C2.session`）的契约：等待行的形状、参数闸与唤醒判据。
//!
//! 设计真源是 `docs/07-bot-runtime-blueprint.md` §5.1。那一节来自旧项目 `wait_input`
//! 的实测教训（`docs/00-assessment.md` P0-3）：等待表只在内存、重启即丢、超时靠
//! 「下一条事件」结算，且 `any_in_group` 会让群里 B 的消息填掉 A 的等待。
//!
//! ## 四条硬规则（每一条都有对应的失败码）
//!
//! 1. **归属键是 `(bot_id, conversation_id, sender_id)` 三元组**，缺一不可；要
//!    「任何人可答」必须显式写 `scope: 'conversation'`，缺省是只认同一个人。
//! 2. **等待落库**（`bot_waits` 表），进程重启后恢复 —— 内存队列重启就是丢。
//! 3. **超时由定时器结算**，不依赖「下一条消息到达」：一个没人回的问题，
//!    在没有新消息的会话里也必须照样在到点时结束。
//! 4. **同一个会话同时只允许一个未结等待**，第二个直接报 `HBOT_SESSION_PENDING`。
//!    两个并存时「用户这句话回答的是哪一个」没有正确答案，只能靠猜。
//!
//! ## 为什么 `await` 不阻塞 isolate
//!
//! §5.1 的旧写法是 `await(predicate, timeoutMs)`。**它会把同会话的串行队列锁死**：
//! 一个会话的队列是严格 FIFO、一次只跑一个 handler（`docs/07` §4.2），而等待要等的
//! 恰恰是「同一个会话的下一条消息」—— 那条消息排在队尾，正等着 handler 还给它。
//! 所以 `await` 只能是**注册**而不是**阻塞**：
//!
//! * `ctx.session.prompt(text, options)` 写一行等待、把提示发出去，然后 handler
//!   正常返回；
//! * 下一条消息到达时，宿主先问库「这条消息唤醒谁」；命中的那条交给 `onResume`
//!   （而不是 `onMessage`），handler 里的 `ctx.waitKey` 告诉它「我在等什么」。
//!
//! 这样带来两个白拿的好处：等待天然就是**持久**的（行在库里），而且**崩溃恢复**
//! 与「重启后继续等」是同一段代码。
//!
//! ## 为什么匹配必须是一条 SQL
//!
//! 「这条消息唤醒哪个等待」如果拆成「先查后改」，两条语句之间会有一个窗口：同一条
//! 消息被投递两次（平台重投、崩溃重放）会**唤醒两次**，而第二次时等待已经 `fulfilled`。
//! 与 `bot_jobs` 的 claim 同一条理由（`crates/hbot-store/src/repositories/jobs.rs` 的
//! 模块文档）：`UPDATE … RETURNING` 一步完成「挑出唯一那个 + 改成已唤醒 + 返回它」。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::Timestamp;
use crate::ids::{BotId, ConversationId, WaitId};

/// 等待键的长度上限（UTF-8 字节）。
///
/// 键是「这个等待在业务里叫什么」（`order-no` / `captcha`），够长能表达层级、
/// 短到能进日志一行的标题区。
pub const MAX_WAIT_KEY_BYTES: usize = 64;

/// 提示文本的字节上限。
///
/// 4 KiB：提示是「请把订单号发给我」这种一句话，不是把整篇文档发给用户。
/// 真要发长文应该用 `ctx.reply`，而不是把它塞进等待行（等待行要在每次重启时
/// 被读出来恢复）。
pub const MAX_WAIT_PROMPT_BYTES: usize = 4 * 1024;

/// 一个 bot 同时挂着的等待行上限。
///
/// 64 是「一个会话最多一个」与「一个 bot 有多少个活跃会话」的乘积级上界。
/// 到这个数说明有人在批量 `prompt` 却没人在回答，那是设计问题不是容量问题。
pub const MAX_WAITS_PER_BOT: usize = 64;

/// 等待超时的下界：1 秒。
pub const MIN_WAIT_TIMEOUT_MS: u64 = 1_000;

/// 等待超时的缺省值：5 分钟。
///
/// 5 分钟是「人工回一句话」的现实上界：更短会让真实用户来不及打完字，更长会让
/// 一个无人应答的会话在库里挂一整天（每次重启都要把它读出来恢复）。
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// 等待超时的上界：24 小时。
///
/// 超过一天的「等一句话」不是等待，是异步任务 —— 那是 `ctx.jobs` 的地盘。
pub const MAX_WAIT_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// 同一个会话同时允许几个未结等待。
///
/// **恒为 1，不是可配项**（§5.1 规则 4）。两个并存时「用户这句话回答的是哪一个」
/// 没有正确答案：猜最近的那个会被乱序消息打穿，猜最老的那个会让后开的等待饿死。
pub const MAX_WAITS_PER_CONVERSATION: usize = 1;

/// 谁来回答这个等待。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitScope {
    /// 只认开等待时那个人（缺省，也是最安全的一种）。
    #[default]
    Sender,
    /// 这个会话里任何人回答都算 —— **必须显式声明**，且日志里要标出来
    /// （「谁能答」是运营审的时候第一个要看的东西）。
    Conversation,
}

impl WaitScope {
    /// 库表 / 日志里的字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Conversation => "conversation",
        }
    }

    /// 从库表读回；未知字符串返回 `None`（由调用方决定报错还是当成默认）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sender" => Some(Self::Sender),
            "conversation" => Some(Self::Conversation),
            _ => None,
        }
    }

    /// 这条消息（发送者 `sender_id`）能不能回答一个由 `owner` 开的等待。
    ///
    /// 唯一的匹配判据：**别在别处再写一份**。JS 侧、存储侧、装配侧的判断都必须
    /// 收敛到这一个函数，否则「群里 B 填掉了 A 的等待」会以「某一层漏判」的形式复活。
    #[must_use]
    pub fn accepts(self, owner: &str, sender_id: &str) -> bool {
        match self {
            Self::Sender => owner == sender_id,
            Self::Conversation => true,
        }
    }
}

impl fmt::Display for WaitScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一条等待在库表里的状态。
///
/// 拼写是契约：`bot_waits.state` 列、日志文案与 `docs/07` §5.1 三处共用这一份
/// [`WaitState::as_str`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    /// 已注册、还没人回答（唯一会「等」的状态）。
    Pending,
    /// 被一条消息唤醒（成功终态）。
    Fulfilled,
    /// 到点还没有消息（超时终态）。
    Expired,
    /// `bot_stop` 取消（终态，重启**不**恢复）。
    Cancelled,
}

impl WaitState {
    /// 库表 / 日志里用的字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fulfilled => "fulfilled",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    /// 从库表读回。未知字符串返回 `None`。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "fulfilled" => Some(Self::Fulfilled),
            "expired" => Some(Self::Expired),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// 终态：不会再被唤醒，也不会被超时再结算一次。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

impl fmt::Display for WaitState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一条等待：`ctx.session.prompt()` 注册的那一份。
///
/// **只放「业务想要什么」**：没有 id、没有状态、没有时间戳 —— 那些是宿主在落库时
/// 补的。这样声明与存储两侧不会各自长出一半字段，也不会出现「构造时就先填一个假
/// 状态」这种伏笔。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitSpec {
    /// 等待键：`onResume` 里 `ctx.waitKey` 就是它（「我在等订单号」）。
    pub key: String,
    /// 注册时发给用户的那句话（`ctx.session.prompt` 的第一个参数）。
    pub prompt: String,
    /// 谁来回答；缺省 [`WaitScope::Sender`]。
    #[serde(default)]
    pub scope: WaitScope,
    /// 超时毫秒；缺省 [`DEFAULT_WAIT_TIMEOUT_MS`]，夹在
    /// [`MIN_WAIT_TIMEOUT_MS`] ..= [`MAX_WAIT_TIMEOUT_MS`] 之间。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl WaitSpec {
    /// 构造一条等待（**不做校验**，校验用 [`validate_spec`]）。
    #[must_use]
    pub fn new(key: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            prompt: prompt.into(),
            scope: WaitScope::Sender,
            timeout_ms: None,
        }
    }

    /// 改成「这个会话里谁都能答」。
    ///
    /// 显式方法而不是 `spec.scope = …`：这是**放宽**授权面的动作，调用点要能一眼
    /// 被 grep 出来（`docs/07` §5.1 要求 UI 与日志标出这种等待）。
    #[must_use]
    pub fn for_conversation(mut self) -> Self {
        self.scope = WaitScope::Conversation;
        self
    }

    /// 指定超时毫秒（会被 [`Self::effective_timeout_ms`] 夹到合法区间）。
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// 生效的超时（缺省 + 夹取之后的那个值）。
    ///
    /// **夹取不报错**：与 `kv` 的 TTL 相反（那边超限报错、这边夹住），理由是意图不同 ——
    /// `kv` 的 TTL 是「我就要它活这么久」，截断会让调用方以为设的值生效了；
    /// 等待的超时是「最多等这么久」，夹住正是用户想要的结果（`docs/07` §5.0 同一口径）。
    #[must_use]
    pub fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
            .clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS)
    }
}

/// 一个到点的等待：宿主结算超时之前要知道的那几件事。
///
/// 与 [`crate::bot_timer::DueSchedule`] 同一形状：只带**派发必需**的字段，
/// 不让调用方为了一个名字再去读一次库（那等于把一个判据拆成两处读）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueWait {
    /// 等待行主键（`bot_waits.id`），取消与结算都靠它。
    pub id: WaitId,
    /// 等待键；`onResume` 的 `ctx.waitKey`。
    pub key: String,
    /// 注册时的提示文本（超时日志里要能说清「哪个等待过期了」）。
    pub prompt: String,
    /// 谁开的（超时行要落 `owner`，排障时才知道是哪个用户没回话）。
    pub owner: String,
    /// 回复目标（`conversations.external_id`）。
    pub conversation: String,
    /// 这个等待到点的时间（Unix 毫秒）。
    pub deadline: Timestamp,
}

/// 一次唤醒：一条消息命中了哪个等待。
///
/// 由存储层在**一条 SQL** 里挑出来（见模块文档「为什么匹配必须是一条 SQL」），
/// 宿主据此把这条消息交给 `onResume` 而不是 `onMessage`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitHit {
    /// 被唤醒的等待行。
    pub id: WaitId,
    /// 等待键。
    pub key: String,
    /// 注册时的提示文本（`onResume` 的 ctx 里要带上它：用户可能已经忘了自己
    /// 被问过什么）。
    pub prompt: String,
    /// 开等待的那个人。
    pub owner: String,
}

/// 会话能力的错误。
///
/// 码表与其它能力一致：声明层 / 装配层 / 参数层 / 执行层，分开报是为了让排障方向
/// 不同（`HBOT_SESSION_NOT_DECLARED` = 改清单；`HBOT_SESSION_UNAVAILABLE` = 装配缺失）；
/// `HBOT_SESSION_PENDING` 是这一层独有的「语义冲突」，它不是配置问题也不是容量问题。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// 清单没声明 `capabilities.state.session`。
    NotDeclared {
        /// 缺的是哪一项（固定 `"session"`）。
        capability: &'static str,
    },
    /// 宿主没装配等待后端。
    Unavailable {
        /// 人话原因。
        reason: String,
    },
    /// 参数不合法（键太长、提示太长、没有正在等待的行……）。
    Invalid {
        /// 人话原因，会原样进 JS 异常 message。
        message: String,
    },
    /// 这个会话已经有一条未结等待（§5.1 规则 4）。
    Pending {
        /// 已有的那条等待的键（文案里要写出来，用户才知道先回答哪个）。
        key: String,
    },
    /// 执行期失败（落库、结算）。
    Failed {
        /// 具体原因。
        message: String,
    },
}

impl SessionError {
    /// 稳定的错误码前缀。JS 侧 `error.message` 以它开头。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_SESSION_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_SESSION_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_SESSION_INVALID",
            Self::Pending { .. } => "HBOT_SESSION_PENDING",
            Self::Failed { .. } => "HBOT_SESSION_FAILED",
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::NotDeclared { capability } => write!(
                formatter,
                "{code}: 项目没有声明能力 `{capability}`（在 hbot.json 的 capabilities.state.session 里声明）"
            ),
            Self::Unavailable { reason } => write!(formatter, "{code}: {reason}"),
            Self::Invalid { message } => write!(formatter, "{code}: {message}"),
            Self::Pending { key } => write!(
                formatter,
                "{code}: 这个会话已经有一条未结等待（`{key}`）：先回答它，或者先 ctx.session.cancel()"
            ),
            Self::Failed { message } => write!(formatter, "{code}: {message}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// 等待存储的端口。
///
/// 与 [`crate::bot_state::BotState`] 一样是**同步阻塞**端口：实现是 SQLite 单 writer
/// 线程，async 调用方必须放进 `spawn_blocking`。
///
/// 两处「必须一条 SQL」的地方（[`Self::claim`] 与 [`Self::expire`]）在各自文档里写了
/// 理由；实现方不许把它们拆成「先查后改」。
pub trait WaitStore: Send + Sync {
    /// 注册一条等待；返回新行的 id。
    ///
    /// `deadline` 由调用方算好（`now + spec.effective_timeout_ms()`）：**库不读时钟**，
    /// 与 `bot_jobs` 的 `next_at` 同一纪律（否则测试注入不了边界、重放也复现不了）。
    ///
    /// 同一个 `(bot_id, conversation_id)` 已有 `pending` 行时必须返回
    /// [`SessionError::Pending`]：这个判断要在**同一条 SQL 里**用部分唯一索引兜住，
    /// 不能靠调用方先查一遍（先查后插在并发下两次都会查到「没有」）。
    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        conversation_external_id: &str,
        owner: &str,
        spec: &WaitSpec,
        deadline: Timestamp,
        now: Timestamp,
    ) -> Result<WaitId, SessionError>;

    /// 一条消息到达时问「它唤醒哪个等待」，命中就**就地**改成 `fulfilled` 并返回。
    ///
    /// 返回 `Ok(None)` 表示这条消息不唤醒任何等待（正常路径，不是错误）。
    /// 判据只有 [`WaitScope::accepts`] 一份，SQL 里的 `scope` 条件必须与它同义。
    fn claim(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        sender_id: &str,
        message_id: &str,
        now: Timestamp,
    ) -> Result<Option<WaitHit>, SessionError>;

    /// 超时结算：把 `deadline <= now` 的 `pending` 行改成 `expired` 并返回它们。
    ///
    /// 一步 SQL（`UPDATE … RETURNING`）：拆成「先查后改」会让同一批行被两次 tick
    /// 各结算一遍，第二次看到一个已经 `expired` 的行。
    fn expire(
        &self,
        bot_id: &BotId,
        now: Timestamp,
        limit: u32,
    ) -> Result<Vec<DueWait>, SessionError>;

    /// 读这个 bot 当前还没结的那条等待（给 `ctx.session.current()` 与日志用）。
    fn current(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
    ) -> Result<Option<DueWait>, SessionError>;

    /// 主动取消一条等待；返回**本次是否真的取消了一行**。
    ///
    /// 已经是终态时不报错、返回 `false`：取消是幂等的（`bot_stop` 会调它，
    /// 而那条等待可能刚好被消息唤醒）。
    fn cancel(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        now: Timestamp,
    ) -> Result<bool, SessionError>;
}

/// 参数闸：等待键。空串、超长都拒。
///
/// 与 `kv` 的 `validate_key` 同一个理由：**判据只能有一份**。生成代码侧（JS 解析
/// 之后）、存储实现侧、以及未来的第二宿主都要过这个函数。
pub fn validate_key(key: &str) -> Result<(), SessionError> {
    if key.is_empty() {
        return Err(SessionError::Invalid {
            message: String::from("等待键不能是空串"),
        });
    }
    if key.len() > MAX_WAIT_KEY_BYTES {
        return Err(SessionError::Invalid {
            message: format!(
                "等待键太长：{} 字节（上限 {MAX_WAIT_KEY_BYTES}）",
                key.len()
            ),
        });
    }
    Ok(())
}

/// 参数闸：提示文本。空白、超长都拒。
///
/// 空提示被拒是因为它把「等什么」这件事从对话里删掉了：用户看到一个没有内容的
/// 提问，只能瞎猜。真要静默等待，就根本不该 `prompt`。
pub fn validate_prompt(prompt: &str) -> Result<(), SessionError> {
    if prompt.trim().is_empty() {
        return Err(SessionError::Invalid {
            message: String::from("等待的提示文本不能为空（用户要靠它知道自己在被问什么）"),
        });
    }
    if prompt.len() > MAX_WAIT_PROMPT_BYTES {
        return Err(SessionError::Invalid {
            message: format!(
                "等待的提示文本太长：{} 字节（上限 {MAX_WAIT_PROMPT_BYTES}）；长文请用 ctx.reply",
                prompt.len()
            ),
        });
    }
    Ok(())
}

/// 参数闸：超时。写下越界的值**报错**，不静默夹取。
///
/// 与 [`WaitSpec::effective_timeout_ms`] 的夹取不矛盾，两者管的是不同的事：
/// 那个函数回答「没写时跑多久 / 边界怎么处理」，这里回答「你写的这个值本身合不
/// 合法」。声明写错（`timeoutMs: 0`）不该被悄悄改成能跑的配置 —— 那会让人以为
/// 自己设的 0 秒生效了，而实际等了一秒。
pub fn validate_timeout(timeout_ms: Option<u64>) -> Result<(), SessionError> {
    let Some(timeout_ms) = timeout_ms else {
        return Ok(());
    };
    if !(MIN_WAIT_TIMEOUT_MS..=MAX_WAIT_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(SessionError::Invalid {
            message: format!(
                "等待超时 {timeout_ms} 毫秒越界：只接受 {MIN_WAIT_TIMEOUT_MS}..={MAX_WAIT_TIMEOUT_MS}（不写就是 {DEFAULT_WAIT_TIMEOUT_MS}）"
            ),
        });
    }
    Ok(())
}

/// 参数闸：整条等待。
///
/// 顺序是**键 → 提示 → 超时**：与错误文案里出现的顺序一致，调用方读到的第一个
/// 问题就是最先要修的那个。
pub fn validate_spec(spec: &WaitSpec) -> Result<(), SessionError> {
    validate_key(&spec.key)?;
    validate_prompt(&spec.prompt)?;
    validate_timeout(spec.timeout_ms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::WaitId;

    fn spec() -> WaitSpec {
        WaitSpec::new("order-no", "请把订单号发给我")
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_WAIT_KEY_BYTES, 64);
        assert_eq!(MAX_WAIT_PROMPT_BYTES, 4096);
        assert_eq!(MAX_WAITS_PER_BOT, 64);
        assert_eq!(MAX_WAITS_PER_CONVERSATION, 1);
        assert_eq!(MIN_WAIT_TIMEOUT_MS, 1_000);
        assert_eq!(DEFAULT_WAIT_TIMEOUT_MS, 300_000);
        assert_eq!(MAX_WAIT_TIMEOUT_MS, 86_400_000);
    }

    #[test]
    fn a_spec_defaults_to_sender_scope_and_five_minutes() {
        assert_eq!(spec().scope, WaitScope::Sender);
        assert_eq!(spec().effective_timeout_ms(), DEFAULT_WAIT_TIMEOUT_MS);
    }

    #[test]
    fn for_conversation_is_the_only_way_to_widen_the_scope() {
        assert_eq!(spec().for_conversation().scope, WaitScope::Conversation);
        // 缺省的命名构造也必须是 sender（放宽授权不能是顺手的事）。
        assert_eq!(
            WaitSpec::new("a", "b").for_conversation().scope,
            WaitScope::Conversation
        );
        assert_eq!(spec().scope, WaitScope::Sender);
    }

    #[test]
    fn the_scope_picks_who_can_answer() {
        assert!(WaitScope::Sender.accepts("u1", "u1"));
        assert!(
            !WaitScope::Sender.accepts("u1", "u2"),
            "缺省必须只认同一个人（群里 B 不能填掉 A 的等待，docs/00 P0-3）"
        );
        assert!(WaitScope::Conversation.accepts("u1", "u2"));
    }

    #[test]
    fn scope_and_state_round_trip_through_their_strings() {
        for scope in [WaitScope::Sender, WaitScope::Conversation] {
            assert_eq!(WaitScope::parse(scope.as_str()), Some(scope));
            assert_eq!(scope.to_string(), scope.as_str());
        }
        for state in [
            WaitState::Pending,
            WaitState::Fulfilled,
            WaitState::Expired,
            WaitState::Cancelled,
        ] {
            assert_eq!(WaitState::parse(state.as_str()), Some(state));
        }
        // 读不懂的字符串绝不能默认成 pending：那会让宿主反复重跑一条没人认得的等待。
        assert_eq!(WaitState::parse("weird"), None);
        assert_eq!(WaitScope::parse("weird"), None);
    }

    #[test]
    fn pending_is_the_only_state_that_is_not_terminal() {
        assert!(!WaitState::Pending.is_terminal());
        assert!(WaitState::Fulfilled.is_terminal());
        assert!(WaitState::Expired.is_terminal());
        assert!(WaitState::Cancelled.is_terminal());
    }

    #[test]
    fn an_empty_or_too_long_key_is_rejected() {
        assert_eq!(validate_key("").unwrap_err().code(), "HBOT_SESSION_INVALID");
        assert!(validate_key(&"k".repeat(MAX_WAIT_KEY_BYTES)).is_ok());
        assert!(validate_key(&"k".repeat(MAX_WAIT_KEY_BYTES + 1)).is_err());
    }

    #[test]
    fn an_empty_prompt_is_rejected_but_a_long_one_is_too() {
        assert!(validate_prompt("   ").is_err(), "空白提示等于什么都没问");
        assert!(validate_prompt(&"p".repeat(MAX_WAIT_PROMPT_BYTES)).is_ok());
        assert!(validate_prompt(&"p".repeat(MAX_WAIT_PROMPT_BYTES + 1)).is_err());
    }

    #[test]
    fn an_out_of_range_timeout_is_reported_rather_than_clamped() {
        assert!(validate_timeout(None).is_ok());
        assert!(
            validate_timeout(Some(0)).is_err(),
            "0 毫秒不是「不超时」，是写错了"
        );
        assert!(validate_timeout(Some(MIN_WAIT_TIMEOUT_MS - 1)).is_err());
        assert!(validate_timeout(Some(MIN_WAIT_TIMEOUT_MS)).is_ok());
        assert!(validate_timeout(Some(MAX_WAIT_TIMEOUT_MS)).is_ok());
        assert!(validate_timeout(Some(MAX_WAIT_TIMEOUT_MS + 1)).is_err());
    }

    #[test]
    fn effective_timeout_clamps_what_the_validator_would_reject() {
        // 这两个数是「已经写进库、不该让宿主算出一个负数等待」的防御。
        assert_eq!(
            spec().with_timeout_ms(0).effective_timeout_ms(),
            MIN_WAIT_TIMEOUT_MS
        );
        assert_eq!(
            spec().with_timeout_ms(u64::MAX).effective_timeout_ms(),
            MAX_WAIT_TIMEOUT_MS
        );
    }

    #[test]
    fn validate_spec_reports_the_first_problem_in_order() {
        let broken = WaitSpec::new("", "");
        let error = validate_spec(&broken).unwrap_err();
        assert!(
            error.to_string().contains("等待键"),
            "先报键、再报提示（调用方按顺序修就对了），实际：{error}"
        );
        assert!(validate_spec(&spec()).is_ok());
    }

    #[test]
    fn the_error_codes_start_with_the_machine_readable_prefix() {
        let errors = [
            SessionError::NotDeclared {
                capability: "session",
            },
            SessionError::Unavailable {
                reason: String::from("没装配"),
            },
            SessionError::Invalid {
                message: String::from("写错了"),
            },
            SessionError::Pending {
                key: String::from("order-no"),
            },
            SessionError::Failed {
                message: String::from("落库失败"),
            },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(
                text.starts_with(error.code()),
                "每条文案都要以码开头（JS 侧按前缀分流），实际：{text}"
            );
        }
    }

    #[test]
    fn a_pending_error_names_the_wait_that_is_in_the_way() {
        let error = SessionError::Pending {
            key: String::from("order-no"),
        };
        assert!(
            error.to_string().contains("order-no"),
            "文案要点名是哪条等待挡住了，实际：{error}"
        );
    }

    #[test]
    fn due_wait_and_hit_carry_the_fields_dispatch_needs() {
        let due = DueWait {
            id: WaitId::from_uuid(uuid::Uuid::from_u128(0x77a17)),
            key: String::from("order-no"),
            prompt: String::from("请把订单号发给我"),
            owner: String::from("u1"),
            conversation: String::from("local:default"),
            deadline: Timestamp::from_unix_millis(1_700_000_000_000),
        };
        assert_eq!(due.key, "order-no");
        let hit = WaitHit {
            id: due.id,
            key: due.key.clone(),
            prompt: due.prompt.clone(),
            owner: due.owner.clone(),
        };
        assert_eq!(hit.id, due.id);
    }
}
