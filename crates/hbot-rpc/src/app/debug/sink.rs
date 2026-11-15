//! 把 isolate 的暂停接到事件总线与 UI 命令上。
//!
//! [`BotDebugSink`] 是 [`hbot_core::debug::DebugSink`] 在 `hbot-rpc` 侧的唯一实现：
//! `hbot-code` 的插桩命中断点后在**沙箱 worker 线程**上同步调用它，它就
//!
//! 1. 在登记表里占一个等待位（[`DebugRegistry::register_pause`]），
//! 2. 发 `debug.paused`（`dto/event.rs` 的冻结形状），
//! 3. **阻塞**等 `debug_resume` / `debug_step` 把 [`DebugAction`] 塞回来，最多
//!    [`DebugRegistry::pause_timeout`]（生产 5 分钟），
//! 4. 收尾：注销等待位 + 补发 `debug.resumed`，把动作交给插桩。
//!
//! ## 线程约束（改这里之前先确认调用路径）
//!
//! `paused()` 是**同步阻塞**的。它只能在
//! [`crate::app::runtime::sandbox_handler`] 的
//! `tokio::task::spawn_blocking(move || sandbox.dispatch_with_debug(&event, Some(&sink)))`
//! 里被调用——那是 blocking 线程池，线程挂了也只是池里少一条，不会卡住 reactor。
//! **绝不要**把 `Some(&sink)` 的 `dispatch_with_debug` 直接放在 `async fn` 里 await 之前
//! 的同步段里跑：那会把整个 tokio worker 线程钉死 5 分钟，同一 runtime 上的 HTTP/WS
//! 任务全部停摆（症状是"整个应用没反应"，而不是"这个 bot 不回话"）。
//!
//! ## 超时的取舍
//!
//! 超时返回 [`DebugAction::Continue`] 并补发 `debug.resumed`：宁可让断点"没停住"，
//! 也不能让 isolate 永久挂着。挂住的代价不只是这个 bot 不回话——暂停期间它的会话队列
//! 会一直积压（§6.5.1 说这是有意行为），5 分钟后队列满了就开始背压上层。
//!
//! 事件顺序（UI 依赖它画"已暂停/已继续"的灯）：
//! `debug.paused` → 用户在 UI 点继续 → 命令返回 → `debug.resumed`。
//! `debug.resumed` 只从这里发（命令侧不发）：超时与正常放行两条路都必须有它，
//! 放在唯一的收尾点上才不会漏。

use std::sync::Arc;
use std::time::Duration;

use hbot_core::BotId;
use hbot_core::debug::{DebugAction, DebugSink, PausedAt};

use crate::app::debug::DebugRegistry;
use crate::app::events::EventBus;
use crate::dto::{DebugScope, Event, StackFrame};

/// 把一个 bot 的调试暂停接到事件总线上的 sink。
///
/// 由 [`crate::app::runtime::sandbox_handler`] 在 blocking 线程里**每次派发前**构造
/// （很便宜：三个 `Arc` + 一个 [`BotId`]），不要跨消息缓存——`Arc` 的生命周期跟着
/// handler 走会拖住事件总线。
pub struct BotDebugSink {
    bot_id: BotId,
    registry: Arc<DebugRegistry>,
    events: Arc<EventBus>,
}

impl BotDebugSink {
    /// 为这个 bot 建一个 sink。
    pub fn new(bot_id: BotId, registry: Arc<DebugRegistry>, events: Arc<EventBus>) -> Self {
        Self {
            bot_id,
            registry,
            events,
        }
    }

    /// 发 `debug.resumed`（收尾点；超时与正常放行共用）。
    fn emit_resumed(&self) {
        self.events.send(Event::DebugResumed {
            bot_id: self.bot_id,
        });
    }
}

