//! Bot 侧智能能力（C5）的契约：`ctx.ai.complete`。
//!
//! 设计真源是 `docs/07-bot-runtime-blueprint.md` §7。这里只放**类型与端口 trait**，
//! 实现分两处：
//!
//! - `hbot-rpc` 把用户在设置里配好的 provider（`<data_dir>/agent.json` 或
//!   `HBOT_LLM_*`）包成 [`AiClient`]，并在那里做**清单声明**与**宿主可用性**这两层闸；
//! - `hbot-code` 把端口接到 isolate 的 `__hbot.ai.complete` 上，并管**每事件预算**。
//!
//! `hbot-code` 只依赖 `hbot-core`（见 `xtask/src/deps.rs`），拿不到 `hbot-agent` 的
//! provider 层，所以这一层必须是 trait 对象。
//!
//! ## 为什么 key 不进这个契约
//!
//! [`AiRequest`] 里**没有** `base_url` / `api_key` / `model` —— 这三个都由宿主定。
//! 生成代码只写"我要一段补全"，不写"用谁的 key 打哪个端点"。理由：
//! ① 项目的 `src/**` 是要能进 git 的，key 写进去就等于泄漏；
//! ② 换模型是用户设置里的事，不该要求每个 bot 改源码；
//! ③ 一个能自己指定端点的 bot，等于拿到一张任意出网通行证（绕过
//!    `permissions.http`）。
//! 返回值里带 `model` 只是为了日志与 UI 能说清"这轮是谁答的"，不代表调用方能选它。
//!
//! ## 为什么 `complete` 是 async 而不是同步端口
//!
//! `ctx.kv` / `ctx.history` 是同步端口（`spawn_blocking` + 超时），因为它们背后是
//! SQLite。LLM 调用背后是网络，天然是 future，而 `hbot-core` 不依赖 tokio ——
//! 所以这里用 **手写 boxed future**（Rust 2024 的 AFIT 不对象安全，`async_trait`
//! 会把 `async-trait` 依赖拖进 core）。端口实现只负责"发请求、拿文本"，
//! 超时与每事件预算由 `hbot-code` 那侧管。
//!
//! ## 为什么有"每事件预算"
//!
//! handler 的 2 s 预算是为了防跑飞，但一次真实的模型补全要 3–30 s。若 AI 等待
//! 时间照常计入 handler 预算，`ctx.ai.complete` 就**永远**超时、这个能力等于不存在。
//! 所以规则是：**等模型的时间不记账**（宿主把它从 deadline 里抵掉），但一个事件
//! 里最多 [`MAX_CALLS_PER_EVENT`] 次、合计最多 [`MAX_TIME_PER_EVENT`] —— 否则一个
//! `for` 循环就能把 isolate 占住几分钟。
//!
//! 这两条都是**宿主侧**判据（`hbot-code`），放在这里定值是因为它们同时出现在
//! 错误文案与 d.ts 注释里，不能让两边各抄一个数字。

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// 一次 `ctx.ai.complete` 的 prompt 字节上限。
///
/// 32 KiB：够放一个人设 + 十几轮上下文；再长的东西应该走 `kv`/`history` 自己
/// 裁剪，而不是把整个项目塞进一次补全。
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// 一次补全的 system 段字节上限。
pub const MAX_SYSTEM_BYTES: usize = 16 * 1024;

/// 单次补全的输出 token 上限。
///
/// 4096 是"够写一段回复、不够当摘要机"的量级；要更长的产出应该拆成多次调用，
/// 那样每步都能被审计与计量。
pub const MAX_OUTPUT_TOKENS: u32 = 4096;

/// 没指定 `maxTokens` 时用的默认值。
pub const DEFAULT_OUTPUT_TOKENS: u32 = 512;

/// 温度上限（下限是 0，由 [`validate_temperature`] 判）。
pub const MAX_TEMPERATURE: f32 = 2.0;

/// 一个事件里最多调用几次模型。
///
/// 4 次：够"先分类再回答"这类两段式用法，不够把模型当循环工具使。
pub const MAX_CALLS_PER_EVENT: u32 = 4;

