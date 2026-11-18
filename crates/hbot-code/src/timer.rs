//! 定时器槽位：声明好的计划表 + "下一次何时触发"的记账。
//!
//! 与 `ai.rs` / `state.rs` 同一形状：`Sandbox::spawn` 的签名是冻结的，所以能力只能
//! 通过**槽位**进 isolate —— 出生时是空的（`spawn_with_http` 建 `TimerSlot::new()`），
//! 装配方（`hbot-rpc` 的 `SandboxHandler::worker`）在 isolate 起来之后按清单写进去。
//!
//! ## 谁在等这个钟
//!
//! 不是 isolate 自己（QuickJS 里没有定时器，生成代码也不许碰 `setTimeout`）：
//! `hbot-rpc` 的 supervisor 每秒问一次 [`TimerSlot::due`]，拿到名字就派发一次
//! [`Sandbox::dispatch_timer`](crate::runtime::Sandbox::dispatch_timer)。这样：
//!
//! * **时钟只有一个**（宿主的 `Counter`/`Instant`），不依赖 JS 引擎；
//! * 错过的触发**不补跑**（见 [`TimerSlot::due`] 的注释），机器休眠回来不会一次喷出
//!   几十条"每日重置"；
//! * 计划表改动（热重载换清单）只要重新 [`TimerSlot::set_schedules`]，下一次 tick 就生效。

use std::fmt;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use hbot_core::{DueSchedule, ScheduleSpec, Timestamp};

/// `ctx.reply` 在定时器里没有目标会话时抛出的码。
///
/// 定时器没有入站消息，宿主因此**没有任何依据**选发送目标；猜错就是往陌生群发消息。
/// 唯一诚实的做法是拒绝，并把"缺什么"说出来。
pub(crate) const NO_TARGET_CODE: &str = "HBOT_TIMER_NO_TARGET";

/// 被记账的一个计划：声明本身 + 它的下一次触发点。
#[derive(Clone, Debug)]
struct Tracked {
    spec: ScheduleSpec,
    next_at: Timestamp,
}

/// 计划表状态。
///
/// 单独一把锁：`due()` 每次 tick 都会动它，而装配只在 isolate 出生后写一次。
#[derive(Default)]
pub(crate) struct TimerState {
    schedules: Vec<Tracked>,
}

/// 定时器槽位（一个 isolate 一个）。
pub(crate) struct TimerSlot {
    state: Mutex<TimerState>,
}

impl TimerSlot {
    /// 空槽位：没有任何计划。
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(TimerState::default()),
        }
    }

    /// 装入计划表（`now` 是从下一次触发点起算的基准）。
    ///
    /// 整份替换是刻意的：清单是唯一真源，不存在"删掉一条但槽位还记得它"的状态。
    pub(crate) fn set_schedules(&self, schedules: &[ScheduleSpec], now: Timestamp) {
        let mut state = self.lock();
        state.schedules = schedules
            .iter()
            .map(|spec| Tracked {
                spec: spec.clone(),
                next_at: spec.when.next_after(now),
            })
            .collect();
    }

    /// 到点该触发的计划。
    ///
    /// **错过的触发不补跑**：推进方式是 `next_after(now)` 而不是 `next_after(next_at)`。
    /// 机器休眠三小时后醒来，"每小时巡检"只会触发一次，而不是三十次 —— 后者会让
    /// handler 队列瞬间满、把真正的新消息挤在后面，而用户看到的是"重启后发了一堆重复消息"。
    ///
    /// 返回 [`DueSchedule`] 而不是名字：`hbot-rpc` 派发完还要知道回复发往哪个会话，
    /// 而计划表只有这里有一份（让它再读一次清单就是把一个判据拆成两处）。
    pub(crate) fn due(&self, now: Timestamp) -> Vec<DueSchedule> {
        let mut state = self.lock();
        let mut fired = Vec::new();
        for tracked in &mut state.schedules {
            if tracked.next_at <= now {
                fired.push(DueSchedule {
                    name: tracked.spec.name.clone(),
                    conversation: tracked.spec.conversation.clone(),
                });
                tracked.next_at = tracked.spec.when.next_after(now);
            }
        }
        fired
    }

    /// 已装了几个计划（测试与 `hbot check` 用）。
    pub(crate) fn len(&self) -> usize {
        self.lock().schedules.len()
    }

    /// 最近的一个触发点（宿主 tick 用；空表返回 `None`）。
    pub(crate) fn next_at(&self) -> Option<Timestamp> {
        self.lock()
            .schedules
            .iter()
            .map(|tracked| tracked.next_at)
            .min()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TimerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for TimerSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimerSlot")
            .field("schedules", &self.len())
            .finish()
    }
}

