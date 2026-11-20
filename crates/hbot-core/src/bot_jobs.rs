//! 后台作业（`C4.jobs`）的契约类型与纯计算。
//!
//! 设计见 `docs/07-bot-runtime-blueprint.md` 的 4.3 节（规则表）与 11 节第 3 步。
//!
//! ## 为什么要有这一层
//!
//! 「同会话严格串行 + handler 预算 2 秒」决定了一个项目**写不出任何跨分钟的业务**
//! （国战结算、批量推送、抓取+分析）。作业把这类重活从消息处理里摘出去：`onMessage`
//! 里 `ctx.jobs.enqueue(...)` 立刻返回，宿主再把作业行落库、按预算跑、失败按退避重试。
//! 这不是性能优化，是能力缺失的补齐（docs/07 的 4.3 节原话）。
//!
//! ## 这个文件只管四件事
//!
//! 1. **声明的形状**：项目在 `hbot.json` 的 `capabilities.jobs` 里写一组具名作业类型
//!    （[`JobSpec`]：名字、预算、重试次数）；
//! 2. **一次入队的形状**（[`JobRequest`]）：名字 + 不透明载荷 + **幂等键**；
//! 3. **状态的字符串形状**（[`JobState`]）：库表、事件、日志三处共用一份拼写；
//! 4. **参数闸与纯计算**（[`validate_request`] / [`retry_delay_ms`]）：声明或参数写错时
//!    在**加载清单 / 入队那一刻**报错，而不是等到作业跑起来才静默不动。
//!
//! ## 语义：至少一次，靠幂等键而不是靠「不会重复执行」
//!
//! 进程崩溃、`bot_stop`、机器休眠都会让「已入队」与「已跑完」之间的窗口变成不确定的。
//! 唯一的正确姿势是：**同一个幂等键最多产生一个作业行**（库表唯一约束），作业体自己
//! 保证重复执行无害。这与入站消息的 `ctx.messageId` 是同一套判据 —— `hbot-code` 在
//! 消息事件里入队时，缺省幂等键就是那条消息的 `messageId`。
//!
//! 反过来说，**不传幂等键的入队必须失败**：没有幂等键就没有去重依据，重启后补跑与
//! 「用户又点了一次」无法区分，那就等于把「至少一次」退化成「随便几次」。
//!
//! ## 为什么作业没有入站身份
//!
//! 作业由某个事件入队，但**执行时**那条消息早已处理完（这正是它的目的）。所以作业的
//! `ctx` 比消息 `ctx` 窄（与 `onTimer` 同构）：有 `reply` / `log` / `sleep` / `ai`，
//! 没有 `kv` / `history` / `device`。要写状态的项目请把状态变化记在下一次消息里，
//! 而不是让作业去猜「上一个事件的 bot 是谁」。
//!
//! 回复目标与定时器同理：**必须声明**（[`JobRequest::conversation`]），否则 `ctx.reply`
//! 明确报错（`HBOT_JOBS_NO_TARGET`）而不是找一个陌生群发出去。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 一个作业名（作业类型名）的最大字节数。
pub const MAX_JOB_NAME_BYTES: usize = 64;

/// 一个项目最多声明多少种作业类型。
///
/// 上限不是为了性能，是为了可解释性：32 种类型已经覆盖「结算 / 推送 / 抓取 / 清理」
/// 这几类真实需求；再往上说明用户在拿作业表当流程引擎用（`AGENTS.md` 第 2 条硬规则
/// 明确不做流程引擎）。
pub const MAX_JOB_TYPES: usize = 32;

/// 一次入队的载荷（`payload`）序列化后的最大字节数：8 KiB。
///
/// 载荷是**不透明 JSON**，宿主只负责原样落库与回填给作业体。上限存在的理由与
/// `bot_state::MAX_KV_VALUE_BYTES` 相同：一行 SQLite 不该装下整个聊天记录，
/// 大对象请先 `kv.set` 再传键。
pub const MAX_JOB_PAYLOAD_BYTES: usize = 8 * 1024;

/// 幂等键的最大字节数。
pub const MAX_JOB_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// 作业预算的缺省值：60 秒。
pub const DEFAULT_JOB_BUDGET_MS: u64 = 60_000;

/// 作业预算的下限：1 秒。
pub const MIN_JOB_BUDGET_MS: u64 = 1_000;