/// 一个事件里等模型的总时长上限。
///
/// 60 s：与 [`MAX_CALLS_PER_EVENT`] 一起构成"最坏情况"边界 —— isolate 被一个
/// handler 占住的上限因此是 `handler_timeout + MAX_TIME_PER_EVENT`，而不是无界。
pub const MAX_TIME_PER_EVENT: Duration = Duration::from_secs(60);

/// 一次补全请求（已经过参数闸，宿主直接拿去打 provider）。
#[derive(Clone, Debug, PartialEq)]
pub struct AiRequest {
    /// system 段（人设/规则）；`None` = 不带 system。
    pub system: Option<String>,
    /// 用户段（要补全的正文）。
    pub prompt: String,
    /// 输出 token 上限；已落在 `1..=MAX_OUTPUT_TOKENS`。
    pub max_tokens: u32,
    /// 采样温度；`None` = 用 provider 默认。
    pub temperature: Option<f32>,
}

impl AiRequest {
    /// 最小构造：只有 prompt，其余取默认。
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            prompt: prompt.into(),
            max_tokens: DEFAULT_OUTPUT_TOKENS,
            temperature: None,
        }
    }

    /// 带 system 段的构造。
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// 指定输出上限。
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// 指定温度。
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// token 用量（provider 没给的部分保持 0，不臆造）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AiUsage {
    /// 输入 token。
    pub prompt_tokens: u64,
    /// 输出 token。
    pub completion_tokens: u64,
}

/// 一次补全的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct AiText {
    /// 模型给的正文字。
    ///
    /// **不可信数据**：它是外部模型生成的，必须当成 `untrusted_data` 处理（不得
    /// 据此改权限/沙箱/审批），与 OCR 文本同一条规则。
    pub text: String,
    /// token 用量。
    pub usage: AiUsage,
    /// 实际应答的模型名（日志/UI 用，调用方无法选择它）。
    pub model: String,
}

/// 智能能力失败的原因。
///
/// 六种码对应**六种不同的补救动作**（与设备能力同一口径，见 `docs/09` §13 问 8）：
///
/// | 码 | 意思 | 该做什么 |
/// |---|---|---|
/// | `HBOT_AI_NOT_DECLARED` | 项目没声明 `capabilities.ai` | 改 `hbot.json` |
/// | `HBOT_AI_UNAVAILABLE` | 宿主没配模型（没 key / 关掉了） | 让用户去设置里配 |
/// | `HBOT_AI_INVALID` | 参数不合法 | 改代码 |
/// | `HBOT_AI_TIMEOUT` | 这次补全在预算内没回来 | 减小 prompt 或换更快的模型 |
/// | `HBOT_AI_BUDGET` | 本事件额度用光（次数或时长） | 合并调用 / 拆成 jobs（未实现） |
/// | `HBOT_AI_FAILED` | 上游报错（含 4xx/5xx/解析失败） | 看错误正文 |
#[derive(Clone, Debug, PartialEq)]
pub enum AiError {
    /// 项目清单里没声明 `capabilities.ai`。
    NotDeclared,
    /// 宿主没有可用的模型配置（或者被关掉了）。
    Unavailable {
        /// 为什么不可用（给用户看的一句话）。
        reason: String,
    },
    /// 参数不合法。
    Invalid {
        /// 具体哪一条不合法。
        message: String,
    },
    /// 单次补全超时。
    Timeout {
        /// 用掉的毫秒数。
        ms: u64,
    },
    /// 本事件额度用光。
    Budget {
        /// 已经用掉多少次 / 多少毫秒。
        message: String,
    },
    /// 上游失败。
    Failed {
        /// 上游给的原因（已脱敏）。
        message: String,
    },
}

