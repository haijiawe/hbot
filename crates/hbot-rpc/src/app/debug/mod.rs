//! 调试登记表：断点、暂停中的等待者，以及把 UI 命令接到 isolate 上的那一层。
//!
//! 契约在 `crates/hbot-core/src/debug.rs`（冻结，见 `docs/02-architecture.md` §6.5.1）：
//! `hbot-code` 实现 [`hbot_core::debug::DebugSink`] 的**调用方**，`hbot-rpc` 实现它。
//! 本模块只做三件事，业务规则一概不下沉：
//!
//! 1. **记住断点**：按 [`BotId`] 存一份 [`Breakpoints`]，并推送给这个 bot 现有的 isolate
//!    （[`DebugRegistry::set_breakpoints`]）。新 isolate 出生时反过来从登记表取（[`DebugRegistry::attach`]）。
//! 2. **登记"暂停中的等待者"**：isolate 线程停在 [`hbot_core::debug::DebugSink::paused`] 里
//!    时通过 [`DebugRegistry::register_pause`] 留一个通道，`debug_resume` / `debug_step`
//!    通过 [`DebugRegistry::release`] 把动作塞回去。
//! 3. **超时兜底**：等待上限由 [`PAUSE_TIMEOUT`] 给出（5 分钟，`docs/02-architecture.md`
//!    §6.5.1）。超时由 sink 自己收尾（返回 `Continue`），登记表不主动唤醒。
//!
//! ## 为什么是一张进程级单例
//!
//! 断点必须同时被**两侧**看见：命令侧（[`crate::app::commands::debug`]，
//! 拿得到 [`crate::app::AppState`]）与 isolate 侧（[`crate::app::runtime::sandbox_handler`]，
//! 每个 bot 一个 `SandboxHandler` 实例，构造参数 [`crate::app::runtime::SandboxHandlerConfig`]
//! 由 `commands::bots` 用完整结构体字面量给出）。那条装配缝属于别的单元，本单元不能改，
//! 所以登记表退化成 [`OnceLock`] 里的单例，两侧都通过 [`registry`] 取同一个 [`Arc`]。
//!
//! **这是有意的妥协**：真正的正确形态是 `SandboxHandlerConfig` 增加一个
//! `debug: Arc<DebugRegistry>` 字段、由 `AppState` 持有并注入（一个进程一份，测试一份）。
//! 拿到那条装配缝的改动权之后应当迁过去，并把 [`registry`] 删掉。
//!
//! ## 锁的顺序（改这个文件前先读）
//!
//! * 登记表的锁**只保护自己的 map**，临界区里不做 IO、不 await；
//! * **绝不在持有登记表锁时调用 [`BreakpointTarget::apply_breakpoints`]**：对方会去拿
//!   isolate 自己的锁，而 isolate 线程在暂停时会反过来拿登记表的锁
//!   （[`DebugRegistry::register_pause`]）——两边顺序不一致就是一个 ABBA 死锁。
//!   所以 [`DebugRegistry::set_breakpoints`] 是"锁内快照 → 锁外推送"。
//!   代价是两次并发设断点可能以相反顺序落地（最后落地的未必是最新一次）；这是
//!   命令级竞态，下一次 `debug_set_breakpoints` 就会纠正，不值得为此加版本号。

pub mod sink;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use hbot_core::debug::{Breakpoints, DebugAction};
use hbot_core::{BotId, HbotError};

pub use sink::BotDebugSink;

/// sink 等 `debug_resume` / `debug_step` 的默认上限：5 分钟（§6.5.1 的"去泡杯咖啡"）。
///
/// 超时后 sink 返回 [`DebugAction::Continue`] 并补一条 `debug.resumed`：**绝不让 isolate
/// 永久挂着**——那样这个 bot 的会话队列会一直积压，用户看到的症状是"机器人不回话了"，
/// 比"断点没停住"难查得多。
pub const PAUSE_TIMEOUT: Duration = Duration::from_secs(300);

