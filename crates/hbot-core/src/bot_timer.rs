//! 定时器（`C1.timer`）的契约类型与纯计算。
//!
//! 设计见 `docs/07-bot-runtime-blueprint.md` §4.1（`onTimer` 钩子）与 §11 第 3 步。
//!
//! ## 这个文件只管三件事
//!
//! 1. **声明的形状**：项目在 `hbot.json` 的 `capabilities.timer` 里写一组具名计划；
//! 2. **下一个触发点是何时**（[`ScheduleWhen::next_after`]）：纯整数运算，不碰系统时钟、不碰 IO，
//!    所以可以用固定时间点写测试；
//! 3. **参数闸**（[`validate_name`] / [`validate_when`] / [`validate_schedules`]）：声明写错时
//!    在**加载清单**时就报错，而不是等到第一次触发才静默不动。
//!
//! ## 为什么是"对齐到时间网格"而不是"上次触发 + 间隔"
//!
//! `every` 的语义是**最小的 `t > now` 且 `t` 是 `every_ms` 的整数倍**（以 Unix 纪元为原点）。
//! 这样：进程重启、暂停、机器休眠都不会产生漂移累积；两个 bot 配同样的 `every` 会在同一秒触发
//! （便于观察与限流）。代价是"从启动时刻开始数"不可表达 —— 那本来也不是"每日重置"想要的东西。
//!
//! ## 为什么没有 `once`
//!
//! "延时一次"需要一份**跨重启的状态**（这次已经触发过了吗）。它属于 `bot_jobs` 那一层的
//! 问题（一次性作业），不属于"计划表"。把它塞进声明式计划会让"重启后是否再触发"变成一个
//! 没有正确答案的语义。见 `docs/07` §10 未决。
//!
//! ## 为什么不引入日期库
//!
//! 领域层的时间就是 Unix 毫秒（[`Timestamp`]）。"每天 09:00"这种说法需要时区，而时区
//! 只需要一个**固定 UTC 偏移**（分钟）就够了：不跨夏令时切换、不做 IANA 数据库查询。
//! 需要真时区的场景（"北京时间每天 9 点"在实行夏令时的国家）请让用户在清单里自己算偏移。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::Timestamp;

/// 一个计划名字的最大字节数（ASCII 为主，留足前缀空间）。
pub const MAX_SCHEDULE_NAME_BYTES: usize = 64;

/// 一个项目最多声明多少个计划。
///
/// 上限不是为了性能，是为了**可解释性**：16 个计划已经覆盖"每日重置 / 每小时巡检 /
/// 每分钟轮询"这三类真实需求，再往上用户应该用 `jobs` 链式调度而不是铺满计划表。
pub const MAX_SCHEDULES: usize = 16;

/// `every` 的最小间隔：1 秒。
///
/// 地板价 1 秒是因为 handler 预算按秒算、且每个计划触发都要经一次会话串行与日志落库；
/// 更密的触发应该走 `onMessage` 或作业池，而不是定时器。
pub const MIN_INTERVAL_MS: u64 = 1_000;

/// `every` 的最大间隔：一天（86 400 000 毫秒）。
///
/// 大于一天的计划用 `daily` 表达更清楚（"每天 09:00" vs "每 172 800 000 毫秒"），
/// 后者在闰秒/时区问题上是无法解释的。
pub const MAX_INTERVAL_MS: u64 = 86_400_000;

/// 一天的毫秒数；`daily` 的时间网格单位。
pub const DAY_MS: u64 = 86_400_000;

/// UTC 偏移的合法下界（分钟）：UTC-12:00。
pub const MIN_UTC_OFFSET_MINUTES: i32 = -720;

/// UTC 偏移的合法上界（分钟）：UTC+14:00。
pub const MAX_UTC_OFFSET_MINUTES: i32 = 840;