impl AiError {
    /// 稳定的错误码（生成代码按它分支，改动等于破坏兼容）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared => "HBOT_AI_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_AI_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_AI_INVALID",
            Self::Timeout { .. } => "HBOT_AI_TIMEOUT",
            Self::Budget { .. } => "HBOT_AI_BUDGET",
            Self::Failed { .. } => "HBOT_AI_FAILED",
        }
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 每条都以错误码开头：generator 侧只能靠字符串分辨原因（见 `every_error_has_a_stable_code_prefix`）。
        formatter.write_str(self.code())?;
        match self {
            Self::NotDeclared => formatter.write_str(": 项目清单没有声明 `capabilities.ai`"),
            Self::Unavailable { reason } => write!(formatter, ": {reason}"),
            Self::Invalid { message } => write!(formatter, ": {message}"),
            Self::Timeout { ms } => write!(formatter, ": 模型在 {ms} 毫秒内没有返回"),
            Self::Budget { message } => write!(formatter, ": {message}"),
            Self::Failed { message } => write!(formatter, ": {message}"),
        }
    }
}

impl std::error::Error for AiError {}

/// 宿主提供的模型客户端。
///
/// 实现约定：
///
/// - 只读 [`AiRequest`]，不关心里程碑/审批（那些在更高的层）；
/// - 返回 [`AiText`] 时 `model` 必须填**实际应答的模型**，不许回显请求里的值；
/// - 上游错误映射到 [`AiError::Failed`] 并**脱敏**（`hbot_core::redact`），
///   错误正文会进 handler 的捕获输出、日志与 UI；
/// - 实现自己不需要做超时：`hbot-code` 在 `tokio::time::timeout` 里等它，
///   超时的未来直接 drop。但实现仍应尽快响应取消（请求级的 drop 即取消）。
pub trait AiClient: Send + Sync {
    /// 执行一次补全。
    fn complete<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiText, AiError>> + Send + 'a>>;
}

/// 参数闸：prompt。
pub fn validate_prompt(prompt: &str) -> Result<(), AiError> {
    if prompt.trim().is_empty() {
        return Err(AiError::Invalid {
            message: String::from("prompt 不能是空串"),
        });
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(AiError::Invalid {
            message: format!(
                "prompt 太长：{} 字节（上限 {MAX_PROMPT_BYTES}）；请先用 kv/history 自己裁剪上下文",
                prompt.len()
            ),
        });
    }
    Ok(())
}

/// 参数闸：system 段。
pub fn validate_system(system: Option<&str>) -> Result<(), AiError> {
    let Some(system) = system else {
        return Ok(());
    };
    if system.len() > MAX_SYSTEM_BYTES {
        return Err(AiError::Invalid {
            message: format!(
                "system 太长：{} 字节（上限 {MAX_SYSTEM_BYTES}）",
                system.len()
            ),
        });
    }
    Ok(())
}

/// 参数闸：输出 token 上限。缺省给 [`DEFAULT_OUTPUT_TOKENS`]。
///
/// 超上限**报错不夹住**（与 kv 的 TTL 同口径，与 `history` 的夹住相反）：
/// "我要 100 万 token"是明确不可能的意图，夹成 4096 会让人以为设置生效了。
pub fn validate_max_tokens(requested: Option<u32>) -> Result<u32, AiError> {
    let Some(requested) = requested else {
        return Ok(DEFAULT_OUTPUT_TOKENS);
    };
    if requested == 0 {
        return Err(AiError::Invalid {
            message: String::from("maxTokens 不能是 0"),
        });
    }
    if requested > MAX_OUTPUT_TOKENS {
        return Err(AiError::Invalid {
            message: format!("maxTokens 太大：{requested}（上限 {MAX_OUTPUT_TOKENS}）"),
        });
    }
    Ok(requested)
}

