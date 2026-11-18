//! S2: `onTimer` 端到端 —— 真的 [`Sandbox`]、真的计划表、真的 `dispatch_timer`。
//!
//! 定时器的判据分散在两层：`crates/hbot-core/src/bot_timer.rs` 算"下一次什么时候"
//! （纯函数，有 10 条单测），`crates/hbot-code/src/timer.rs` 记"哪一条到点了"
//! （7 条单测）。这里钉的是第三件、也是最容易接错的一件：**槽位里的名字真的能调到
//! JS 里的 `onTimer`，并且它的输出/错误/预算语义与消息事件一致**。
//!
//! ① 没有 `onTimer` 的项目：报错必须点名缺的是哪个钩子，而不是静默成功；
//! ② 装表后到点触发一次，`ctx.log` / `ctx.reply` 都被收集；
//! ③ **错过的窗口只补一次**：休眠三小时后醒来，"每秒"只触发一次；
//! ④ 触发之后同一时刻不再重复触发（网格推进了）；
//! ⑤ `onTimer` 抛错时日志不丢，错误码可见；
//! ⑥ 定时器上下文没有入站字段（`text` 是 `undefined`），`ctx.name` 有值；
//! ⑦ 定时器的 handler 抛错不会被当成"没有钩子"。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use hbot_code::error::CodeError;
use hbot_code::{HttpConfig, Limits, Sandbox, compile_project};
use hbot_core::{DueSchedule, ScheduleSpec, ScheduleWhen, Timestamp};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// 临时项目目录，Drop 时删掉。
struct Project(PathBuf);

impl Project {
    fn new(name: &str, source: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hbot-s2-timer-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("建项目目录");
        std::fs::write(src.join("main.ts"), source).expect("写 src/main.ts");
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 编译 `source` 并起一个 isolate。
fn sandbox(name: &str, source: &str) -> (Project, Sandbox) {
    let project = Project::new(name, source);
    let compiled = compile_project(&project.0).expect("编译项目");
    let limits = Limits::default();
    let sandbox =
        Sandbox::spawn_with_http(compiled, limits, HttpConfig::deny_all()).expect("启动 isolate");
    (project, sandbox)
}

/// 一条"每秒"的计划。
fn every_second() -> ScheduleSpec {
    ScheduleSpec::new("tick", ScheduleWhen::Every { every_ms: 1_000 })
}

/// 一条"每秒"、并且声明了回复目标的计划。
fn every_second_to(conversation: &str) -> ScheduleSpec {
    every_second().with_conversation(conversation.to_owned())
}

/// 到点条目：名字 + 回复目标都写在计划里，宿主不该自己编。
fn due(name: &str, conversation: Option<&str>) -> DueSchedule {
    DueSchedule {
        name: name.to_owned(),
        conversation: conversation.map(ToOwned::to_owned),
    }
}

/// 从 `t0` 起算的时间点。
fn at(t0: Timestamp, millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(t0.unix_millis() + millis)
}

/// 抛出来的 JavaScript 异常 message。
fn thrown(error: &CodeError) -> &str {
    match error {
        CodeError::JsException { message, .. } => message,
        other => panic!("期望 JavaScript 异常，实际是 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ① 没有 onTimer
// ---------------------------------------------------------------------------

#[test]
fn a_project_without_on_timer_names_the_missing_hook() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
});
";
    let (_project, sandbox) = sandbox("no-hook", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    let error = sandbox
        .dispatch_timer(&due("tick", None))
        .expect_err("没有 onTimer 必须失败，否则每次到点都静默丢掉");
    let message = thrown(&error);
    assert!(
        message.contains("onTimer"),
        "错误信息必须点名缺的钩子，实际：{message}"
    );
    assert!(
        message.contains("tick"),
        "错误信息必须点名是哪条计划，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ② 到点触发一次，输出被收集
// ---------------------------------------------------------------------------

#[test]
fn a_due_schedule_calls_on_timer_and_collects_its_output() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('message');
  },
  onTimer(ctx, name) {
    ctx.log(`timer:${name}`);
    ctx.reply(`at:${name}`);
  },
});
";
    let (_project, sandbox) = sandbox("fires", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second_to("Cc")], t0);
    assert_eq!(sandbox.schedule_count(), 1, "装表后应有一条计划");

    // 早于下一次触发点：什么都不该发生。
    assert!(sandbox.due(at(t0, 999)).is_empty(), "999 毫秒时还没到点");
    // 到点：名字与回复目标一起回来（目标是计划自己声明的，宿主不编）。
    let due = sandbox.due(at(t0, 1_000));
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].name, "tick");
    assert_eq!(due[0].conversation.as_deref(), Some("Cc"));

    let outcome = sandbox.dispatch_timer(&due[0]).expect("到点了应能跑");
    assert_eq!(outcome.logs, vec!["timer:tick".to_owned()]);
    assert_eq!(outcome.replies, vec!["at:tick".to_owned()]);
}