/// 断点的接收方：`hbot-code` 的 isolate 句柄。
///
/// 抽这一层是为了**测试不需要真插桩**：登记表的用例用一个记录调用的假实现就够了
/// （真实现只是把参数转给 [`hbot_code::Sandbox::set_breakpoints`]）。
pub trait BreakpointTarget: Send + Sync {
    /// 用这份断点**整体替换**接收方当前的断点集合（不是增量）。
    fn apply_breakpoints(&self, breakpoints: Breakpoints);
}

impl BreakpointTarget for hbot_code::Sandbox {
    fn apply_breakpoints(&self, breakpoints: Breakpoints) {
        self.set_breakpoints(breakpoints);
    }
}

/// 一次暂停的登记凭据。
///
/// sink 拿到它之后：先发 `debug.paused`（此时凭据已经在登记表里，UI 看到事件就能
/// `debug_resume`），再 [`Receiver::recv_timeout`] 等动作。收尾必须调
/// [`DebugRegistry::cancel_pause`]（超时与断线两条路都要走）。
pub struct PauseTicket {
    /// 本次暂停的序号（同一 bot 可能有多条并发暂停）。
    pub id: u64,
    /// 动作通道：`debug_resume` → `Continue`，`debug_step` → `Step*`。
    pub actions: Receiver<DebugAction>,
}

/// 一个 bot 的调试状态。
#[derive(Default)]
struct BotState {
    /// 当前断点（跨 isolate 存活期保留：热重载/重启之后新 isolate 出生时自动套用）。
    breakpoints: Breakpoints,
    /// 活着的 isolate。用 [`Weak`]：isolate 的生命周期归 `sandbox_handler` 的 worker 槽位，
    /// 崩溃/热重载之后旧句柄自动失效，不需要通知登记表。
    targets: Vec<Weak<dyn BreakpointTarget>>,
    /// 暂停中的等待者（FIFO）。一个 bot 最多 2 个 isolate，理论上可以同时暂停两条。
    waiters: VecDeque<Waiter>,
}

/// 一个停在 `DebugSink::paused` 里的等待者。
struct Waiter {
    id: u64,
    /// 暂停位置（诊断/测试用；`debug_resume` 不按位置区分）。
    file: String,
    /// 暂停行号。
    line: u32,
    /// 动作通道发送端。
    actions: Sender<DebugAction>,
}

/// 调试登记表（见模块文档）。
pub struct DebugRegistry {
    bots: Mutex<HashMap<BotId, BotState>>,
    /// sink 等动作的上限（生产恒为 [`PAUSE_TIMEOUT`]，测试可缩短）。
    pause_timeout: Duration,
    /// 等待者序号（单调递增，只用于日志与"同一次暂停"的自校验）。
    next_waiter: AtomicU64,
}

impl DebugRegistry {
    /// 用默认暂停上限（[`PAUSE_TIMEOUT`]）建一张空表。
    pub fn new() -> Self {
        Self::with_pause_timeout(PAUSE_TIMEOUT)
    }

    /// 指定暂停上限（测试用；生产走 [`DebugRegistry::new`]）。
    pub fn with_pause_timeout(pause_timeout: Duration) -> Self {
        Self {
            bots: Mutex::new(HashMap::new()),
            pause_timeout,
            next_waiter: AtomicU64::new(1),
        }
    }

    /// sink 等 `debug_resume` / `debug_step` 的上限。
    pub fn pause_timeout(&self) -> Duration {
        self.pause_timeout
    }