/// 调度器的默认 tick 间隔。
///
/// 1 秒：`every` 的地板价就是 1 秒（`MIN_INTERVAL_MS`），tick 比它更快没有意义；
/// `daily` 的精度也就是 1 秒。这个值同时也是"最长偶尔迟到"的上限 —— 设计上定时器
/// 不承诺毫秒级准时（`docs/07` §4.1）。
pub const TICK: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::ScheduleWhen;
    use hbot_core::{MAX_INTERVAL_MS, MIN_INTERVAL_MS};

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    fn every(name: &str, every_ms: u64) -> ScheduleSpec {
        ScheduleSpec::new(name, ScheduleWhen::Every { every_ms })
    }

    /// 只看名字（多数用例不关心回复目标）。
    fn names(due: Vec<DueSchedule>) -> Vec<String> {
        due.into_iter().map(|item| item.name).collect()
    }

    #[test]
    fn an_empty_slot_never_fires() {
        let slot = TimerSlot::new();
        assert_eq!(slot.len(), 0);
        assert!(slot.next_at().is_none());
        assert!(slot.due(at(1_000_000)).is_empty());
    }

    #[test]
    fn set_schedules_replaces_the_whole_table() {
        let slot = TimerSlot::new();
        slot.set_schedules(&[every("a", 1_000), every("b", 5_000)], at(0));
        assert_eq!(slot.len(), 2);
        assert_eq!(slot.next_at(), Some(at(1_000)));

        slot.set_schedules(&[every("c", 10_000)], at(0));
        assert_eq!(slot.len(), 1, "旧的计划必须整份消失");
        assert_eq!(slot.next_at(), Some(at(10_000)));
    }

    #[test]
    fn due_returns_only_the_names_that_reached_their_grid_point() {
        let slot = TimerSlot::new();
        slot.set_schedules(
            &[every("fast", MIN_INTERVAL_MS), every("slow", 60_000)],
            at(0),
        );
        assert!(slot.due(at(999)).is_empty());
        assert_eq!(names(slot.due(at(1_000))), vec!["fast"]);
        assert_eq!(names(slot.due(at(2_000))), vec!["fast"]);
        // 刚触发过、还没到下一个网格点：不触发。注意不能用 `at(59_999)` 来测这一句
        // —— 从 2 000 直接跳到 59 999 是**错过窗口**，按语义 fast 会补一次。
        assert!(slot.due(at(2_999)).is_empty());
        assert_eq!(names(slot.due(at(60_000))), vec!["fast", "slow"]);
    }

    #[test]
    fn a_missed_window_fires_once_not_thirty_times() {
        let slot = TimerSlot::new();
        // 每小时一次；假装机器休眠了 3 小时。
        slot.set_schedules(&[every("hourly", 3_600_000)], at(0));
        assert_eq!(slot.next_at(), Some(at(3_600_000)));
        let fired = slot.due(at(3 * 3_600_000 + 17));
        assert_eq!(names(fired), vec!["hourly"], "只补一次，不补三十次");
        assert_eq!(
            slot.next_at(),
            Some(at(4 * 3_600_000)),
            "下一次应该是 4 小时的网格点，不是 3 小时 + 1 间隔"
        );
        assert!(slot.due(at(3 * 3_600_000 + 18)).is_empty());
    }

    #[test]
    fn a_daily_plan_rolls_to_tomorrow_after_firing() {
        let slot = TimerSlot::new();
        let daily = ScheduleSpec::new(
            "daily-reset",
            ScheduleWhen::Daily {
                at_ms_of_day: 32_400_000,
                utc_offset_minutes: 480,
            },
        );
        slot.set_schedules(&[daily], at(0));
        // 09:00 UTC+8 == 01:00 UTC == 3_600_000 ms。
        assert_eq!(slot.next_at(), Some(at(3_600_000)));
        assert!(slot.due(at(3_599_999)).is_empty());
        assert_eq!(names(slot.due(at(3_600_000))), vec!["daily-reset"]);
        assert_eq!(slot.next_at(), Some(at(90_000_000)));
    }

    #[test]
    fn next_at_is_the_earliest_across_the_table() {
        let slot = TimerSlot::new();
        slot.set_schedules(
            &[every("a", 30_000), every("b", 10_000), every("c", 60_000)],
            at(0),
        );
        assert_eq!(slot.next_at(), Some(at(10_000)));
        slot.due(at(10_000));
        assert_eq!(slot.next_at(), Some(at(20_000)));
    }

    #[test]
    fn a_due_schedule_carries_its_conversation_so_the_host_can_deliver() {
        let slot = TimerSlot::new();
        let push = ScheduleSpec::new("push", ScheduleWhen::Every { every_ms: 60_000 })
            .with_conversation("778899");
        let silent = every("silent", 60_000);
        slot.set_schedules(&[push, silent], at(0));
        let fired = slot.due(at(60_000));
        assert_eq!(fired.len(), 2);
        assert_eq!(fired[0].name, "push");
        assert_eq!(fired[0].conversation.as_deref(), Some("778899"));
        assert_eq!(fired[1].name, "silent");
        assert!(
            fired[1].conversation.is_none(),
            "没写目标和写了目标必须能区分"
        );
    }

    #[test]
    fn a_fresh_isolate_starts_empty_until_assembled() {
        // 这与 `Sandbox::is_timer_assembled()` 的语义对齐：出生为空，装配晚于首个
        // 事件是合法状态，`due()` 返回空表就什么都不触发。
        let slot = TimerSlot::new();
        assert_eq!(slot.len(), 0);
        assert!(slot.due(at(1_000_000)).is_empty());
    }

    #[test]
    fn the_tick_is_one_second_and_the_interval_floor_is_not_smaller() {
        assert_eq!(TICK, Duration::from_secs(1));
        // 轮询周期不能比计划表的最小间隔还大，否则小间隔计划会被系统性推迟一拍。
        const { assert!(MIN_INTERVAL_MS >= TICK.as_millis() as u64) };
        const { assert!(MAX_INTERVAL_MS > MIN_INTERVAL_MS) };
    }
}