// ---------------------------------------------------------------------------
// ②-b 没声明回复目标时 ctx.reply 必须拒绝
// ---------------------------------------------------------------------------

#[test]
fn a_timer_without_a_reply_target_refuses_instead_of_guessing() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.reply(`at:${name}`);
  },
});
";
    let (_project, sandbox) = sandbox("no-target", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    let error = sandbox
        .dispatch_timer(&due("tick", None))
        .expect_err("没写目标就该拒绝，猜一个群发出去没法收回");
    let message = thrown(&error);
    assert!(
        message.contains("HBOT_TIMER_NO_TARGET"),
        "错误必须带稳定码，实际：{message}"
    );
    assert!(
        message.contains("conversation"),
        "错误必须告诉用户去写哪个字段，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ③ 错过的窗口只补一次
// ---------------------------------------------------------------------------

#[test]
fn a_three_hour_nap_fires_the_grid_once_not_ten_thousand_times() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.log(`timer:${name}`);
  },
});
";
    let (_project, sandbox) = sandbox("nap", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    // 三小时 = 10800 个本该触发的网格点。只补一次：队列不是垃圾桶。
    let due = sandbox.due(at(t0, 3 * 60 * 60 * 1_000));
    assert_eq!(due.len(), 1, "错过的窗口只补一次");
    assert_eq!(due[0].name, "tick");
    assert!(
        sandbox.due(at(t0, 3 * 60 * 60 * 1_000)).is_empty(),
        "同一刻不重复触发"
    );
}

// ---------------------------------------------------------------------------
// ④ 触发后网格推进
// ---------------------------------------------------------------------------

#[test]
fn firing_advances_the_grid_so_the_same_moment_does_not_fire_twice() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.log(`timer:${name}`);
  },
});
";
    let (_project, sandbox) = sandbox("grid", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    let now = at(t0, 1_000);
    let due = sandbox.due(now);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].name, "tick");
    assert!(sandbox.due(now).is_empty(), "同一时刻第二次查询必须为空");
    assert!(sandbox.next_fire().is_some(), "推进后仍有下一次");
    assert!(
        sandbox.next_fire().unwrap_or(now) > now,
        "下一次必须严格更晚"
    );
}

// ---------------------------------------------------------------------------
// ⑤ handler 抛错时日志不丢
// ---------------------------------------------------------------------------

#[test]
fn a_throwing_timer_keeps_the_logs_it_already_printed() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.log(`before:${name}`);
    throw new Error('boom');
  },
});
";
    let (_project, sandbox) = sandbox("throws", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    let failure = sandbox
        .dispatch_timer_collecting(&due("tick", None))
        .expect_err("抛错必须上报");
    assert_eq!(
        failure.logs,
        vec!["before:tick".to_owned()],
        "抛错前的日志不许丢"
    );
    let message = thrown(&failure.error);
    assert!(
        message.contains("boom"),
        "错误信息应带抛出内容，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ⑥ 定时器上下文没有入站字段
// ---------------------------------------------------------------------------

#[test]
fn the_timer_context_has_no_inbound_fields_and_no_conversation_surfaces() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.log(String(name));
    ctx.log(String(ctx.name));
    ctx.log(String(ctx.text));
    ctx.log(String(ctx.conversationId));
    ctx.log(String(ctx.kv));
    ctx.log(String(ctx.history));
    ctx.log(String(ctx.device));
  },
});
";
    let (_project, sandbox) = sandbox("ctx-shape", source);
    let t0 = Timestamp::from_unix_millis(1_700_000_000_000);
    sandbox.set_schedules(&[every_second()], t0);

    let outcome = sandbox.dispatch_timer(&due("tick", None)).expect("跑通");
    assert_eq!(
        outcome.logs,
        vec![
            "tick".to_owned(),
            "tick".to_owned(),
            "undefined".to_owned(),
            "undefined".to_owned(),
            "undefined".to_owned(),
            "undefined".to_owned(),
            "undefined".to_owned(),
        ],
        "定时器上下文只给 reply/log/sleep/ai/name"
    );
}

// ---------------------------------------------------------------------------
// ⑦ 没装表就什么都不触发
// ---------------------------------------------------------------------------

#[test]
fn an_empty_schedule_table_never_fires() {
    let source = r"
import { defineBot } from 'hbot';

export default defineBot({
  onMessage(ctx) {
    ctx.reply('hi');
  },
  onTimer(ctx, name) {
    ctx.log(`timer:${name}`);
  },
});
";
    let (_project, sandbox) = sandbox("empty", source);
    assert_eq!(sandbox.schedule_count(), 0);
    assert!(sandbox.next_fire().is_none(), "没有计划就没有下一次");
    assert!(
        sandbox
            .due(Timestamp::from_unix_millis(i64::MAX))
            .is_empty(),
        "空表在任何时刻都不触发"
    );
}