    /// 设某个文件的断点，并推给这个 bot 现有的 isolate。
    ///
    /// `entries` 是 `(行号, 条件)`：条件原样存进 [`Breakpoints`]，**这里不求值**（求值在
    /// 沙箱 isolate 的 JS 侧）。空 `entries` = 移除该文件的断点
    /// （[`Breakpoints::set`] 的语义，行号 0 被忽略）。
    ///
    /// 长度上限（200 字符）与"空白条件当无条件"都在命令层
    /// （[`crate::app::commands::debug::set_breakpoints`]）做：本层是状态层，把它当纯
    /// 数据存，测试可以不经过命令层直接验证推送形状。
    ///
    /// 返回这个 bot 设完之后的**完整**断点集合（诊断/日志用）。
    pub fn set_breakpoints(
        &self,
        bot_id: &BotId,
        file: &str,
        entries: impl IntoIterator<Item = (u32, Option<String>)>,
    ) -> Breakpoints {
        let (snapshot, targets) = {
            let mut bots = self.lock();
            let state = bots.entry(*bot_id).or_default();
            state.breakpoints.set(file, entries);
            (state.breakpoints.clone(), live(state))
        };
        // 锁外推送：见模块文档「锁的顺序」。
        for target in targets {
            target.apply_breakpoints(snapshot.clone());
        }
        snapshot
    }

    /// 这个 bot 当前的断点（副本；`bot_start` 之前设置过就会在这里）。
    pub fn breakpoints_of(&self, bot_id: &BotId) -> Breakpoints {
        self.lock()
            .get(bot_id)
            .map(|state| state.breakpoints.clone())
            .unwrap_or_default()
    }

    /// 清空断点（不唤醒暂停，也不动等待者）。
    pub fn clear_breakpoints(&self, bot_id: &BotId) {
        let (empty, targets) = {
            let mut bots = self.lock();
            let state = bots.entry(*bot_id).or_default();
            state.breakpoints.clear();
            (Breakpoints::new(), live(state))
        };
        for target in targets {
            target.apply_breakpoints(empty.clone());
        }
    }

    /// 登记一个刚出生的 isolate：把当前断点套上去，并记住它（下次设断点时推给它）。
    ///
    /// 由 [`crate::app::runtime::sandbox_handler`] 在 **isolate 建好之后、接受第一条消息
    /// 之前**调用，否则新 isolate 上的断点会漏到第一批消息。
    pub fn attach(&self, bot_id: BotId, target: Arc<dyn BreakpointTarget>) {
        let snapshot = {
            let mut bots = self.lock();
            let state = bots.entry(bot_id).or_default();
            // 顺手清掉已经死掉的句柄（热重载/崩溃后的旧 isolate）。
            state.targets.retain(|weak| weak.strong_count() > 0);
            state.targets.push(Arc::downgrade(&target));
            state.breakpoints.clone()
        };
        // 锁外推送：见模块文档「锁的顺序」。
        target.apply_breakpoints(snapshot);
    }

    /// 登记一次暂停，返回动作通道。
    ///
    /// **必须在发 `debug.paused` 之前调用**：UI 看到事件立刻发 `debug_resume` 时，
    /// 等待者已经就位（否则那条 resume 会撞上 -32003）。
    pub fn register_pause(&self, bot_id: BotId, file: impl Into<String>, line: u32) -> PauseTicket {
        let id = self.next_waiter.fetch_add(1, Ordering::Relaxed);
        let (actions, waiting) = std::sync::mpsc::channel();
        self.lock()
            .entry(bot_id)
            .or_default()
            .waiters
            .push_back(Waiter {
                id,
                file: file.into(),
                line,
                actions,
            });
        PauseTicket {
            id,
            actions: waiting,
        }
    }

    /// 注销一次暂停（sink 醒来之后必须调用；重复调用无副作用）。
    pub fn cancel_pause(&self, bot_id: &BotId, waiter: u64) {
        if let Some(state) = self.lock().get_mut(bot_id) {
            state.waiters.retain(|pending| pending.id != waiter);
        }
    }