/// 作业预算的上限：10 分钟（桌面端的平台上限，见 `docs/08`）。
///
/// 手机端的 30 秒是**宿主策略**（`hbot-desktop` 按平台算），不在核心契约里：
/// 核心只冻结「声明不能超过多少」，取小的一步（平台上限 vs 声明值）由装配方做。
pub const MAX_JOB_BUDGET_MS: u64 = 600_000;

/// 一次执行真正该给的预算：声明值再被**宿主上限**收紧一次（只收紧，不放宽）。
///
/// `host_ceiling_ms == 0` 表示这台宿主不限（等价于 [`MAX_JOB_BUDGET_MS`]）。
/// 返回值恒落在 [`MIN_JOB_BUDGET_MS`]..=[`MAX_JOB_BUDGET_MS`]。
///
/// 为什么要这一步：清单里的 `budgetMs` 是**项目**要多少，而宿主可能给不了那么多
/// （手机端后台执行窗口只有几十秒，见 `docs/08`）。取小的一步必须只有一份实现 ——
/// 装配方各自写一个 `min` 迟早漂移，而漂移的表现是「同一个项目在桌面能跑完、在手机上
/// 莫名其妙超时」，用户看不出这是两个判据。
///
/// 方向是**只收紧不放宽**：宿主上限写成 1 秒时，声明 60 秒的作业只拿 1 秒；反过来
/// 宿主上限写成 10 分钟也不会把声明 1 秒的作业抬成 10 分钟 —— 声明是用户对代价的承诺。
#[must_use]
pub fn effective_job_budget_ms(row_budget_ms: u64, host_ceiling_ms: u64) -> u64 {
    let declared = if row_budget_ms == 0 {
        DEFAULT_JOB_BUDGET_MS
    } else {
        row_budget_ms.clamp(MIN_JOB_BUDGET_MS, MAX_JOB_BUDGET_MS)
    };
    let ceiling = if host_ceiling_ms == 0 {
        MAX_JOB_BUDGET_MS
    } else {
        host_ceiling_ms.clamp(MIN_JOB_BUDGET_MS, MAX_JOB_BUDGET_MS)
    };
    declared.min(ceiling)
}

/// 重试次数的缺省值：0（不重试）。
///
/// 缺省不重试是刻意的：重试要幂等键真的幂等，而「缺省就重试 3 次」会让一个
/// 只是「忘了写幂等」的项目在真机上重复发消息。要重试就在清单里显式写 `retries`。
pub const DEFAULT_JOB_RETRIES: u32 = 0;

/// 重试次数的上限：5。
pub const MAX_JOB_RETRIES: u32 = 5;

/// 第一次重试的退避：1 秒。
pub const MIN_RETRY_DELAY_MS: u64 = 1_000;

/// 退避的封顶：5 分钟（指数翻倍到顶后不再继续长大）。
pub const MAX_RETRY_DELAY_MS: u64 = 300_000;

/// 每个 bot 最多同时存在多少条未完成作业（超出即 [`JobError::Full`]）。
///
/// 「超出排队」的语义必须有一个上界：没有上界的队列在真机上就是「内存吃满之后
/// 才开始丢」，而丢的时候已经在跑了几千条。
pub const MAX_JOBS_PER_BOT: usize = 256;

/// 一条作业在库表里的状态。
///
/// 拼写是**契约**：`bot_jobs.state` 列、日志文案、以及 `docs/07` 的规则表三处共用
/// 这一份 [`JobState::as_str`]，不要在各层各写一份字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// 已入队、尚未开始（`budgetMs` 说的是「跑多久」，不是「等多久」）。
    Pending,
    /// 正在执行；进程崩在这里，重启后按「至少一次」重新入队。
    Running,
    /// 执行成功（唯一的成功终态）。
    Done,
    /// 重试耗尽的失败终态；错误正文写在 `bot_jobs.last_error`。
    Failed,
    /// `bot_stop` 取消；重启**不**恢复。
    Cancelled,
}

