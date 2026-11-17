//! Bot 状态能力（C2）在沙箱里的宿主侧装配：`ctx.kv.*` 与 `ctx.history(n)`。
//!
//! 契约类型与两个端口 trait 在 `hbot_core::bot_state`（**已冻结**）；本模块只做
//! 三件事：
//!
//! 1. [`StateSlot`] —— `Sandbox` 上的可替换槽位。宿主每次调用都来读一次，所以
//!    "换一个拒绝的实现"对下一次调用立刻生效（与 [`crate::device::DeviceSlot`]
//!    同一个理由，见 `docs/09` §4.2）；
//! 2. [`call`] —— **同步阻塞端口**的统一调用形状：`spawn_blocking` + 超时，
//!    绝不在 isolate 线程上做 SQLite IO；
//! 3. 把 [`hbot_core::BotStateError`] 转成 handler 能检查的 message（错误码前缀）。
//!
//! # 为什么槽位不是 `OnceLock`
//!
//! [`std::sync::OnceLock`] 只能写一次，而测试要换实现、宿主可能要撤销能力。
//! `Mutex<Option<..>>` 换来"每次调用读当前值"，代价是一次几乎不可能竞争的小锁。
//!
//! # 为什么每次调用都要重新查一遍装配
//!
//! `ctx.kv` 出现在 `hbot.d.ts` 里只说明**这个项目声明了**这项能力；能不能真的
//! 用还要看宿主有没有装上 [`hbot_core::BotState`]。装不上时报
//! `HBOT_STATE_UNAVAILABLE`，**不**退回一个内存实现 —— 那会让"数据在重启后就
//! 没了"变成一个没人发现的静默行为。

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hbot_core::{BotId, BotState, BotStateError, ConversationId, HistoryReader};

/// 一次状态调用的默认预算。
///
/// handler 自己的预算更短时取更短的那个（见 [`budget_of`]）：一个 2 s 的 handler
/// 不该为了读一个键等 5 s。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 宿主没装配状态能力时用的码。
pub const UNAVAILABLE_CODE: &str = "HBOT_STATE_UNAVAILABLE";

/// 参数不合法、后端失败、超时统一的码（与 [`BotStateError::Failed`] 一致）。
pub const FAILED_CODE: &str = "HBOT_STATE_FAILED";

/// 宿主注入的一次状态装配。
#[derive(Clone)]
pub struct StateHandles {
    state: Arc<dyn BotState>,
    history: Arc<dyn HistoryReader>,
}

impl StateHandles {
    /// 一份装配。两个端口分开注入：宿主可以只给 kv 不给 history（`history` 需要
    /// 读账本表，某些嵌入场景没有）。
    #[must_use]
    pub fn new(state: Arc<dyn BotState>, history: Arc<dyn HistoryReader>) -> Self {
        Self { state, history }
    }

    /// `kv` 端口。
    #[must_use]
    pub fn state(&self) -> &Arc<dyn BotState> {
        &self.state
    }

    /// `history` 端口。
    #[must_use]
    pub fn history(&self) -> &Arc<dyn HistoryReader> {
        &self.history
    }
}

impl std::fmt::Debug for StateHandles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 端口没有 `Debug`（trait object），只报形状：装了什么。
        formatter
            .debug_struct("StateHandles")
            .field("state", &"<BotState>")
            .field("history", &"<HistoryReader>")
            .finish()
    }
}

/// `Sandbox` 上的状态槽。
///
/// 除了装配本身，槽位还记着**本次事件的身份**（`botId` / `conversationId`）：
/// isolate 跨事件复用，`ctx.kv` 与 `ctx.history` 又是在 bootstrap 时装好的，
/// 所以身份只能在 [`crate::runtime::isolate`] 的 `build_event` 里逐事件写入。
/// 与 [`crate::device::DeviceSlot`] 同一个理由、同一个写法。
#[derive(Default)]
pub(crate) struct StateSlot {
    inner: Mutex<Option<StateHandles>>,
    identity: Mutex<Identity>,
}