    /// 把动作发给这个 bot **所有**暂停中的等待者，返回送达条数。
    ///
    /// 为什么是"全部"而不是"最早的一条"：wire 上没有 pauseId（`dto/event.rs` 的
    /// `DebugPaused` 只有 `botId`），UI 无法区分同一个 bot 的两个 isolate。逐个放行会
    /// 变成"用户点了继续，画面还停着"——调试单位是 bot（§6.5.1），一次命令就该让这个
    /// bot 全部动起来。
    ///
    /// 返回 `0` 表示当前没有等待者：调用方据此回 `-32003`（conflict）。
    pub fn release(&self, bot_id: &BotId, action: DebugAction) -> usize {
        let mut bots = self.lock();
        let Some(state) = bots.get_mut(bot_id) else {
            return 0;
        };
        let count = state.waiters.len();
        for waiter in state.waiters.drain(..) {
            // 发送失败 = 那个 sink 已经不在了（isolate 被丢弃/超时先走一步）。
            // 这不是错误：`release()` 只对"还在等的人"负责。
            let _ = waiter.actions.send(action);
        }
        count
    }

    /// 这个 bot 现在有几个暂停中的等待者（UI 的"已暂停"灯与测试用）。
    pub fn paused(&self, bot_id: &BotId) -> usize {
        self.lock()
            .get(bot_id)
            .map(|state| state.waiters.len())
            .unwrap_or(0)
    }

    /// 这个 bot 是否正停着。
    pub fn is_paused(&self, bot_id: &BotId) -> bool {
        self.paused(bot_id) > 0
    }

