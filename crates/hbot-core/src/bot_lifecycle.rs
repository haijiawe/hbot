//! 生命周期钩子（`onStart` / `onStop`）的契约：阶段、停机原因与预算。
//!
//! 设计真源是 `docs/07-bot-runtime-blueprint.md` §4.1。两个钩子各只有一件事要回答：
//!
//! - `onStart`：**这一次启动成不成立**。它失败就等于 `bot_start` 失败（回滚到 stopped），
//!   而不是"bot 起来了但没初始化完"—— 后者会让每一条后续消息都在一个半初始化的状态里跑。
//! - `onStop`：**停机原因是什么**。`user`（用户点了停止）/ `shutdown`（进程要退了）/
//!   `crash`（supervisor 判定它崩了）。三者能做的事不同：`user` 可以存档、`shutdown` 只能
//!   尽快、`crash` 拿到的状态本来就已经不可信。
//!
//! # 为什么 `onStop` 的预算比 handler 短
//!
//! handler 有 2 秒（`limits.handlerMs`），`onStop` 只给 5 秒 —— 但它的**性质**不同：handler
//! 的 2 秒是"不能拖住会话"，`onStop` 的 5 秒是"不能拖住停机"。停机路径上有人在等
//! （`bot_stop` 的返回、进程退出），所以它必须有一个**比任何业务动作都短**的硬上界；
//! 5 秒够写一次存档，不够跑一次完整结算 —— 那是 `ctx.jobs` 的事。
//!
//! # 为什么没有 `onStop` 的返回值
//!
//! 停机时没有任何人在等结果：`bot_stop` 已经成功、进程可能正在退出。给它一个返回值只会
//! 诱使调用方去判断"停得好不好"，而那件事无法恢复。所以失败只落一行 error 日志。

use std::fmt;

use serde::{Deserialize, Serialize};

/// `onStop` 的预算上限：5 秒。
///
/// 与 handler 预算分开定：那个是"不拖住会话"，这个是"不拖住停机"。`bot_stop` 等它、
/// 进程退出也等它，所以它必须比任何业务动作都短。
pub const STOP_BUDGET: std::time::Duration = std::time::Duration::from_millis(5_000);

/// 停机原因（`onStop` 的第二个参数）。
///
/// 拼写是契约：日志、d.ts 与 `docs/07` §4.1 三处共用这一份 [`StopReason::as_str`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 用户点了停止（`bot_stop`）：最"干净"的一种，存档、告别消息都可以做。
    User,
    /// 进程要退了（关窗、Ctrl-C、systemd stop）：能做的只有尽快收尾。
    Shutdown,
    /// supervisor 判定它崩了（连续失败到降级）：拿到的状态本来就不可信。
    Crash,
}

impl StopReason {
    /// 日志与 JS 侧看到的字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Shutdown => "shutdown",
            Self::Crash => "crash",
        }
    }

    /// 从字符串读回；未知值返回 `None`。
    ///
    /// 不默认成 `User`：把一次崩溃报成"用户点的停止"，会让 `onStop` 里的存档逻辑
    /// 从一个已经不可信的状态里读数据。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "shutdown" => Some(Self::Shutdown),
            "crash" => Some(Self::Crash),
            _ => None,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 生命周期钩子属于哪个阶段。
///
/// 一个枚举而不是两个 `Command` 变体：两者的预算、ctx 与失败处理都不同，但**派发路径**
/// 完全一样（编译 → 取 worker → 在 isolate 上跑一个可选钩子）。分开写会复制那份路径，
/// 而它正好是最容易写错的一段（槽位守卫、崩溃预算、日志收集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecyclePhase {
    /// 启动：失败即 `bot_start` 失败。
    Start,
    /// 停机：失败只落一行 error 日志。
    Stop,
}

impl LifecyclePhase {
    /// 日志里用的名字。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }

    /// 对应的钩子名（缺钩子时的错误文案要用它点名）。
    #[must_use]
    pub fn hook_name(self) -> &'static str {
        match self {
            Self::Start => "onStart",
            Self::Stop => "onStop",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_round_trip_through_their_strings() {
        for reason in [StopReason::User, StopReason::Shutdown, StopReason::Crash] {
            assert_eq!(StopReason::parse(reason.as_str()), Some(reason));
            assert_eq!(reason.to_string(), reason.as_str());
        }
        // 读不懂的原因绝不能默认成 User：把崩溃报成用户点的停止会让存档读到坏状态。
        assert_eq!(StopReason::parse("weird"), None);
    }

    #[test]
    fn the_stop_budget_is_five_seconds() {
        assert_eq!(STOP_BUDGET, std::time::Duration::from_millis(5_000));
    }

    #[test]
    fn phases_name_their_hook() {
        assert_eq!(LifecyclePhase::Start.hook_name(), "onStart");
        assert_eq!(LifecyclePhase::Stop.hook_name(), "onStop");
        assert_eq!(LifecyclePhase::Start.as_str(), "start");
        assert_eq!(LifecyclePhase::Stop.as_str(), "stop");
    }
}
