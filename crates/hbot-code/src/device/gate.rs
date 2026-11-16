//! **唯一的**设备闸（`docs/09-device-capability.md` §2「两个消费者，一份闸」）。
//!
//! 两个消费者共享这一份判据与顺序，谁都不许再写一遍：
//!
//! 1. 生成代码的 `ctx.device.*` —— 宿主函数在 [`crate::runtime::host_api`] 里，
//!    每次调用都先过 [`crate::device::DeviceSlot::authorize`]，也就是这里；
//! 2. Agent 工具的 `ToolGateway` 第一步 —— 在 `crates/hbot-agent`（**不在本
//!    crate 的写域**），它拿同一个 [`DeviceGate`] 调同一个 [`authorize`]。
//!
//! # 顺序（每一条设备调用都一样，不许换）
//!
//! 1. **沙箱档**：`read-only` 一律拒，并且**不碰任何 OS 资源**（不开 socket、
//!    不调后端）。换成第二条判据就会重演 `AGENTS.md` 第 8 条那个缺口 ——只读档
//!    下照样能写盘；
//! 2. **装配**：宿主没注入 `DeviceGate`/`DeviceBackend` 就报
//!    `HBOT_DEVICE_UNAVAILABLE`，绝不静默返回空数组（不伪造成功）；
//! 3. **机器人身份**：没有 `botId` 的事件不知道问谁的会话，报
//!    `HBOT_DEVICE_FAILED`；
//! 4. **清单声明 → 宿主设置 → 限时会话**：三层**一次**问进去，
//!    [`DeviceGate::authorize`] 自己知道该报 `NotDeclared` / `NotEnabled` /
//!    `NotGranted`。本 crate 不复制这三层，也不缓存结论 ——
//!    `docs/09` §4.2：撤销必须对**下一次调用**生效，不许在 spawn 时快照；
//! 5. **后端调用**：[`call`]，`spawn_blocking` + 超时，绝不在 isolate 线程阻塞。
//!
//! # 错误形状
//!
//! handler 拿到的是 JS 异常（`Exception::throw_message`），message 以
//! [`DeviceError::code`] 开头（形如 `HBOT_DEVICE_NOT_GRANTED: ...`）。这句
//! message 是 handler 唯一能检查的东西，所以前缀属于契约。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use hbot_core::{
    BotId, DeviceBackend, DeviceCapability, DeviceError, DeviceGate, DeviceTarget, SandboxMode,
};

/// 一次设备后端调用默认最多等多久。
///
/// handler 自己的预算更短时取更短的那个（[`budget_of`]）：一个 2 s 的 handler
/// 不该为了截屏等 10 s。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// 参数不合法 / 后端线程异常时用的码。
///
/// 与 [`DeviceError::Failed`] 的 `code()` 同一个字符串：handler 那边看到的就是
/// 一种"这次调用失败了"的前缀，不需要区分是参数错还是后端错。
pub const FAILED_CODE: &str = "HBOT_DEVICE_FAILED";

/// 没装配设备能力时的码（`Sandbox::set_device` 从未被调用）。
pub const UNAVAILABLE_CODE: &str = "HBOT_DEVICE_UNAVAILABLE";

/// "这次不给"的码：read-only 档与"没有生效中的会话"共用它。
///
/// `DeviceError::NotGranted` 的 `code()` 就是这一个；read-only 档刻意复用，因为
/// handler 能做的补救动作是同一个 —— 去开个会话说清楚，而不是换个 API。
pub const NOT_GRANTED_CODE: &str = "HBOT_DEVICE_NOT_GRANTED";

/// 设备调用为什么没发生。
///
/// 这是"本 crate 的闸"的错误，不是端口的错误：
/// [`DeviceFailure::Denied`] / [`DeviceFailure::Backend`] 里包着的才是
/// [`DeviceError`]，其余三种是这台宿主在端口之外自己判的
/// （没装配 / 只读档 / 没有机器人身份）。
#[derive(Clone, Debug)]
pub enum DeviceFailure {
    /// 沙箱没有装配设备能力（`Sandbox::set_device` 从未被调用）。
    Unavailable,
    /// 沙箱是 `read-only` 档：设备能力一律拒绝，与 `permissions.device` 无关。
    ReadOnly {
        /// 被请求的子能力。
        capability: DeviceCapability,
    },
    /// 本次事件没有 `botId`，判不了"哪个机器人的会话"。
    NoBotIdentity,
    /// 清单 / 宿主设置 / 会话任一层拒绝。
    Denied(DeviceError),
    /// 过了闸，但后端自己失败（超时、线程异常、系统调用错误）。
    Backend(DeviceError),
}