    /// 暂停中的等待者位置（`文件:行`），诊断/日志用。
    pub fn paused_at(&self, bot_id: &BotId) -> Vec<(String, u32)> {
        self.lock()
            .get(bot_id)
            .map(|state| {
                state
                    .waiters
                    .iter()
                    .map(|waiter| (waiter.file.clone(), waiter.line))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 忘掉这个 bot 的全部调试状态（断点 + 句柄 + 等待者）。
    ///
    /// 只给测试清理用：**`bot_stop` 不清断点**——断点属于 bot 而不是某一次运行，
    /// 重启之后仍应生效（`docs/02-architecture.md` §6.5.1 的"断点跟着某个 bot 的 isolate"）。
    pub fn forget(&self, bot_id: &BotId) {
        self.lock().remove(bot_id);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<BotId, BotState>> {
        self.bots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for DebugRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DebugRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DebugRegistry")
            .field("bots", &self.lock().len())
            .field("pause_timeout", &self.pause_timeout)
            .finish_non_exhaustive()
    }
}

/// 进程级登记表（见模块文档「为什么是一张进程级单例」）。
///
/// 调用方一律写 [`crate::app::AppState::debug`]，不要直接碰这个函数——将来登记表变成
/// `AppState` 的字段时，只需要改那个访问器。
pub fn registry() -> Arc<DebugRegistry> {
    static REGISTRY: OnceLock<Arc<DebugRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(DebugRegistry::new())))
}

/// 还活着的接收方（锁内调用；只做 `Weak::upgrade`，不碰对方的锁）。
fn live(state: &mut BotState) -> Vec<Arc<dyn BreakpointTarget>> {
    state.targets.retain(|weak| weak.strong_count() > 0);
    state
        .targets
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>()
}

/// 拒绝"bot 现在不能挂断点"的统一错误（`-32005`）。
///
/// 放在这里而不是命令里，是为了让 `debug_set_breakpoints` 与将来的"从 IDE 里附加调试"
/// 说同一句话。
pub fn not_running(bot_id: BotId) -> HbotError {
    // 选 -32005（denied）而不是 -32006（internal）或 -32002（invalid）：
    // * 这不是 bug 也不是环境故障（那是 internal 的语义），`bot_start` 之后同一条命令就成功了；
    // * 参数本身没问题（file/lines 都合法），只是"现在不能调"，正好是 denied 的语义；
    // * 也不是 conflict（-32003 在本组命令里专门留给"没有暂停中的等待者"）。
    HbotError::denied(format!(
        "bot `{bot_id}` 未运行：断点只能挂在活着的 isolate 上，先 bot_start"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假的断点接收方：只记录每次收到的断点集合（不需要真插桩）。
    #[derive(Default)]
    struct FakeTarget {
        applied: Mutex<Vec<Breakpoints>>,
    }

    impl FakeTarget {
        fn applied(&self) -> Vec<Breakpoints> {
            self.applied.lock().expect("假接收方锁").clone()
        }
    }

    impl BreakpointTarget for FakeTarget {
        fn apply_breakpoints(&self, breakpoints: Breakpoints) {
            self.applied.lock().expect("假接收方锁").push(breakpoints);
        }
    }

    fn bot() -> BotId {
        BotId::from_uuid(uuid::Uuid::new_v4())
    }

    fn attach_target(registry: &DebugRegistry, bot_id: BotId) -> Arc<FakeTarget> {
        let target = Arc::new(FakeTarget::default());
        registry.attach(bot_id, Arc::clone(&target) as Arc<dyn BreakpointTarget>);
        target
    }

    /// 一组无条件断点条目（大多数测试只关心行号）。
    fn lines<const N: usize>(rows: [u32; N]) -> [(u32, Option<String>); N] {
        rows.map(|line| (line, None))
    }

    /// 空条目列表（= 移除该文件的断点）。
    fn no_lines() -> Vec<(u32, Option<String>)> {
        Vec::new()
    }

    #[test]
    fn breakpoints_are_remembered_per_bot() {
        let registry = DebugRegistry::new();
        let (a, b) = (bot(), bot());

        let after = registry.set_breakpoints(&a, "src/main.ts", lines([3, 7, 0]));
        assert!(after.hits("src/main.ts", 3), "行号应被记住（0 被忽略）");
        assert!(after.hits("src/main.ts", 7));
        assert!(!after.hits("src/main.ts", 0), "行号 0 必须被忽略");
        assert!(
            registry.breakpoints_of(&b).is_empty(),
            "断点必须按 bot 分开存"
        );

        // 第二次设同一个文件 = 整体替换那个文件的行集合，不是追加。
        let after = registry.set_breakpoints(&a, "src/main.ts", lines([9]));
        assert!(after.hits("src/main.ts", 9));
        assert!(!after.hits("src/main.ts", 3), "同文件应当整体替换");
    }

    #[test]
    fn empty_lines_remove_the_file() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        registry.set_breakpoints(&bot_id, "src/main.ts", lines([3]));
        let after = registry.set_breakpoints(&bot_id, "src/main.ts", no_lines());
        assert!(
            after.lines_of("src/main.ts").is_none(),
            "空行集合 = 移除该文件的断点"
        );
        assert!(after.is_empty());
    }

    #[test]
    fn setting_breakpoints_pushes_to_live_isolates_only() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        let mine = attach_target(&registry, bot_id);
        let other = attach_target(&registry, bot());

        registry.set_breakpoints(&bot_id, "src/main.ts", lines([4]));

        let applied = mine.applied();
        assert_eq!(applied.len(), 2, "出生一次 + 设断点一次");
        assert!(applied[0].is_empty(), "attach 时还没有断点");
        assert!(applied[1].hits("src/main.ts", 4), "设断点要推给 isolate");
        assert!(
            other.applied().iter().all(Breakpoints::is_empty),
            "别的 bot 的 isolate 不该收到"
        );
    }

    #[test]
    fn a_new_isolate_starts_with_the_remembered_breakpoints() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        registry.set_breakpoints(&bot_id, "src/main.ts", lines([12]));

        // 这个 bot 之前没有 isolate（断点先到、沙箱后建）：出生时必须补上。
        let target = attach_target(&registry, bot_id);
        let applied = target.applied();
        assert_eq!(applied.len(), 1);
        assert!(applied[0].hits("src/main.ts", 12));
    }

    #[test]
    fn dead_isolates_are_pruned_instead_of_leaking() {
        let registry = DebugRegistry::new();
        let bot_id = bot();

        {
            let target = attach_target(&registry, bot_id);
            registry.set_breakpoints(&bot_id, "src/main.ts", lines([1]));
            drop(target);
        }

        // 旧句柄已失效：下一次设断点不该再尝试推给它（也不该因此报错）。
        registry.set_breakpoints(&bot_id, "src/main.ts", lines([2]));
        let target = attach_target(&registry, bot_id);
        assert_eq!(
            target.applied().len(),
            1,
            "新句柄只该收到 attach 那一次（旧的已被清掉）"
        );
        assert!(target.applied()[0].hits("src/main.ts", 2));
    }

    #[test]
    fn release_without_a_waiter_reports_zero() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        assert_eq!(registry.release(&bot_id, DebugAction::Continue), 0);
        assert!(!registry.is_paused(&bot_id));
    }