impl JobState {
    /// 库表 / 日志里用的字符串（小写下划线）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 从库表读回。未知字符串返回 `None`，由调用方决定是报错还是当成终态。
    ///
    /// 不在这里 `unwrap_or(default)`：把一条读不懂的行当成 `pending` 会让宿主
    /// 反复重跑一条已经没人认得的作业，那是最坏的一种静默。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// 终态：不会再被派发，也不会被 `bot_stop` 再取消一次。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// 一种具名作业的声明：`hbot.json` 的 `capabilities.jobs` 里的一项。
///
/// `#[serde(rename_all = "camelCase")]`：清单里写
/// `{ "name": "settle-war", "budgetMs": 120000, "retries": 2 }`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    /// 作业类型名，非空、[`MAX_JOB_NAME_BYTES`] 以内、同一项目内唯一。
    pub name: String,
    /// 单次执行预算（毫秒），缺省 [`DEFAULT_JOB_BUDGET_MS`]。
    #[serde(default = "default_budget_ms")]
    pub budget_ms: u64,
    /// 失败后最多再试几次，缺省 [`DEFAULT_JOB_RETRIES`]（= 0，不重试）。
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_budget_ms() -> u64 {
    DEFAULT_JOB_BUDGET_MS
}

fn default_retries() -> u32 {
    DEFAULT_JOB_RETRIES
}

impl JobSpec {
    /// 构造一种作业声明（**不做校验**，校验用 [`validate_specs`]）。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            budget_ms: DEFAULT_JOB_BUDGET_MS,
            retries: DEFAULT_JOB_RETRIES,
        }
    }

    /// 指定预算与重试次数（链式写法，测试与清单构造用）。
    #[must_use]
    pub fn with_limits(mut self, budget_ms: u64, retries: u32) -> Self {
        self.budget_ms = budget_ms;
        self.retries = retries;
        self
    }
}

impl Default for JobSpec {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// 一次入队请求：`ctx.jobs.enqueue(name, payload, options)` 在宿主侧的等价形状。
///
/// 字段全公开是刻意的：这是**跨 crate 的契约**（`hbot-code` 造它、`hbot-rpc` 落库），
/// 中间任何一层都不该再定义一个「几乎一样」的结构。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    /// 作业类型名；必须对应清单里声明过的一条 [`JobSpec`]。
    pub name: String,
    /// 不透明载荷，原样落库、原样回填给作业体。
    pub payload: serde_json::Value,
    /// 幂等键：同一个键最多产生一条作业行（见模块文档的语义一节）。
    pub idempotency_key: String,
    /// 作业的回复目标（平台会话 `external_id`）。
    ///
    /// `None` = 作业只能记日志；`ctx.reply` 会明确报 [`JobError`] 的
    /// `HBOT_JOBS_NO_TARGET` —— 作业背后没有入站消息，宿主没有任何依据猜该发给谁。
    #[serde(default)]
    pub conversation: Option<String>,
}

impl JobRequest {
    /// 构造一次入队请求（**不做校验**，校验用 [`validate_request`]）。
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        payload: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            payload,
            idempotency_key: idempotency_key.into(),
            conversation: None,
        }
    }

    /// 指定回复目标会话（平台外部 ID）。
    #[must_use]
    pub fn with_conversation(mut self, conversation: impl Into<String>) -> Self {
        self.conversation = Some(conversation.into());
        self
    }
}

/// 后台作业功能的错误。
///
/// 码表与其它能力一致，只是把「池满」与「超预算」单独拆出来：两者的补救动作完全不同
/// （前者要降频或延长间隔，后者要拆作业）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobError {
    /// 清单没声明 `capabilities.jobs`，或没声明这个名字。
    NotDeclared {
        /// 缺的是哪一项（`"jobs"` 或具体的作业名）。
        capability: &'static str,
    },
    /// 宿主没有装配作业队列（无头模式、测试、`hbot-bench`）。
    Unavailable {
        /// 具体原因（人话）。
        reason: String,
    },
    /// 参数不合法（名字、载荷、幂等键、预算、重试次数、重复声明）。
    Invalid {
        /// 具体哪里错了。
        message: String,
    },
    /// 这个 bot 的未完成作业已经到 [`MAX_JOBS_PER_BOT`]。
    Full {
        /// 当前数量与上限。
        message: String,
    },
    /// 执行超预算被中断。
    Timeout {
        /// 预算毫秒数。
        ms: u64,
    },
    /// 执行期失败（派发、落库、作业体抛异常）。
    Failed {
        /// 具体原因。
        message: String,
    },
}