/// 一个具名计划何时触发。
///
/// `#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]`：
/// `hbot.json` 里写
/// `{ "name": "daily-reset", "kind": "daily", "atMsOfDay": 32400000, "utcOffsetMinutes": 480 }`
/// （`32400000` = 09:00 UTC+8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ScheduleWhen {
    /// 固定间隔：对齐到 `every_ms` 的时间网格。
    Every {
        /// 间隔毫秒数，[`MIN_INTERVAL_MS`] ..= [`MAX_INTERVAL_MS`]。
        every_ms: u64,
    },
    /// 每天固定时刻（本地墙钟 = UTC + `utc_offset_minutes`）。
    Daily {
        /// 一天内的毫秒偏移，`0` = 00:00:00.000，必须 `< DAY_MS`。
        at_ms_of_day: u64,
        /// UTC 偏移（分钟）。+480 = UTC+8。
        utc_offset_minutes: i32,
    },
}

impl ScheduleWhen {
    /// 严格晚于 `now` 的第一个触发点。
    ///
    /// 两个变体都不会返回 `now` 本身：触发后立刻重算不会原地打转。
    /// 越界（年代溢出）不会 panic —— 结果被夹到 `i64::MAX`，调用方看到的是一个
    /// "永远不会到"的时间点，而不是进程崩掉。
    #[must_use]
    pub fn next_after(&self, now: Timestamp) -> Timestamp {
        match *self {
            Self::Every { every_ms } => {
                let step = i64::try_from(every_ms.max(MIN_INTERVAL_MS)).unwrap_or(i64::MAX);
                // 向下取整到网格：Rust 的 `/` 对负数是向零取整，所以负时间戳要手动纠偏。
                let mut floor = now.unix_millis() / step;
                if now.unix_millis() < 0 && now.unix_millis() % step != 0 {
                    floor -= 1;
                }
                let next = floor.saturating_add(1).saturating_mul(step);
                Timestamp::from_unix_millis(next)
            }
            Self::Daily {
                at_ms_of_day,
                utc_offset_minutes,
            } => {
                let offset_ms = i64::from(utc_offset_minutes).saturating_mul(60_000);
                let day_ms = i64::try_from(DAY_MS).unwrap_or(i64::MAX);
                let at_ms = i64::try_from(at_ms_of_day)
                    .unwrap_or(i64::MAX)
                    .min(day_ms - 1);
                let local = now.unix_millis().saturating_add(offset_ms);
                let mut floor = local / day_ms;
                if local < 0 && local % day_ms != 0 {
                    floor -= 1;
                }
                let mut candidate_local = floor.saturating_mul(day_ms).saturating_add(at_ms);
                if candidate_local <= local {
                    candidate_local = candidate_local.saturating_add(day_ms);
                }
                Timestamp::from_unix_millis(candidate_local.saturating_sub(offset_ms))
            }
        }
    }

    /// 用来写进错误文案的人话描述。
    #[must_use]
    pub fn describe(&self) -> String {
        match *self {
            Self::Every { every_ms } => format!("每 {every_ms} 毫秒"),
            Self::Daily {
                at_ms_of_day,
                utc_offset_minutes,
            } => {
                let total_minutes = at_ms_of_day / 60_000;
                let hour = total_minutes / 60;
                let minute = total_minutes % 60;
                let sign = if utc_offset_minutes < 0 { '-' } else { '+' };
                let abs = utc_offset_minutes.unsigned_abs();
                format!(
                    "每天 {hour:02}:{minute:02}（UTC{sign}{:02}:{:02}）",
                    abs / 60,
                    abs % 60
                )
            }
        }
    }
}