    #[test]
    fn release_hands_the_action_back_to_the_paused_isolate() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        let ticket = registry.register_pause(bot_id, "src/main.ts", 7);

        assert!(registry.is_paused(&bot_id));
        assert_eq!(registry.paused(&bot_id), 1);
        assert_eq!(
            registry.paused_at(&bot_id),
            vec![("src/main.ts".to_owned(), 7)]
        );

        assert_eq!(registry.release(&bot_id, DebugAction::StepInto), 1);
        assert_eq!(
            ticket.actions.recv().expect("动作必须送达"),
            DebugAction::StepInto
        );
        // release 之后等待者已经从表里摘掉（sink 的 cancel_pause 只是幂等兜底）。
        assert!(!registry.is_paused(&bot_id));
        assert_eq!(
            registry.release(&bot_id, DebugAction::Continue),
            0,
            "同一次暂停不该被放行两次"
        );
    }

    #[test]
    fn release_wakes_every_waiter_of_the_bot() {
        // 同一 bot 的两个 isolate 同时暂停：一条 resume 让两个都动（wire 上没有 pauseId）。
        let registry = DebugRegistry::new();
        let (bot_id, other) = (bot(), bot());
        let first = registry.register_pause(bot_id, "a.ts", 1);
        let second = registry.register_pause(bot_id, "b.ts", 2);
        let untouched = registry.register_pause(other, "c.ts", 3);

        assert_eq!(registry.release(&bot_id, DebugAction::Continue), 2);
        assert_eq!(first.actions.recv().expect("第一条"), DebugAction::Continue);
        assert_eq!(
            second.actions.recv().expect("第二条"),
            DebugAction::Continue
        );
        assert!(untouched.actions.try_recv().is_err(), "别的 bot 不该被放行");
        assert!(!registry.is_paused(&bot_id));
        assert!(registry.is_paused(&other));
    }

    #[test]
    fn cancel_pause_is_idempotent_and_keeps_other_waiters() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        let first = registry.register_pause(bot_id, "a.ts", 1);
        let second = registry.register_pause(bot_id, "b.ts", 2);

        registry.cancel_pause(&bot_id, first.id);
        registry.cancel_pause(&bot_id, first.id);
        assert_eq!(registry.paused(&bot_id), 1, "只摘掉自己那一条");
        assert!(first.actions.try_recv().is_err());

        assert_eq!(registry.release(&bot_id, DebugAction::StepOver), 1);
        assert_eq!(
            second.actions.recv().expect("第二条"),
            DebugAction::StepOver
        );
    }

    #[test]
    fn forget_clears_breakpoints_and_waiters() {
        let registry = DebugRegistry::new();
        let bot_id = bot();
        registry.set_breakpoints(&bot_id, "src/main.ts", lines([3]));
        let ticket = registry.register_pause(bot_id, "src/main.ts", 3);

        registry.forget(&bot_id);
        assert!(registry.breakpoints_of(&bot_id).is_empty());
        assert!(!registry.is_paused(&bot_id));
        assert!(
            ticket.actions.try_recv().is_err(),
            "forget 不是 release：不该给等待者发动作"
        );
    }

    #[test]
    fn not_running_is_a_denied_error() {
        let error = not_running(bot());
        assert_eq!(error.code(), -32005, "-32005 才是 denied");
    }

    #[test]
    fn the_process_registry_is_one_shared_instance() {
        assert!(Arc::ptr_eq(&registry(), &registry()));
        assert_eq!(
            registry().pause_timeout(),
            PAUSE_TIMEOUT,
            "生产登记表必须用 5 分钟的上限"
        );
    }
}