impl JobError {
    /// 稳定的错误码前缀。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_JOBS_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_JOBS_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_JOBS_INVALID",
            Self::Full { .. } => "HBOT_JOBS_FULL",
            Self::Timeout { .. } => "HBOT_JOBS_TIMEOUT",
            Self::Failed { .. } => "HBOT_JOBS_FAILED",
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 每条都以错误码开头：生成代码侧只能靠字符串分辨原因（与 TimerError / AiError 同口径）。
        write!(f, "{}", self.code())?;
        match self {
            Self::NotDeclared { capability } => {
                write!(f, ": 项目没有声明 {capability} 这项作业能力")
            }
            Self::Unavailable { reason } => write!(f, ": {reason}"),
            Self::Invalid { message } | Self::Full { message } | Self::Failed { message } => {
                write!(f, ": {message}")
            }
            Self::Timeout { ms } => write!(f, ": 作业在 {ms} 毫秒预算内没有结束"),
        }
    }
}

impl std::error::Error for JobError {}

/// 校验一个作业名。
///
/// # Errors
///
/// 空、纯空白、超长、含控制字符都报 `Invalid`。
pub fn validate_job_name(name: &str) -> Result<(), JobError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(JobError::Invalid {
            message: "作业名不能为空".into(),
        });
    }
    if trimmed.len() > MAX_JOB_NAME_BYTES {
        return Err(JobError::Invalid {
            message: format!(
                "作业名 {} 有 {} 字节，超过上限 {MAX_JOB_NAME_BYTES}",
                trimmed.chars().take(16).collect::<String>(),
                trimmed.len()
            ),
        });
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(JobError::Invalid {
            message: format!("作业名 {trimmed:?} 含控制字符"),
        });
    }
    if trimmed != name {
        return Err(JobError::Invalid {
            message: format!("作业名 {name:?} 首尾有空白"),
        });
    }
    Ok(())
}

/// 校验载荷大小（序列化之后）。
///
/// # Errors
///
/// 无法序列化、或超过 [`MAX_JOB_PAYLOAD_BYTES`] 都报 `Invalid`。
pub fn validate_payload(payload: &serde_json::Value) -> Result<(), JobError> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|error| JobError::Invalid {
            message: format!("作业载荷无法序列化成 JSON：{error}"),
        })?
        .len();
    if bytes > MAX_JOB_PAYLOAD_BYTES {
        return Err(JobError::Invalid {
            message: format!(
                "作业载荷太大：{bytes} 字节（上限 {MAX_JOB_PAYLOAD_BYTES}）；大对象请先 kv.set 再传键"
            ),
        });
    }
    Ok(())
}

/// 校验幂等键。
///
/// # Errors
///
/// 空、纯空白、超长、含控制字符都报 `Invalid`。空白**不是**可选项：缺省即失败。
pub fn validate_idempotency_key(key: &str) -> Result<(), JobError> {
    if key.trim().is_empty() {
        return Err(JobError::Invalid {
            message: "幂等键不能为空（作业是至少一次语义，没有键就无法去重）".into(),
        });
    }
    if key.len() > MAX_JOB_IDEMPOTENCY_KEY_BYTES {
        return Err(JobError::Invalid {
            message: format!(
                "幂等键太长：{} 字节（上限 {MAX_JOB_IDEMPOTENCY_KEY_BYTES}）",
                key.len()
            ),
        });
    }
    if key.chars().any(|c| c.is_control()) {
        return Err(JobError::Invalid {
            message: String::from("幂等键含控制字符"),
        });
    }
    if key.trim() != key {
        return Err(JobError::Invalid {
            message: format!("幂等键 {key:?} 首尾有空白"),
        });
    }
    Ok(())
}

/// 校验一次完整的入队请求：名字、载荷、幂等键、回复目标。
///
/// # Errors
///
/// 任一项不合法都报 `Invalid`。
pub fn validate_request(request: &JobRequest) -> Result<(), JobError> {
    validate_job_name(&request.name)?;
    validate_payload(&request.payload)?;
    validate_idempotency_key(&request.idempotency_key)?;
    if let Some(conversation) = &request.conversation {
        if conversation.trim() != conversation || conversation.trim().is_empty() {
            return Err(JobError::Invalid {
                message: format!(
                    "作业 {} 的 conversation 不能为空串或带首尾空白：收到的是 {conversation:?}",
                    request.name
                ),
            });
        }
    }
    Ok(())
}