impl DeviceFailure {
    /// 稳定错误前缀，就是 handler 在异常 message 里看到的那一段。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => UNAVAILABLE_CODE,
            // 只读档和"没会话"在 handler 看来是同一件事：这次不给。
            Self::ReadOnly { .. } => NOT_GRANTED_CODE,
            Self::NoBotIdentity => FAILED_CODE,
            Self::Denied(error) | Self::Backend(error) => error.code(),
        }
    }

    /// 完整的、可以原样扔给 `Exception::throw_message` 的文本。
    ///
    /// 端口错误直接用它的 [`fmt::Display`]（已经是 `CODE: 中文说明`），其余
    /// 三种在这里拼；**任何分支都不许把参数内容（剪贴板文本、注入的字符串）
    /// 拼进来** —— 异常 message 会进 RunLog、面板和模型上下文（`docs/09` §3
    /// 的审计红线）。
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Denied(error) | Self::Backend(error) => error.to_string(),
            Self::Unavailable => {
                format!("{UNAVAILABLE_CODE}: 设备能力尚未装配")
            }
            Self::ReadOnly { capability } => format!(
                "{NOT_GRANTED_CODE}: 沙箱是 read-only 档，设备能力一律拒绝（请求 `{capability}`，与 permissions.device 无关）"
            ),
            Self::NoBotIdentity => format!(
                "{}: 本次事件没有 botId，无法判定设备授权",
                Self::NoBotIdentity.code()
            ),
        }
    }
}

impl fmt::Display for DeviceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for DeviceFailure {}

/// **一份闸**：判"这次设备调用允不允许"。
///
/// 五个参数就是上面那五步的输入；除了 [`SandboxMode`] 之外全是借用，调用方
/// 保留所有权（`DeviceHandles` 每次调用都克隆一份，不跨 `await` 持锁）。
///
/// 返回值只有"允许/拒绝"，因为**执行**是 [`call`] 的事：两个消费者在闸之后做
/// 的事不同（一个回 JS，一个回 RPC），共用的是判据而不是动作。
///
/// # Errors
///
/// [`DeviceFailure::ReadOnly`]（沙箱档）、[`DeviceFailure::Unavailable`]（没装配）、
/// [`DeviceFailure::NoBotIdentity`]（没有 `botId`）、[`DeviceFailure::Denied`]
/// （清单/设置/会话拒绝了，里面是端口给的 [`DeviceError`]）。
pub fn authorize(
    mode: SandboxMode,
    gate: Option<&dyn DeviceGate>,
    bot_id: Option<&BotId>,
    capability: DeviceCapability,
    target: &DeviceTarget,
) -> Result<(), DeviceFailure> {
    // ① 沙箱档先判：read-only 下连"清单声明了什么"都不用看，更不碰 OS。
    if mode == SandboxMode::ReadOnly {
        return Err(DeviceFailure::ReadOnly { capability });
    }

    // ② 没装配就是没有这个能力。这里**不**返回空结果：handler 看到
    //    `HBOT_DEVICE_UNAVAILABLE` 才会去查自己是不是被误配了。
    let Some(gate) = gate else {
        return Err(DeviceFailure::Unavailable);
    };

    // ③ 没有机器人身份就判不了会话；猜一个（比如"随便一个"）是把授权判据
    //    变成默认通过。
    let Some(bot_id) = bot_id else {
        return Err(DeviceFailure::NoBotIdentity);
    };

    // ④ 清单声明 / 宿主设置 / 会话：一次问进去，不复制、不缓存。
    gate.authorize(bot_id, capability, target)
        .map_err(DeviceFailure::Denied)
}

/// 本次调用的时间预算：默认 [`DEFAULT_TIMEOUT`] 与 handler 剩余预算的较小者。
///
/// `remaining` 是 [`crate::limits::Deadline::remaining`] 的返回值；`None` 表示
/// 当前没有预算限制（例如测试里没装 deadline），这时用默认值。
#[must_use]
pub fn budget_of(remaining: Option<Duration>) -> Duration {
    remaining.map_or(DEFAULT_TIMEOUT, |left| left.min(DEFAULT_TIMEOUT))
}