/// 本次事件的归属：哪个 bot、哪个会话。
///
/// 两个字段分开存而不是合成一个 `Option<Identity>`：`history` 需要两个都在，
/// 而 `kv` 只要 `botId`。缺哪个就让需要它的那条路径报错，不要让"没有会话"
/// 连带把 kv 也拒掉。
#[derive(Clone, Default)]
struct Identity {
    bot: Option<BotId>,
    conversation: Option<ConversationId>,
}

impl StateSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 装入（或替换）一份装配。
    pub(crate) fn set(&self, state: Arc<dyn BotState>, history: Arc<dyn HistoryReader>) {
        *self.lock() = Some(StateHandles::new(state, history));
    }

    /// 当前装配。
    pub(crate) fn handles(&self) -> Option<StateHandles> {
        self.lock().clone()
    }

    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().is_some()
    }

    /// 记下本次事件的身份。
    ///
    /// **每次都整体覆盖**，包括覆盖成 `None`：payload 里没有 `botId` 时必须把
    /// 上一次的清掉，否则一个没有身份的事件会继承上一个事件的 bot —— 那是把
    /// "不知道"变成"默认放行"。`build_event` 在每次调用前都会走这里。
    pub(crate) fn set_current_bot(&self, bot: Option<BotId>, conversation: Option<ConversationId>) {
        let mut identity = self.identity.lock().unwrap_or_else(PoisonError::into_inner);
        identity.bot = bot;
        identity.conversation = conversation;
    }

    /// 本次事件的 `(botId, conversationId)`。
    pub(crate) fn current_identity(&self) -> (Option<BotId>, Option<ConversationId>) {
        let identity = self.identity.lock().unwrap_or_else(PoisonError::into_inner);
        (identity.bot, identity.conversation)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<StateHandles>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for StateSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateSlot")
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 把剩余事件预算换算成状态调用的预算：取两者更短的那个。
///
/// `remaining == None` 表示没有对方预算（不该发生，`Deadline` 总会给值），退回
/// [`DEFAULT_TIMEOUT`]。
#[must_use]
pub fn budget_of(remaining: Option<Duration>) -> Duration {
    match remaining {
        Some(left) => left.min(DEFAULT_TIMEOUT),
        None => DEFAULT_TIMEOUT,
    }
}

/// 在阻塞线程上执行一次状态调用，带超时。
///
/// 端口是同步的（见 `hbot_core::bot_state` 模块文档），所以这里必须在
/// `spawn_blocking` 里跑；超时后**不等**那个线程（它在后台跑完就被丢掉），因为
/// 等它等于把 isolate 也卡住。
pub(crate) async fn call<T, F>(budget: Duration, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, BotStateError> + Send + 'static,
{
    let joined = tokio::time::timeout(budget, tokio::task::spawn_blocking(work)).await;

    match joined {
        Err(_elapsed) => Err(format!(
            "{FAILED_CODE}: 状态后端在 {} 毫秒内没有返回",
            budget.as_millis()
        )),
        Ok(Err(join_error)) => {
            // 后端 panic 不许把 isolate 带下去，也不许静默：记日志、回明确失败码。
            tracing::error!(error = %join_error, "state backend task did not finish");
            Err(format!("{FAILED_CODE}: 状态后端线程异常退出"))
        }
        Ok(Ok(Ok(value))) => Ok(value),
        // `BotStateError` 的 Display 已经以错误码开头，原样交给 handler。
        Ok(Ok(Err(error))) => Err(error.to_string()),
    }
}

/// 没装配时的错误 message。
#[must_use]
pub fn unavailable(detail: &str) -> String {
    format!("{UNAVAILABLE_CODE}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::{BotId, HistoryEntry, KvEntry, KvWriteOptions};

    struct Fixed(Result<Option<serde_json::Value>, BotStateError>);

    impl BotState for Fixed {
        fn kv_get(
            &self,
            _bot_id: &BotId,
            _key: &str,
        ) -> Result<Option<serde_json::Value>, BotStateError> {
            self.0.clone()
        }

        fn kv_set(
            &self,
            _bot_id: &BotId,
            _key: &str,
            _value: serde_json::Value,
            _options: KvWriteOptions,
        ) -> Result<(), BotStateError> {
            Ok(())
        }

        fn kv_delete(&self, _bot_id: &BotId, _key: &str) -> Result<bool, BotStateError> {
            Ok(false)
        }

        fn kv_list(
            &self,
            _bot_id: &BotId,
            _prefix: Option<&str>,
        ) -> Result<Vec<String>, BotStateError> {
            Ok(Vec::new())
        }

        fn kv_set_many(
            &self,
            _bot_id: &BotId,
            _entries: Vec<KvEntry>,
            _options: KvWriteOptions,
        ) -> Result<(), BotStateError> {
            Ok(())
        }
    }

    struct NoHistory;

    impl HistoryReader for NoHistory {
        fn history(
            &self,
            _bot_id: &BotId,
            _conversation_id: &hbot_core::ConversationId,
            _limit: u32,
        ) -> Result<Vec<HistoryEntry>, BotStateError> {
            Ok(Vec::new())
        }
    }

    fn bot() -> BotId {
        "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("固定 UUID 字面量一定能解析")
    }

    #[test]
    fn the_slot_starts_empty_and_reports_when_assembled() {
        let slot = StateSlot::new();
        assert!(!slot.is_assembled());
        assert!(slot.handles().is_none());

        let state = Arc::new(Fixed(Ok(Some(serde_json::json!(1)))));
        slot.set(state, Arc::new(NoHistory));
        assert!(slot.is_assembled());
        let handles = slot.handles().expect("装配已装入");
        assert_eq!(
            handles.state().kv_get(&bot(), "k").expect("读出"),
            Some(serde_json::json!(1))
        );
    }

    #[test]
    fn replacing_the_assembly_takes_effect_on_the_next_read() {
        let slot = StateSlot::new();
        slot.set(
            Arc::new(Fixed(Ok(Some(serde_json::json!("first"))))),
            Arc::new(NoHistory),
        );
        slot.set(
            Arc::new(Fixed(Err(BotStateError::Failed {
                message: String::from("拒绝"),
            }))),
            Arc::new(NoHistory),
        );
        let handles = slot.handles().expect("装配已装入");
        let error = handles.state().kv_get(&bot(), "k").expect_err("应当失败");
        assert_eq!(error.code(), "HBOT_STATE_FAILED");
    }

    #[test]
    fn the_budget_never_exceeds_the_default() {
        assert_eq!(budget_of(None), DEFAULT_TIMEOUT);
        assert_eq!(
            budget_of(Some(Duration::from_millis(50))),
            Duration::from_millis(50)
        );
        assert_eq!(budget_of(Some(Duration::from_secs(60))), DEFAULT_TIMEOUT);
    }

    /// 手搓一个只开时间盘的 runtime。
    ///
    /// 本 crate 的 tokio features 只有 `rt` / `time`（见 `Cargo.toml`），没有
    /// `macros`，所以不能用 `#[tokio::test]`；`gate.rs` 里的超时测试是同一写法。
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("建一个只开时间盘的 runtime")
    }

    #[test]
    fn a_hanging_backend_reports_a_timeout_instead_of_blocking() {
        let outcome: Result<(), String> =
            runtime().block_on(call(Duration::from_millis(10), || {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            }));
        let message = outcome.expect_err("应当超时");
        assert!(message.starts_with(FAILED_CODE), "{message}");
    }

    #[test]
    fn a_backend_error_keeps_its_own_code() {
        let outcome: Result<(), String> = runtime().block_on(call(Duration::from_secs(1), || {
            Err(BotStateError::NotDeclared { capability: "kv" })
        }));
        let message = outcome.expect_err("应当失败");
        assert!(message.starts_with("HBOT_STATE_NOT_DECLARED"), "{message}");
    }
}