/// 校验整张作业类型表：数量、每个名字、每个预算与重试次数、名字唯一。
///
/// # Errors
///
/// 任一类型不合法、重名、超过 [`MAX_JOB_TYPES`] 都报 `Invalid`。
pub fn validate_specs(specs: &[JobSpec]) -> Result<(), JobError> {
    if specs.len() > MAX_JOB_TYPES {
        return Err(JobError::Invalid {
            message: format!(
                "声明了 {} 种作业类型，超过上限 {MAX_JOB_TYPES}",
                specs.len()
            ),
        });
    }
    let mut seen: Vec<&str> = Vec::with_capacity(specs.len());
    for spec in specs {
        validate_job_name(&spec.name)?;
        if spec.budget_ms < MIN_JOB_BUDGET_MS || spec.budget_ms > MAX_JOB_BUDGET_MS {
            return Err(JobError::Invalid {
                message: format!(
                    "作业 {} 的 budgetMs {} 超出 {MIN_JOB_BUDGET_MS}..={MAX_JOB_BUDGET_MS}",
                    spec.name, spec.budget_ms
                ),
            });
        }
        if spec.retries > MAX_JOB_RETRIES {
            return Err(JobError::Invalid {
                message: format!(
                    "作业 {} 的 retries {} 超过上限 {MAX_JOB_RETRIES}",
                    spec.name, spec.retries
                ),
            });
        }
        if seen.iter().any(|name| *name == spec.name) {
            return Err(JobError::Invalid {
                message: format!("作业类型 {} 重复声明", spec.name),
            });
        }
        seen.push(&spec.name);
    }
    Ok(())
}