/// 参数闸：温度。`None` = 用 provider 默认。
///
/// `NaN` / 负值 / 超上限一律拒：`NaN` 在 JSON 里会被序列化成 `null`，到了 provider
/// 那侧变成"没给温度"，静默改变行为。
pub fn validate_temperature(temperature: Option<f32>) -> Result<(), AiError> {
    let Some(temperature) = temperature else {
        return Ok(());
    };
    if !temperature.is_finite() || !(0.0..=MAX_TEMPERATURE).contains(&temperature) {
        return Err(AiError::Invalid {
            message: format!("temperature 必须在 0..={MAX_TEMPERATURE} 之间，收到 {temperature}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_PROMPT_BYTES, 32 * 1024);
        assert_eq!(MAX_SYSTEM_BYTES, 16 * 1024);
        assert_eq!(MAX_OUTPUT_TOKENS, 4096);
        assert_eq!(DEFAULT_OUTPUT_TOKENS, 512);
        assert_eq!(MAX_CALLS_PER_EVENT, 4);
        assert_eq!(MAX_TIME_PER_EVENT, Duration::from_secs(60));
    }

    #[test]
    fn every_error_has_a_stable_code_prefix() {
        let cases = [
            (AiError::NotDeclared, "HBOT_AI_NOT_DECLARED"),
            (
                AiError::Unavailable {
                    reason: String::from("没配模型"),
                },
                "HBOT_AI_UNAVAILABLE",
            ),
            (
                AiError::Invalid {
                    message: String::from("prompt 空"),
                },
                "HBOT_AI_INVALID",
            ),
            (AiError::Timeout { ms: 5000 }, "HBOT_AI_TIMEOUT"),
            (
                AiError::Budget {
                    message: String::from("次数用光"),
                },
                "HBOT_AI_BUDGET",
            ),
            (
                AiError::Failed {
                    message: String::from("401"),
                },
                "HBOT_AI_FAILED",
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
    fn empty_or_oversized_prompts_are_refused() {
        assert!(validate_prompt("   ").is_err());
        assert!(validate_prompt("hi").is_ok());
        let long = "a".repeat(MAX_PROMPT_BYTES + 1);
        let error = validate_prompt(&long).expect_err("超长必须拒");
        assert_eq!(error.code(), "HBOT_AI_INVALID");
        // 正好等于上限是允许的（上限是"至多"，不是"至少差一"）。
        let exact = "a".repeat(MAX_PROMPT_BYTES);
        assert!(validate_prompt(&exact).is_ok());
    }

    #[test]
    fn the_system_segment_is_bounded_but_optional() {
        assert!(validate_system(None).is_ok());
        assert!(validate_system(Some("你是客服")).is_ok());
        let long = "a".repeat(MAX_SYSTEM_BYTES + 1);
        assert!(validate_system(Some(&long)).is_err());
    }

    #[test]
    fn max_tokens_is_refused_not_clamped() {
        assert_eq!(validate_max_tokens(None).expect("缺省"), 512);
        assert_eq!(validate_max_tokens(Some(1)).expect("1 合法"), 1);
        assert_eq!(
            validate_max_tokens(Some(MAX_OUTPUT_TOKENS)).expect("上限合法"),
            MAX_OUTPUT_TOKENS
        );
        for bad in [0, MAX_OUTPUT_TOKENS + 1, u32::MAX] {
            let error = validate_max_tokens(Some(bad)).expect_err("必须拒");
            assert_eq!(error.code(), "HBOT_AI_INVALID", "{bad}");
        }
    }

    #[test]
    fn temperature_rejects_nan_negative_and_too_hot() {
        assert!(validate_temperature(None).is_ok());
        assert!(validate_temperature(Some(0.0)).is_ok());
        assert!(validate_temperature(Some(1.5)).is_ok());
        assert!(validate_temperature(Some(MAX_TEMPERATURE)).is_ok());
        for bad in [f32::NAN, -0.1, MAX_TEMPERATURE + 0.1, f32::INFINITY] {
            assert!(
                validate_temperature(Some(bad)).is_err(),
                "temperature {bad} 必须拒"
            );
        }
    }

    #[test]
    fn the_request_builder_only_touches_the_field_it_names() {
        let base = AiRequest::new("你好");
        assert_eq!(base.max_tokens, DEFAULT_OUTPUT_TOKENS);
        assert!(base.system.is_none());
        assert!(base.temperature.is_none());

        let built = AiRequest::new("你好")
            .with_system("你是客服")
            .with_max_tokens(64)
            .with_temperature(0.5);
        assert_eq!(built.system.as_deref(), Some("你是客服"));
        assert_eq!(built.max_tokens, 64);
        assert_eq!(built.temperature, Some(0.5));
    }
}