/// 一个具名计划：`onTimer(ctx, name)` 里的 `name` 就是这里的 `name`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSpec {
    /// 计划名，非空、[`MAX_SCHEDULE_NAME_BYTES`] 以内、同一项目内唯一。
    pub name: String,
    /// 何时触发。
    pub when: ScheduleWhen,
    /// 这个计划的回复发往哪个**平台会话**（`conversations.external_id`）。
    ///
    /// 缺省 = `None`：定时器可以 `ctx.log`，但 `ctx.reply` 会明确报
    /// `HBOT_TIMER_NO_TARGET` —— 定时器背后没有入站消息，宿主没有任何依据猜该发给谁，
    /// 猜错就是往一个陌生群发消息。要"每日推送到某个群"就必须在这里写群号，
    /// 这也是 `docs/07` §11 第 3 步"主动推送"唯一的声明面。
    ///
    /// 会话类型（group / private）不在这里：它由 `conversations` 表那一行决定，
    /// 那张表的写方是适配器（第一条该会话的消息到达时）。表里还没有这一行时，
    /// 定时器不猜类型，而是记一行 `error` 日志说明"这个会话还没出现过"——
    /// 谁知道这个会话是群还是私聊，只有平台自己。
    #[serde(default)]
    pub conversation: Option<String>,
}

impl ScheduleSpec {
    /// 构造一个计划（**不做校验**，校验用 [`validate_schedules`]）。
    #[must_use]
    pub fn new(name: impl Into<String>, when: ScheduleWhen) -> Self {
        Self {
            name: name.into(),
            when,
            conversation: None,
        }
    }

    /// 指定这个计划的回复目标会话（平台外部 ID）。
    #[must_use]
    pub fn with_conversation(mut self, conversation: impl Into<String>) -> Self {
        self.conversation = Some(conversation.into());
        self
    }
}

/// 一个到点的计划：宿主派发 `onTimer(ctx, name)` 时需要知道的两件事。
///
/// 为什么不只返回名字：回复要发往 [`ScheduleSpec::conversation`]，而
/// [`TimerSlot`](https://docs.rs) 之外的人（`hbot-rpc` 的 tick）拿不到计划表；
/// 让它再读一次清单就等于把一个判据拆成两处读。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueSchedule {
    /// 计划名（`onTimer` 的第二个参数）。
    pub name: String,
    /// 回复目标；`None` = 这个计划只能记日志。
    pub conversation: Option<String>,
}

/// 定时器功能的错误。
///
/// 码表与其它能力一致：声明层 / 装配层 / 参数层 / 执行层四种，分开报是为了让排障方向不同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerError {
    /// 清单没声明 `capabilities.timer`。
    NotDeclared {
        /// 缺的是哪一项（固定 `"timer"`，留给未来 `webhook` 之类复用）。
        capability: &'static str,
    },
    /// 宿主没有装配调度器（例如无头模式没接时钟）。
    Unavailable {
        /// 具体原因（人话）。
        reason: String,
    },
    /// 声明本身不合法（名字、间隔、偏移、重复、超量）。
    Invalid {
        /// 具体哪里错了。
        message: String,
    },
    /// 执行期失败（落库、派发）。
    Failed {
        /// 具体原因。
        message: String,
    },
}

impl TimerError {
    /// 稳定的错误码前缀。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_TIMER_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_TIMER_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_TIMER_INVALID",
            Self::Failed { .. } => "HBOT_TIMER_FAILED",
        }
    }
}

impl fmt::Display for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())?;
        match self {
            Self::NotDeclared { capability } => {
                write!(f, ": 项目没有声明 `{capability}` 能力")
            }
            Self::Unavailable { reason } => write!(f, ": {reason}"),
            Self::Invalid { message } | Self::Failed { message } => write!(f, ": {message}"),
        }
    }
}

impl std::error::Error for TimerError {}

/// 校验一个计划名。
///
/// # Errors
///
/// 空、纯空白、超长、含控制字符都报 `Invalid`。
pub fn validate_name(name: &str) -> Result<(), TimerError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(TimerError::Invalid {
            message: "计划名不能为空".into(),
        });
    }
    if trimmed.len() > MAX_SCHEDULE_NAME_BYTES {
        return Err(TimerError::Invalid {
            message: format!(
                "计划名 `{}` 有 {} 字节，超过上限 {MAX_SCHEDULE_NAME_BYTES}",
                trimmed.chars().take(16).collect::<String>(),
                trimmed.len()
            ),
        });
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(TimerError::Invalid {
            message: format!("计划名 `{trimmed}` 含控制字符"),
        });
    }
    if trimmed != name {
        return Err(TimerError::Invalid {
            message: format!("计划名 `{name}` 首尾有空白"),
        });
    }
    Ok(())
}