/// 过闸之后的**执行**：把阻塞的后端调用挪到 `spawn_blocking`，并给它上超时。
///
/// 三条硬约束（`docs/09` §5）：阻塞调用绝不在 isolate 线程做；超时必须有上界；
/// 后端不给"该不该"的答案 —— 该不该已经在 [`authorize`] 判完了，这里只把失败
/// 归一成 [`DeviceFailure::Backend`]。
///
/// # Errors
///
/// [`DeviceFailure::Backend`]，三种来源：超时（`HBOT_DEVICE_TIMEOUT`）、
/// 后端线程 panic（`HBOT_DEVICE_FAILED`）、后端返回的 [`DeviceError`]。
pub async fn call<T, F>(
    backend: Arc<dyn DeviceBackend>,
    budget: Duration,
    work: F,
) -> Result<T, DeviceFailure>
where
    T: Send + 'static,
    F: FnOnce(&dyn DeviceBackend) -> Result<T, DeviceError> + Send + 'static,
{
    let joined =
        tokio::time::timeout(budget, tokio::task::spawn_blocking(move || work(&*backend))).await;

    match joined {
        // 超时不是"后端坏了"，也不是"宿主不支持"：报一个**独立的**超时码。
        // 三种码对应三种补救动作（换机器 / 加预算 / 等一等），合成 `HOST_LIMIT`
        // 会让用户以为这台机器根本做不到设备能力（`docs/09` §13 问 8）。
        Err(_elapsed) => Err(DeviceFailure::Backend(DeviceError::Timeout {
            ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
        })),
        Ok(Err(join_error)) => {
            // 后端 panic 不能把 isolate 线程带下去，也不能吞掉：记日志，回一个
            // 明确的失败码。**日志里不带参数内容**（可能含剪贴板文本）。
            tracing::error!(error = %join_error, "device backend task did not finish");
            Err(DeviceFailure::Backend(DeviceError::Failed {
                message: "设备后端线程异常退出".to_owned(),
            }))
        }
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(error))) => Err(DeviceFailure::Backend(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Option<DeviceError>);

    impl DeviceGate for Fixed {
        fn authorize(
            &self,
            _bot_id: &BotId,
            _capability: DeviceCapability,
            _target: &DeviceTarget,
        ) -> Result<(), DeviceError> {
            match &self.0 {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    /// 一个"全拒"的闸：每个子能力都报 `NotDeclared`。
    fn declaring_nothing() -> Fixed {
        Fixed(Some(DeviceError::NotDeclared {
            capability: DeviceCapability::Screen,
        }))
    }

    fn bot() -> BotId {
        // `uuid` 不是本 crate 的依赖，所以走 `BotId: FromStr`（`define_id!` 给的）。
        "00000000-0000-0000-0000-000000000001"
            .parse::<BotId>()
            .expect("固定 UUID 字面量一定能解析")
    }

    /// 一个只会睡觉的后端：用来证明"预算用完了"报的是超时码，不是宿主限制。
    struct Slow;

    impl DeviceBackend for Slow {
        fn capabilities(&self) -> hbot_core::DeviceCapabilityReport {
            hbot_core::DeviceCapabilityReport::default()
        }

        fn screenshot(
            &self,
            _request: &hbot_core::ScreenshotRequest,
        ) -> Result<hbot_core::Screenshot, DeviceError> {
            Err(DeviceError::HostLimit {
                detail: "not used".to_owned(),
            })
        }

        fn ocr(&self, _request: &hbot_core::OcrRequest) -> Result<hbot_core::OcrText, DeviceError> {
            std::thread::sleep(Duration::from_millis(120));
            Ok(hbot_core::OcrText {
                text: "too late".to_owned(),
                truncated: false,
            })
        }

        fn input(
            &self,
            _action: &hbot_core::InputAction,
            _apps: &[String],
        ) -> Result<(), DeviceError> {
            Err(DeviceError::HostLimit {
                detail: "not used".to_owned(),
            })
        }

        fn windows(&self) -> Result<Vec<hbot_core::WindowInfo>, DeviceError> {
            Err(DeviceError::HostLimit {
                detail: "not used".to_owned(),
            })
        }

        fn clipboard(
            &self,
            _op: &hbot_core::ClipboardOp,
        ) -> Result<hbot_core::ClipboardValue, DeviceError> {
            Err(DeviceError::HostLimit {
                detail: "not used".to_owned(),
            })
        }
    }

    /// 超时必须是**自己的**码（`docs/09` §13 问 8）：`HOST_LIMIT` 会让用户以为
    /// 这台机器不支持设备能力，而实际上只是这次预算太短。
    #[test]
    fn a_call_that_outlives_its_budget_reports_a_timeout_not_a_host_limit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread runtime");
        let failure = runtime
            .block_on(call(Arc::new(Slow), Duration::from_millis(10), |backend| {
                backend.ocr(&hbot_core::OcrRequest {
                    target: DeviceTarget::None,
                    max_chars: 100,
                })
            }))
            .expect_err("120 ms 的工作给了 10 ms，必须超时");
        assert_eq!(failure.code(), "HBOT_DEVICE_TIMEOUT");
        assert!(
            failure.message().contains("10 毫秒"),
            "文案要带上预算：{}",
            failure.message()
        );
    }

    #[test]
    fn read_only_wins_over_everything_else() {
        // 没装配、没身份、闸还全拒 —— 只读档仍然必须第一个报，且报的是"拒绝"而不是
        // "没装配"。这是 AGENTS.md 第 8 条的判据顺序。
        let failure = authorize(
            SandboxMode::ReadOnly,
            None,
            None,
            DeviceCapability::Screen,
            &DeviceTarget::None,
        )
        .expect_err("只读档必须拒");
        assert_eq!(failure.code(), "HBOT_DEVICE_NOT_GRANTED");
        assert!(matches!(failure, DeviceFailure::ReadOnly { .. }));

        let gate = declaring_nothing();
        let failure = authorize(
            SandboxMode::ReadOnly,
            Some(&gate),
            Some(&bot()),
            DeviceCapability::Input,
            &DeviceTarget::None,
        )
        .expect_err("只读档即使闸全允许也必须拒");
        assert_eq!(failure.code(), "HBOT_DEVICE_NOT_GRANTED");
    }

    #[test]
    fn a_missing_slot_is_unavailable_not_an_empty_result() {
        let gate = Fixed(None);
        let failure = authorize(
            SandboxMode::WorkspaceWrite,
            None,
            Some(&bot()),
            DeviceCapability::Clipboard,
            &DeviceTarget::None,
        )
        .expect_err("没装配必须拒");
        assert_eq!(failure.code(), "HBOT_DEVICE_UNAVAILABLE");
        assert!(
            failure
                .message()
                .starts_with("HBOT_DEVICE_UNAVAILABLE: 设备能力尚未装配"),
            "message 是 handler 唯一的判据：{}",
            failure.message()
        );

        // 反向确认：装配了、身份也给了、闸允许 —— 这条通路是通的（否则上面那条
        // 断言可能只是因为别的分支先失败）。
        authorize(
            SandboxMode::WorkspaceWrite,
            Some(&gate),
            Some(&bot()),
            DeviceCapability::Clipboard,
            &DeviceTarget::None,
        )
        .expect("闸允许时应通过");
    }

    #[test]
    fn a_missing_bot_id_does_not_default_to_someone_elses_session() {
        let gate = Fixed(None);
        let failure = authorize(
            SandboxMode::Danger,
            Some(&gate),
            None,
            DeviceCapability::Window,
            &DeviceTarget::None,
        )
        .expect_err("没有 botId 必须拒");
        assert_eq!(failure.code(), "HBOT_DEVICE_FAILED");
    }

    #[test]
    fn the_gates_own_codes_pass_through_unchanged() {
        // 三种"声明/设置/会话"的拒绝必须原样透出：UI 靠 `NotEnabled` 的 toggle
        // 画"去打开"的按钮，换成别的码就丢了。
        for (error, code) in [
            (
                DeviceError::NotDeclared {
                    capability: DeviceCapability::Screen,
                },
                "HBOT_DEVICE_NOT_DECLARED",
            ),
            (
                DeviceError::NotEnabled {
                    toggle: "device.enabled".to_owned(),
                },
                "HBOT_DEVICE_NOT_ENABLED",
            ),
            (DeviceError::Revoked, "HBOT_DEVICE_REVOKED"),
        ] {
            let gate = Fixed(Some(error));
            let failure = authorize(
                SandboxMode::WorkspaceWrite,
                Some(&gate),
                Some(&bot()),
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect_err("闸拒了");
            assert_eq!(failure.code(), code);
            assert!(matches!(failure, DeviceFailure::Denied(_)));
        }
    }

    #[test]
    fn the_budget_never_exceeds_the_default_and_follows_a_shorter_handler() {
        assert_eq!(budget_of(None), DEFAULT_TIMEOUT);
        assert_eq!(
            budget_of(Some(Duration::from_millis(2))),
            Duration::from_millis(2)
        );
        assert_eq!(budget_of(Some(Duration::from_secs(600))), DEFAULT_TIMEOUT);
    }
}