/// 第 `retries_used` 次重试（从 1 开始数）之前要等多久：`MIN_RETRY_DELAY_MS` 指数翻倍，
/// 封顶 [`MAX_RETRY_DELAY_MS`]。
///
/// 纯整数运算、不读时钟：退避算错了是契约问题，不该需要一秒钟的真等待才能测。
/// `0` 当 `1` 处理（「第 0 次重试」不存在，但把参数夹住比让调用方 panic 好）。
///
/// 为什么是指数而不是固定值：作业失败的原因大多是「上游临时不可用」（模型限流、
/// 平台 502）。固定 1 秒会在一分钟里打 60 次，指数退避把第 5 次推到上限以内。
#[must_use]
pub fn retry_delay_ms(retries_used: u32) -> u64 {
    let exponent = retries_used.clamp(1, 16);
    let factor = 1u64 << (exponent - 1);
    MIN_RETRY_DELAY_MS
        .saturating_mul(factor)
        .min(MAX_RETRY_DELAY_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_JOB_NAME_BYTES, 64);
        assert_eq!(MAX_JOB_TYPES, 32);
        assert_eq!(MAX_JOB_PAYLOAD_BYTES, 8 * 1024);
        assert_eq!(MAX_JOB_IDEMPOTENCY_KEY_BYTES, 128);
        assert_eq!(DEFAULT_JOB_BUDGET_MS, 60_000);
        assert_eq!(MIN_JOB_BUDGET_MS, 1_000);
        assert_eq!(MAX_JOB_BUDGET_MS, 600_000);
        assert_eq!(DEFAULT_JOB_RETRIES, 0);
        assert_eq!(MAX_JOB_RETRIES, 5);
        assert_eq!(MIN_RETRY_DELAY_MS, 1_000);
        assert_eq!(MAX_RETRY_DELAY_MS, 300_000);
        assert_eq!(MAX_JOBS_PER_BOT, 256);
    }

    #[test]
    fn a_spec_defaults_to_a_minute_and_no_retries() {
        let spec = JobSpec::new("settle-war");
        assert_eq!(spec.budget_ms, 60_000);
        assert_eq!(spec.retries, 0);
    }

    #[test]
    fn specs_deserialize_from_camel_case_with_defaults() {
        // 清单里只写名字时，预算与重试必须落到缺省值而不是 0（0 会被 validate 拒）。
        let bare: JobSpec = serde_json::from_str(r#"{"name": "settle-war"}"#).expect("能解析");
        assert_eq!(bare.budget_ms, DEFAULT_JOB_BUDGET_MS);
        assert_eq!(bare.retries, DEFAULT_JOB_RETRIES);
        let full: JobSpec =
            serde_json::from_str(r#"{"name": "settle-war", "budgetMs": 120000, "retries": 2}"#)
                .expect("能解析");
        assert_eq!(full.budget_ms, 120_000);
        assert_eq!(full.retries, 2);
        assert_eq!(
            serde_json::to_string(&full).expect("能序列化"),
            r#"{"name":"settle-war","budgetMs":120000,"retries":2}"#
        );
    }

    #[test]
    fn the_budget_and_retries_are_bounded_at_both_ends() {
        for budget_ms in [0_u64, MIN_JOB_BUDGET_MS - 1, MAX_JOB_BUDGET_MS + 1] {
            let error = validate_specs(&[JobSpec::new("x").with_limits(budget_ms, 0)])
                .expect_err("预算越界应当报错");
            assert_eq!(error.code(), "HBOT_JOBS_INVALID");
            assert!(error.to_string().contains("budgetMs"), "{error}");
        }
        let error = validate_specs(&[JobSpec::new("x").with_limits(60_000, MAX_JOB_RETRIES + 1)])
            .expect_err("重试越界应当报错");
        assert!(error.to_string().contains("retries"), "{error}");
    }

    #[test]
    fn names_are_trimmed_refused_and_bounded() {
        assert!(validate_job_name("settle-war").is_ok());
        assert!(validate_job_name("").is_err());
        assert!(validate_job_name("   ").is_err());
        assert!(validate_job_name(" settle-war").is_err());
        assert!(validate_job_name("settle\u{0}war").is_err());
        let long = "a".repeat(MAX_JOB_NAME_BYTES + 1);
        let error = validate_job_name(&long).expect_err("超长名字应当报错");
        assert!(error.to_string().contains("超过上限"), "{error}");
    }

    #[test]
    fn the_type_table_refuses_duplicates_and_overflow() {
        let duplicate = [
            JobSpec::new("settle-war"),
            JobSpec::new("settle-war").with_limits(120_000, 1),
        ];
        let error = validate_specs(&duplicate).expect_err("重名应当报错");
        assert!(error.to_string().contains("重复声明"), "{error}");

        let mut many = Vec::new();
        for index in 0..=MAX_JOB_TYPES {
            many.push(JobSpec::new(format!("job-{index}")));
        }
        let error = validate_specs(&many).expect_err("超量应当报错");
        assert!(error.to_string().contains("超过上限"), "{error}");
    }

    #[test]
    fn a_payload_over_the_cap_is_refused_not_truncated() {
        let big = serde_json::json!({ "blob": "x".repeat(MAX_JOB_PAYLOAD_BYTES) });
        let error = validate_payload(&big).expect_err("超长载荷应当报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        assert!(error.to_string().contains("太大"), "{error}");
        assert!(validate_payload(&serde_json::json!({ "warId": 7 })).is_ok());
    }

    #[test]
    fn an_enqueue_without_an_idempotency_key_is_refused() {
        let request = JobRequest::new("settle-war", serde_json::json!({ "warId": 7 }), "");
        let error = validate_request(&request).expect_err("空幂等键应当报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        assert!(error.to_string().contains("幂等键"), "{error}");

        let blank = JobRequest::new("settle-war", serde_json::json!({}), "   ");
        assert!(validate_request(&blank).is_err());

        let padded = JobRequest::new("settle-war", serde_json::json!({}), " key");
        assert!(validate_request(&padded).is_err());

        let long = JobRequest::new(
            "settle-war",
            serde_json::json!({}),
            "k".repeat(MAX_JOB_IDEMPOTENCY_KEY_BYTES + 1),
        );
        assert!(validate_request(&long).is_err());
    }

    #[test]
    fn a_blank_or_padded_conversation_is_refused() {
        for conversation in ["", " ", " 12345", "12345 "] {
            let request = JobRequest::new("settle-war", serde_json::json!({}), "m1")
                .with_conversation(conversation);
            let error = validate_request(&request).expect_err("空白会话应当报错");
            assert!(error.to_string().contains("conversation"), "{error}");
        }
        let ok = JobRequest::new("settle-war", serde_json::json!({}), "m1")
            .with_conversation("987654321");
        assert!(validate_request(&ok).is_ok());
    }

    #[test]
    fn a_valid_request_round_trips_through_camel_case_json() {
        let request = JobRequest::new("settle-war", serde_json::json!({ "warId": 7 }), "m1")
            .with_conversation("987654321");
        let text = serde_json::to_string(&request).expect("能序列化");
        assert!(text.contains(r#""idempotencyKey":"m1""#), "{text}");
        assert!(text.contains(r#""conversation":"987654321""#), "{text}");
        let back: JobRequest = serde_json::from_str(&text).expect("能反序列化");
        assert_eq!(back, request);
        // 不写 conversation 的请求也必须能解析回来（#[serde(default)]）。
        let bare: JobRequest =
            serde_json::from_str(r#"{"name":"x","payload":null,"idempotencyKey":"m1"}"#)
                .expect("能解析");
        assert_eq!(bare.conversation, None);
    }

    #[test]
    fn every_state_spelling_round_trips_and_only_three_are_terminal() {
        let states = [
            JobState::Pending,
            JobState::Running,
            JobState::Done,
            JobState::Failed,
            JobState::Cancelled,
        ];
        for state in states {
            assert_eq!(JobState::parse(state.as_str()), Some(state));
        }
        assert_eq!(JobState::parse("Pending"), None);
        assert_eq!(JobState::parse("finished"), None);
        assert!(JobState::Done.is_terminal());
        assert!(JobState::Failed.is_terminal());
        assert!(JobState::Cancelled.is_terminal());
        assert!(!JobState::Pending.is_terminal());
        assert!(!JobState::Running.is_terminal());
    }

    #[test]
    fn every_error_has_a_stable_code_prefix() {
        let cases = [
            JobError::NotDeclared { capability: "jobs" },
            JobError::Unavailable {
                reason: String::from("没接队列"),
            },
            JobError::Invalid {
                message: String::from("名字为空"),
            },
            JobError::Full {
                message: String::from("已经 256 条"),
            },
            JobError::Timeout { ms: 60_000 },
            JobError::Failed {
                message: String::from("作业体抛了"),
            },
        ];
        let codes = [
            "HBOT_JOBS_NOT_DECLARED",
            "HBOT_JOBS_UNAVAILABLE",
            "HBOT_JOBS_INVALID",
            "HBOT_JOBS_FULL",
            "HBOT_JOBS_TIMEOUT",
            "HBOT_JOBS_FAILED",
        ];
        for (case, code) in cases.iter().zip(codes) {
            assert_eq!(case.code(), code);
            assert!(case.to_string().starts_with(code), "{case}");
        }
    }

    #[test]
    fn the_backoff_doubles_and_caps() {
        assert_eq!(retry_delay_ms(0), MIN_RETRY_DELAY_MS);
        assert_eq!(retry_delay_ms(1), 1_000);
        assert_eq!(retry_delay_ms(2), 2_000);
        assert_eq!(retry_delay_ms(3), 4_000);
        assert_eq!(retry_delay_ms(4), 8_000);
        assert_eq!(retry_delay_ms(9), 256_000);
        assert_eq!(retry_delay_ms(10), MAX_RETRY_DELAY_MS);
        // 再多也只是封顶，不会溢出。
        assert_eq!(retry_delay_ms(u32::MAX), MAX_RETRY_DELAY_MS);
    }

    #[test]
    fn the_host_ceiling_only_ever_tightens_the_declared_budget() {
        // 默认上限 = 核心上限：行为与没有这一步完全一致（这是「默认不改行为」的钉子）。
        assert_eq!(
            effective_job_budget_ms(DEFAULT_JOB_BUDGET_MS, MAX_JOB_BUDGET_MS),
            DEFAULT_JOB_BUDGET_MS
        );
        // 0 = 宿主不限。
        assert_eq!(
            effective_job_budget_ms(120_000, 0),
            120_000,
            "0 表示不限，不能把作业压成 0 毫秒"
        );
        // 收紧：手机端 30 秒窗口 vs 声明 60 秒。
        assert_eq!(effective_job_budget_ms(60_000, 30_000), 30_000);
        // 不放宽：上限比声明大时保留声明值。
        assert_eq!(effective_job_budget_ms(5_000, 600_000), 5_000);
        // 越界的入参两头都夹回契约区间，结果不越界。
        for (row, ceiling) in [(0_u64, 0_u64), (1, 1), (u64::MAX, u64::MAX)] {
            let budget = effective_job_budget_ms(row, ceiling);
            assert!(
                (MIN_JOB_BUDGET_MS..=MAX_JOB_BUDGET_MS).contains(&budget),
                "row={row} ceiling={ceiling} -> {budget} 越界"
            );
        }
    }
}