/// 校验一个触发规则。
///
/// # Errors
///
/// 间隔越界、日偏移 `>= DAY_MS`、UTC 偏移越界都报 `Invalid`。
pub fn validate_when(when: &ScheduleWhen) -> Result<(), TimerError> {
    match *when {
        ScheduleWhen::Every { every_ms } => {
            if every_ms < MIN_INTERVAL_MS {
                return Err(TimerError::Invalid {
                    message: format!("间隔 {every_ms} 毫秒小于下限 {MIN_INTERVAL_MS}"),
                });
            }
            if every_ms > MAX_INTERVAL_MS {
                return Err(TimerError::Invalid {
                    message: format!("间隔 {every_ms} 毫秒大于上限 {MAX_INTERVAL_MS}"),
                });
            }
            Ok(())
        }
        ScheduleWhen::Daily {
            at_ms_of_day,
            utc_offset_minutes,
        } => {
            if at_ms_of_day >= DAY_MS {
                return Err(TimerError::Invalid {
                    message: format!("一天内的偏移 {at_ms_of_day} 必须小于 {DAY_MS}"),
                });
            }
            if !(MIN_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&utc_offset_minutes) {
                return Err(TimerError::Invalid {
                    message: format!(
                        "UTC 偏移 {utc_offset_minutes} 分钟超出 {MIN_UTC_OFFSET_MINUTES}..={MAX_UTC_OFFSET_MINUTES}"
                    ),
                });
            }
            Ok(())
        }
    }
}