impl DebugSink for BotDebugSink {
    fn paused(&self, at: PausedAt) -> DebugAction {
        // 1) 先占位再发事件：UI 看到 debug.paused 立刻点"继续"时，等待者必须已经在表里，
        //    否则那条 debug_resume 会撞上 -32003（"没有暂停中的调试帧"）。
        let ticket = self
            .registry
            .register_pause(self.bot_id, at.file.clone(), at.line);

        // 2) 发 debug.paused（冻结形状：scopes / callStack 都要给全）。
        self.events.send(Event::DebugPaused {
            bot_id: self.bot_id,
            file: at.file.clone(),
            line: at.line,
            scopes: at.scopes.iter().map(scope_of).collect(),
            call_stack: at.call_stack.iter().map(frame_of).collect(),
        });

        // 3) 阻塞等命令。这里是 blocking 线程（见模块文档「线程约束」）。
        let action = match ticket.actions.recv_timeout(self.registry.pause_timeout()) {
            Ok(action) => action,
            Err(error) => {
                // 超时 = 没人理会这次暂停（UI 关了/用户走开了）；断线 = isolate 那边的
                // 登记表没了（进程收尾）。两种都只能继续跑，不能永远停着。
                //
                // 注意：这两种情况**不能**报错给上层——`DebugSink::paused` 的返回类型只有
                // 动作，没有错误通道；用户视角是"断点没停住"，日志里留一条 warn 够定位。
                tracing::warn!(
                    bot_id = %self.bot_id,
                    file = %at.file,
                    line = at.line,
                    error = %error,
                    "调试暂停未被放行，按继续处理（5 分钟上限）"
                );
                DebugAction::Continue
            }
        };

        // 4) 收尾：注销等待位（幂等；release 通常已经摘掉）+ 补发 debug.resumed。
        self.registry.cancel_pause(&self.bot_id, ticket.id);
        self.emit_resumed();

        action
    }
}

impl std::fmt::Debug for BotDebugSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotDebugSink")
            .field("bot_id", &self.bot_id)
            .finish_non_exhaustive()
    }
}

/// 默认的暂停上限（与 [`crate::app::debug::PAUSE_TIMEOUT`] 同一个值，测试要能直接读）。
pub const PAUSE_TIMEOUT: Duration = super::PAUSE_TIMEOUT;

/// `hbot-core` 的作用域 → wire 形状（字段一一对应，冻结在 `dto/common.rs`）。
fn scope_of(scope: &hbot_core::debug::DebugScope) -> DebugScope {
    DebugScope {
        name: scope.name.clone(),
        variables: scope.variables.clone(),
    }
}