/// 校验整张计划表：数量、每个名字、每个规则、名字唯一。
///
/// # Errors
///
/// 任一计划不合法、重名、超过 [`MAX_SCHEDULES`] 都报 `Invalid`。
pub fn validate_schedules(schedules: &[ScheduleSpec]) -> Result<(), TimerError> {
    if schedules.len() > MAX_SCHEDULES {
        return Err(TimerError::Invalid {
            message: format!(
                "声明了 {} 个计划，超过上限 {MAX_SCHEDULES}",
                schedules.len()
            ),
        });
    }
    let mut seen: Vec<&str> = Vec::with_capacity(schedules.len());
    for schedule in schedules {
        validate_name(&schedule.name)?;
        validate_when(&schedule.when)?;
        if let Some(conversation) = &schedule.conversation {
            if conversation.trim() != conversation || conversation.trim().is_empty() {
                return Err(TimerError::Invalid {
                    message: format!(
                        "计划 `{}` 的 `conversation` 不能为空串或带首尾空白：收到的是 `{conversation}`",
                        schedule.name
                    ),
                });
            }
        }
        if seen.iter().any(|name| *name == schedule.name) {
            return Err(TimerError::Invalid {
                message: format!("计划名 `{}` 重复", schedule.name),
            });
        }
        seen.push(&schedule.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_SCHEDULES, 16);
        assert_eq!(MIN_INTERVAL_MS, 1_000);
        assert_eq!(MAX_INTERVAL_MS, DAY_MS);
        assert_eq!(DAY_MS, 86_400_000);
        assert_eq!(MAX_SCHEDULE_NAME_BYTES, 64);
        assert_eq!(MIN_UTC_OFFSET_MINUTES, -720);
        assert_eq!(MAX_UTC_OFFSET_MINUTES, 840);
    }

    #[test]
    fn every_snaps_forward_to_the_next_grid_point() {
        let every = ScheduleWhen::Every { every_ms: 60_000 };
        assert_eq!(every.next_after(at(0)).unix_millis(), 60_000);
        assert_eq!(every.next_after(at(1)).unix_millis(), 60_000);
        assert_eq!(every.next_after(at(60_000)).unix_millis(), 120_000);
        assert_eq!(every.next_after(at(60_001)).unix_millis(), 120_000);
    }

    #[test]
    fn every_handles_negative_timestamps_without_drift() {
        let every = ScheduleWhen::Every { every_ms: 1_000 };
        assert_eq!(every.next_after(at(-1_000)).unix_millis(), 0);
        assert_eq!(every.next_after(at(-1)).unix_millis(), 0);
        assert_eq!(every.next_after(at(-1_001)).unix_millis(), -1_000);
    }

    #[test]
    fn daily_fires_once_then_rolls_to_the_next_local_day() {
        // 09:00 UTC+8 == 01:00 UTC == 3_600_000。
        let daily = ScheduleWhen::Daily {
            at_ms_of_day: 32_400_000,
            utc_offset_minutes: 480,
        };
        // 本地 09:00:00 整点：这次已经到点了，下一次是明天。
        assert_eq!(daily.next_after(at(3_600_000)).unix_millis(), 90_000_000);
        // 本地 08:59:59.999：还有 1 毫秒。
        assert_eq!(daily.next_after(at(3_599_999)).unix_millis(), 3_600_000);
        // 本地 09:00:00.001：明天同一时刻。
        assert_eq!(daily.next_after(at(3_600_001)).unix_millis(), 90_000_000);
    }

    #[test]
    fn daily_crossing_the_utc_midnight_keeps_the_local_wall_clock() {
        // UTC-5 的每天 23:00 == 次日 04:00 UTC。
        let daily = ScheduleWhen::Daily {
            at_ms_of_day: 82_800_000,
            utc_offset_minutes: -300,
        };
        let day = 86_400_000;
        // 本地 1969-12-31 22:00（UTC 1970-01-01 03:00）→ 下一个是本地 23:00（UTC 04:00）。
        assert_eq!(daily.next_after(at(10_800_000)).unix_millis(), 14_400_000);
        // 本地已经到点（UTC 04:00 正是本地 Dec 31 23:00）→ 下一个是次日同一时刻。
        assert_eq!(daily.next_after(at(14_400_000)).unix_millis(), 100_800_000);
        // 再往后一天。
        assert_eq!(
            daily.next_after(at(100_800_000)).unix_millis(),
            day + 100_800_000
        );
    }

    #[test]
    fn next_after_is_always_strictly_later_than_now() {
        for when in [
            ScheduleWhen::Every { every_ms: 1_000 },
            ScheduleWhen::Every { every_ms: 60_000 },
            ScheduleWhen::Daily {
                at_ms_of_day: 0,
                utc_offset_minutes: 0,
            },
            ScheduleWhen::Daily {
                at_ms_of_day: 32_400_000,
                utc_offset_minutes: 480,
            },
        ] {
            for now in [-86_400_000i64, -1, 0, 1, 3_599_999, 3_600_000, 86_400_000] {
                assert!(
                    when.next_after(at(now)).unix_millis() > now,
                    "{when:?} at {now} did not move forward"
                );
            }
        }
    }

    #[test]
    fn names_are_trimmed_refused_and_bounded() {
        assert!(validate_name("daily-reset").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(" leading").is_err());
        assert!(validate_name("two\nlines").is_err());
        let long = "a".repeat(MAX_SCHEDULE_NAME_BYTES + 1);
        assert!(validate_name(&long).is_err());
        assert!(validate_name(&"a".repeat(MAX_SCHEDULE_NAME_BYTES)).is_ok());
    }

    #[test]
    fn when_rules_are_bounded_at_both_ends() {
        assert!(
            validate_when(&ScheduleWhen::Every {
                every_ms: MIN_INTERVAL_MS
            })
            .is_ok()
        );
        assert!(
            validate_when(&ScheduleWhen::Every {
                every_ms: MIN_INTERVAL_MS - 1
            })
            .is_err()
        );
        assert!(
            validate_when(&ScheduleWhen::Every {
                every_ms: MAX_INTERVAL_MS + 1
            })
            .is_err()
        );
        assert!(
            validate_when(&ScheduleWhen::Daily {
                at_ms_of_day: DAY_MS - 1,
                utc_offset_minutes: 0
            })
            .is_ok()
        );
        assert!(
            validate_when(&ScheduleWhen::Daily {
                at_ms_of_day: DAY_MS,
                utc_offset_minutes: 0
            })
            .is_err()
        );
        assert!(
            validate_when(&ScheduleWhen::Daily {
                at_ms_of_day: 0,
                utc_offset_minutes: MAX_UTC_OFFSET_MINUTES + 1
            })
            .is_err()
        );
    }

    #[test]
    fn the_schedule_table_refuses_duplicates_and_overflow() {
        let ok = vec![
            ScheduleSpec::new("a", ScheduleWhen::Every { every_ms: 1_000 }),
            ScheduleSpec::new("b", ScheduleWhen::Every { every_ms: 60_000 }),
        ];
        assert!(validate_schedules(&ok).is_ok());

        let duplicate = vec![
            ScheduleSpec::new("a", ScheduleWhen::Every { every_ms: 1_000 }),
            ScheduleSpec::new("a", ScheduleWhen::Every { every_ms: 60_000 }),
        ];
        let error = validate_schedules(&duplicate).unwrap_err();
        assert!(error.to_string().contains("重复"), "{error}");

        let too_many: Vec<_> = (0..=MAX_SCHEDULES)
            .map(|index| {
                ScheduleSpec::new(
                    format!("s{index}"),
                    ScheduleWhen::Every { every_ms: 60_000 },
                )
            })
            .collect();
        assert!(validate_schedules(&too_many).is_err());
        assert!(validate_schedules(&Vec::new()).is_ok());
    }

    #[test]
    fn every_error_has_a_stable_code_prefix() {
        let errors = [
            TimerError::NotDeclared {
                capability: "timer",
            },
            TimerError::Unavailable { reason: "x".into() },
            TimerError::Invalid {
                message: "y".into(),
            },
            TimerError::Failed {
                message: "z".into(),
            },
        ];
        for error in errors {
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{}: ", error.code())),
                "{error}"
            );
            assert!(
                error.code().starts_with("HBOT_TIMER_"),
                "{:?}",
                error.code()
            );
        }
    }

    #[test]
    fn a_schedule_can_name_the_conversation_it_replies_to() {
        let spec = ScheduleSpec::new("tick", ScheduleWhen::Every { every_ms: 1_000 })
            .with_conversation("123456");
        assert_eq!(spec.conversation.as_deref(), Some("123456"));
        assert!(validate_schedules(&[spec]).is_ok());

        // 缺省是 None：定时器只能记日志，不能猜该发给谁。
        let bare = ScheduleSpec::new("tick", ScheduleWhen::Every { every_ms: 1_000 });
        assert!(bare.conversation.is_none());
    }

    #[test]
    fn a_blank_or_padded_conversation_is_refused() {
        for bad in ["", "   ", " 123456", "123456 "] {
            let spec = ScheduleSpec::new("tick", ScheduleWhen::Every { every_ms: 1_000 })
                .with_conversation(bad);
            let error = validate_schedules(&[spec]).expect_err("应拒绝");
            assert_eq!(error.code(), "HBOT_TIMER_INVALID", "收到 {bad:?}");
            assert!(error.to_string().contains("conversation"), "{error}");
        }
    }

    #[test]
    fn describe_reads_like_a_sentence_for_humans() {
        assert_eq!(
            ScheduleWhen::Every { every_ms: 60_000 }.describe(),
            "每 60000 毫秒"
        );
        assert_eq!(
            ScheduleWhen::Daily {
                at_ms_of_day: 32_400_000,
                utc_offset_minutes: 480
            }
            .describe(),
            "每天 09:00（UTC+08:00）"
        );
        assert_eq!(
            ScheduleWhen::Daily {
                at_ms_of_day: 0,
                utc_offset_minutes: -300
            }
            .describe(),
            "每天 00:00（UTC-05:00）"
        );
    }
}