/// `hbot-core` 的栈帧 → wire 形状（`call_stack` → `callStack`，见 `dto/event.rs`）。
fn frame_of(frame: &hbot_core::debug::DebugFrame) -> StackFrame {
    StackFrame {
        name: frame.name.clone(),
        file: frame.file.clone(),
        line: frame.line,
        column: frame.column,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::thread;
    use std::time::Instant;

    use hbot_core::debug::{DebugFrame, DebugScope as CoreScope, PausedAt};
    use serde_json::json;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn bot() -> BotId {
        BotId::from_uuid(uuid::Uuid::new_v4())
    }

    /// 一次"有人在断点上停下来"的现场：局部变量 + 两层调用栈。
    fn paused_at() -> PausedAt {
        let mut variables = BTreeMap::new();
        variables.insert("count".to_owned(), json!(3));
        variables.insert("label".to_owned(), json!("hi"));
        PausedAt {
            file: "src/main.ts".to_owned(),
            line: 42,
            column: 7,
            scopes: vec![CoreScope {
                name: "onMessage".to_owned(),
                variables,
            }],
            call_stack: vec![
                DebugFrame {
                    name: "onMessage".to_owned(),
                    file: "src/main.ts".to_owned(),
                    line: 42,
                    column: 7,
                },
                DebugFrame {
                    name: "dispatch".to_owned(),
                    file: "src/main.ts".to_owned(),
                    line: 11,
                    column: 1,
                },
            ],
        }
    }

    fn sink_of(registry: Arc<DebugRegistry>) -> (BotId, Arc<EventBus>, BotDebugSink) {
        let bot_id = bot();
        let events = Arc::new(EventBus::new());
        let sink = BotDebugSink::new(bot_id, registry, Arc::clone(&events));
        (bot_id, events, sink)
    }

    /// 在另一个线程里跑 `paused()`（它会阻塞），主线程做"UI 侧"的动作。
    fn spawn_pause(sink: BotDebugSink, at: PausedAt) -> thread::JoinHandle<DebugAction> {
        thread::spawn(move || sink.paused(at))
    }

    #[test]
    fn pause_round_trip_emits_events_and_returns_the_action() {
        let registry = Arc::new(DebugRegistry::with_pause_timeout(WAIT));
        let (bot_id, events, sink) = sink_of(Arc::clone(&registry));
        let mut wire = events.subscribe();

        let worker = spawn_pause(sink, paused_at());

        // 第一件事应当是 debug.paused（形状按 dto/event.rs 的冻结契约）。
        let paused = wire.blocking_recv().expect("必须发出 debug.paused");
        let Event::DebugPaused {
            bot_id: event_bot,
            file,
            line,
            scopes,
            call_stack,
        } = paused
        else {
            panic!("第一条事件应当是 debug.paused，实际是 {paused:?}");
        };
        assert_eq!(event_bot, bot_id);
        assert_eq!(file, "src/main.ts");
        assert_eq!(line, 42);
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].name, "onMessage");
        assert_eq!(scopes[0].variables.get("count"), Some(&json!(3)));
        assert_eq!(scopes[0].variables.get("label"), Some(&json!("hi")));
        assert_eq!(call_stack.len(), 2, "callStack 要原样转过去");
        assert_eq!(call_stack[0].name, "onMessage");
        assert_eq!(call_stack[0].column, 7);
        assert_eq!(call_stack[1].file, "src/main.ts");

        // 事件已经在路上 ⇒ 等待位必须已经就绪（否则 UI 的 resume 会撞 -32003）。
        assert_eq!(registry.paused(&bot_id), 1, "发事件前必须先登记");

        // "UI 点了单步前进"。
        assert_eq!(registry.release(&bot_id, DebugAction::StepInto), 1);
        assert_eq!(
            worker.join().expect("sink 线程不应 panic"),
            DebugAction::StepInto
        );

        // 收尾必须补一条 debug.resumed，且等待位已清空。
        let resumed = wire.blocking_recv().expect("必须发出 debug.resumed");
        assert!(
            matches!(resumed, Event::DebugResumed { bot_id: id } if id == bot_id),
            "收尾事件应当是 debug.resumed，实际是 {resumed:?}"
        );
        assert_eq!(registry.paused(&bot_id), 0);
    }

    #[test]
    fn a_step_over_command_comes_back_as_step_over() {
        let registry = Arc::new(DebugRegistry::with_pause_timeout(WAIT));
        let (bot_id, events, sink) = sink_of(Arc::clone(&registry));
        let mut wire = events.subscribe();

        let worker = spawn_pause(sink, paused_at());
        let _ = wire.blocking_recv().expect("debug.paused");
        registry.release(&bot_id, DebugAction::StepOver);

        assert_eq!(worker.join().expect("sink 线程"), DebugAction::StepOver);
    }

    #[test]
    fn a_timeout_continues_instead_of_hanging_the_isolate() {
        // 50ms 上限：模拟"用户走开了"，UI 永远不发 resume。
        let registry = Arc::new(DebugRegistry::with_pause_timeout(Duration::from_millis(50)));
        let (bot_id, events, sink) = sink_of(Arc::clone(&registry));
        let mut wire = events.subscribe();

        let started = Instant::now();
        let action = sink.paused(paused_at());
        let elapsed = started.elapsed();

        assert_eq!(action, DebugAction::Continue, "超时必须继续跑，不能停着");
        assert!(
            elapsed >= Duration::from_millis(50),
            "应当等满上限才返回，实际 {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "不该比上限慢太多，实际 {elapsed:?}"
        );

        // 两条事件都要在：debug.paused 之后必须补 debug.resumed，否则 UI 的灯一直亮着。
        let first = wire.blocking_recv().expect("debug.paused");
        assert!(matches!(first, Event::DebugPaused { .. }));
        let second = wire.blocking_recv().expect("debug.resumed");
        assert!(matches!(second, Event::DebugResumed { .. }));
        assert_eq!(registry.paused(&bot_id), 0, "超时也要注销等待位");
    }

    #[test]
    fn pausing_without_any_subscriber_still_finishes() {
        // 没人在听事件（应用刚起、UI 还没订阅）时，send 返回 0 而不是失败；
        // sink 必须照常登记、等超时、返回 Continue。
        let registry = Arc::new(DebugRegistry::with_pause_timeout(Duration::from_millis(20)));
        let (_bot_id, _events, sink) = sink_of(registry);
        assert_eq!(sink.paused(paused_at()), DebugAction::Continue);
    }

    #[test]
    fn two_isolates_paused_at_once_both_get_the_command() {
        // 一个 bot 最多两个 isolate（MAX_ISOLATES），可能同时停在断点上。
        let registry = Arc::new(DebugRegistry::with_pause_timeout(WAIT));
        let bot_id = bot();
        let events = Arc::new(EventBus::new());
        let first = BotDebugSink::new(bot_id, Arc::clone(&registry), Arc::clone(&events));
        let second = BotDebugSink::new(bot_id, Arc::clone(&registry), Arc::clone(&events));

        let one = spawn_pause(first, paused_at());
        let mut other_at = paused_at();
        other_at.file = "src/lib.ts".to_owned();
        let two = spawn_pause(second, other_at);

        // 两个都登记完再放行（轮询等待位数量，避免依赖调度顺序）。
        let deadline = Instant::now() + WAIT;
        while registry.paused(&bot_id) < 2 {
            assert!(Instant::now() < deadline, "两个 sink 都该登记为暂停");
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(registry.release(&bot_id, DebugAction::Continue), 2);
        assert_eq!(one.join().expect("第一条"), DebugAction::Continue);
        assert_eq!(two.join().expect("第二条"), DebugAction::Continue);
        assert_eq!(registry.paused(&bot_id), 0);
    }

    #[test]
    fn the_sink_is_send_and_sync() {
        // 契约要求 DebugSink: Send + Sync（它要在 blocking 线程间传递）。
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BotDebugSink>();
    }

    /// 事件确实走的是共享的那条 `broadcast` 总线（而不是 sink 自己另开一条）。
    ///
    /// 这条断言的是**装配**：`EventBus::subscribe()` 能看到 sink 发的事件，`ws.rs` 正是
    /// 这样把事件推给 UI 的。如果将来有人把 `Arc<EventBus>` 换成自定义通道，这条会红。
    #[test]
    fn events_go_through_the_shared_bus() {
        let registry = Arc::new(DebugRegistry::with_pause_timeout(Duration::from_millis(20)));
        let (bot_id, events, sink) = sink_of(registry);
        let mut wire = events.subscribe();
        assert_eq!(events.receiver_count(), 1, "总线上应当只有这一个订阅者");

        let action = sink.paused(paused_at());
        assert_eq!(action, DebugAction::Continue);

        assert!(
            matches!(
                wire.blocking_recv().expect("debug.paused"),
                Event::DebugPaused { bot_id: id, .. } if id == bot_id
            ),
            "sink 发的 debug.paused 必须能从 EventBus 上收到"
        );
        assert!(matches!(
            wire.blocking_recv().expect("debug.resumed"),
            Event::DebugResumed { .. }
        ));
    }
}
