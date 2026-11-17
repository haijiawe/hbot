//! 调试命令：`debug_*`。
//!
//! 接线形状见 `docs/02-architecture.md` §6.5 / §6.5.1：断点属于 **bot**（不是 Agent 会话），
//! 命中后由 `hbot-code` 的插桩在沙箱 worker 线程上同步回调
//! [`hbot_core::debug::DebugSink::paused`]，本模块的三条命令则是那个回调的**放行者**。
//!
//! 真正的状态在 [`crate::app::debug`]（登记表：断点 + 等待者），sink 在
//! [`crate::app::debug::sink`]，装配缝在
//! [`crate::app::runtime::sandbox_handler`]。本文件只做裁决：先校验，再转成一个登记表调用。
//!
//! ## 三条命令的返回（data 形状）
//!
//! 三条**成功时 data 都是 `null`**（`Response::ok()`；`dto/common.rs` 的
//! `response_wire_shape_is_tagged` 钉住 `{"type":"ok","data":null}`）。原因：这三条都是
//! "动作"而不是"查询"——命令回来只代表指令已经交给 isolate，真正的结果通过
//! `debug.paused` / `debug.resumed` 事件回来（异步，见 §6.5.1）。给一个"已暂停"之类的
//! 假快照反而会诱使 UI 用它覆盖事件驱动的状态。
//!
//! **调试的单位是 bot，不是 Agent 会话**：断点跟着某个 bot 的沙箱走，因为同一份源码在
//! 不同 bot 的 isolate 里可以是不同版本（见 `dto/command.rs` 的 `DebugSetBreakpoints`
//! 注释）。

use super::{done, failed};
use crate::app::AppState;
use crate::app::debug::not_running;
use crate::dto::{Command, DebugBreakpoint, Response, StepKind};
use hbot_core::debug::DebugAction;
use hbot_core::{BotId, HbotError};

/// 一条断点条件的长度上限（**字符**，不是字节）：超过它就是 `-32002`。
///
/// 条件是一段同步 JS 表达式，一行以内就该写完。上限存在的理由不是"防止慢"（求值次数
/// 已经被命中点限制住了），而是防止把整段代码误粘进条件框：那既不是条件的用法，也会让
/// 错误信息变成一整段源码。按字符数而不是字节数，是因为中文串在条件里是合法且常见的
/// （`name === "你好"`），按字节算会让同样的表达式在中英文下限额差三倍。
pub const MAX_CONDITION_CHARS: usize = 200;

/// 一条日志点消息模板的长度上限（**字符**，不是字节）：超过它就是 `-32002`。
///
/// 模板是"给用户看的运行时输出"（`{count} items`），500 字符够写一句带几个变量的
/// 人话；再长就已经不是日志点而是往日志里塞文章了，面板一行放不下、环形缓冲也会被
/// 少数几行挤爆。与条件一样按字符数算：中文模板（`第 {n} 条消息`）是正常用法。
pub const MAX_LOG_CHARS: usize = 500;

/// 一次 `debug_set_breakpoints` 拆出来的两路（两张表各自整份替换）。
///
/// `lines` 进 [`hbot_core::debug::Breakpoints::set`]（会停下的断点，`None` = 无条件），
/// `logs` 进 `Breakpoints::set_logs`（只打印、不停下的日志点）。
///
/// 拆分依据是**每个条目自己的 `log` 字段**，不是"按行合并"：同一行可以出现两次
/// （一次 `log: null` 一次 `log: Some(...)`），那样它既是断点又是日志点——命中时先打印
/// 再停下，正是 debugger 里"断点 + 日志点"的用法。
#[derive(Debug, Default, PartialEq, Eq)]
struct PlannedBreakpoints {
    lines: Vec<(u32, Option<String>)>,
    logs: Vec<(u32, String)>,
}

/// `debug_set_breakpoints`：记住断点/日志点，并推给这个 bot 现有的 isolate。
///
/// * 成功：`{"type":"ok","data":null}`。注意"成功"**不代表**"已经停在断点上"——如果
///   isolate 还没出生（bot 刚启动、还没收到消息），断点先记在登记表里，等沙箱建好时
///   [`crate::app::debug::DebugRegistry::attach`] 会补上（§6.5.1"断点跟着某个 bot 的 isolate"）。
/// * 插桩未就绪（`dispatch_with_debug` 还是空壳）：同样返回成功。断点被记住，等
///   `hbot-code` 落地后自动生效——**不能**因为"暂时不会命中"就报错，否则 UI 的断点面板
///   在对方落地前永远显示失败。
/// * 条件超长（超过 [`MAX_CONDITION_CHARS`] 个字符）或日志点模板超长（超过
///   [`MAX_LOG_CHARS`] 个字符）：`-32002 invalid`。这两条在 `ensure_running` **之前**判
///   ——参数不合法不该因为 bot 恰好没在跑就变成 `-32005`。
/// * bot 不存在：`-32001 not_found`。
/// * bot 存在但没在跑：`-32005 denied`（见 [`not_running`] 里的取舍说明）。
///
/// ## 两张表分开替换（日志点会话）
///
/// `Breakpoints::set` 与 `set_logs` 都是**整份替换自己那张表**，所以这里不能"把两路拼成
/// 一次调用"：只给日志点的请求如果顺手调了 `set`，会把该文件已有的普通断点清掉。
/// 规则：
///
/// * `breakpoints` 整个为空 → 该文件两张表都清空（沿用"空列表 = 取消该文件的断点"）。
/// * 否则 `lines` 非空才调 `set`，`logs` 非空才调 `set_logs`；只出现日志点的请求
///   因此完全不动 `files` 表，反之亦然。
pub async fn set_breakpoints(
    state: &AppState,
    bot_id: BotId,
    file: String,
    breakpoints: Vec<DebugBreakpoint>,
) -> Response {
    let plan = match plan_breakpoints(&breakpoints) {
        Ok(plan) => plan,
        Err(error) => return failed(error),
    };
    if let Err(error) = ensure_running(state, bot_id).await {
        return failed(error);
    }
    // 空 `breakpoints` = 取消该文件的断点与日志点；行号 0 被忽略。
    // 条件/模板原样交给登记表，**这里不求值**：求值/展开在 isolate 的 JS 侧（`hbot-code`）。
    if breakpoints.is_empty() {
        state.debug().set_breakpoints(&bot_id, &file, []);
        state.debug().set_logpoints(&bot_id, &file, []);
        return done();
    }
    if !plan.lines.is_empty() {
        state.debug().set_breakpoints(&bot_id, &file, plan.lines);
    }
    if !plan.logs.is_empty() {
        state.debug().set_logpoints(&bot_id, &file, plan.logs);
    }
    done()
}

/// 把 wire 上的断点列表拆成 [`PlannedBreakpoints`]。
///
/// 规则（都在这里而不是在 `hbot-core`：那是纯数据层，越少"命令语义"越好测试）：
///
/// * 行号 `0` 忽略（1-based 语义，沿用现状）。
/// * 空白条件（`"  "`、`""`）视作无条件：`Some("")` 只会让 isolate 每次命中都编译失败
///   一次，然后按"不命中"处理，没有任何语义价值。
/// * 条件两端空白会被去掉：条件是一段表达式，首尾空白不改变含义；去掉后长度校验与
///   JS 侧缓存键更稳定。
/// * 同理，空白日志点模板（`""`、`"   "`）当作**没有模板**，该条目退回普通断点：空模板
///   展开出来是空串，用户什么也看不到，却要为它背每次命中的一次 JS 调用。
/// * 带非空 `log` 的条目进 `logs`，**不进** `lines`：`log: Some(t)` 的契约就是"不停下"。
///   如果同一行还想停下，wire 上再给一条 `log: null` 的条目即可（两路各记一行）。
fn plan_breakpoints(breakpoints: &[DebugBreakpoint]) -> Result<PlannedBreakpoints, HbotError> {
    let mut plan = PlannedBreakpoints {
        lines: Vec::with_capacity(breakpoints.len()),
        logs: Vec::with_capacity(breakpoints.len()),
    };
    for breakpoint in breakpoints {
        if breakpoint.line == 0 {
            continue;
        }
        if let Some(template) = breakpoint
            .log
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let chars = template.chars().count();
            if chars > MAX_LOG_CHARS {
                return Err(HbotError::invalid(format!(
                    "第 {} 行的日志点模板有 {chars} 个字符，超过上限 {MAX_LOG_CHARS} 个字符：\
                     日志点是给面板看的一行话（如 `{{count}} items`），不该把整段文本粘进来",
                    breakpoint.line
                )));
            }
            plan.logs.push((breakpoint.line, template.to_owned()));
            continue;
        }
        let Some(text) = breakpoint
            .condition
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            plan.lines.push((breakpoint.line, None));
            continue;
        };
        let chars = text.chars().count();
        if chars > MAX_CONDITION_CHARS {
            return Err(HbotError::invalid(format!(
                "第 {} 行的断点条件有 {chars} 个字符，超过上限 {MAX_CONDITION_CHARS} 个字符：\
                 条件是一段同步 JS 表达式（如 `count > 3`），不该把整段代码粘进来",
                breakpoint.line
            )));
        }
        plan.lines.push((breakpoint.line, Some(text.to_owned())));
    }
    Ok(plan)
}

/// `debug_resume`：让这个 bot 所有暂停中的 isolate 继续跑。
///
/// 成功 `{"type":"ok","data":null}`；当前没有暂停中的等待者 → `-32003 conflict`
/// （不是"bot 不存在"也不是"未实现"：命令本身可用，只是现在没有可放行的帧）。
///
/// 为什么不做 bot 存在性检查：这条命令唯一有意义的失败条件就是"没人在等"，
/// 而那正是 conflict 的语义。多一次 store 查询只会让"暂停 → 用户点了继续 → bot 恰好
/// 在这一瞬崩了"变成 -32001，反而把"取消一个已经不存在的暂停"说得像参数错误。
pub async fn resume(state: &AppState, bot_id: BotId) -> Response {
    release(state, bot_id, DebugAction::Continue)
}

/// `debug_step`：单步（`StepKind` 的三个变体一一映射到 [`DebugAction`]）。
///
/// 成功 `{"type":"ok","data":null}`；没有暂停中的等待者 → `-32003 conflict`。
pub async fn step(state: &AppState, bot_id: BotId, kind: StepKind) -> Response {
    release(state, bot_id, action_of(kind))
}

/// 路由入口。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::DebugSetBreakpoints {
            bot_id,
            file,
            breakpoints,
        } => set_breakpoints(state, bot_id, file, breakpoints).await,
        Command::DebugResume { bot_id } => resume(state, bot_id).await,
        Command::DebugStep { bot_id, kind } => step(state, bot_id, kind).await,
        other => super::unimplemented(other.kind()),
    }
}

/// `StepKind` → `DebugAction` 的唯一映射（`dto/common.rs` 的 wire 变体是 over/into/out）。
fn action_of(kind: StepKind) -> DebugAction {
    match kind {
        StepKind::Over => DebugAction::StepOver,
        StepKind::Into => DebugAction::StepInto,
        StepKind::Out => DebugAction::StepOut,
    }
}

/// 把动作交给登记表；没有等待者时给 `-32003`。
fn release(state: &AppState, bot_id: BotId, action: DebugAction) -> Response {
    let woken = state.debug().release(&bot_id, action);
    if woken == 0 {
        return failed(HbotError::conflict(format!(
            "bot `{bot_id}` 当前没有暂停中的调试帧"
        )));
    }
    done()
}

/// 校验"这个 bot 现在能不能挂断点"。
///
/// * 已在运行（有 supervisor）→ 通过，不再查库：`bot_start` 之后库里一定有这一行。
/// * 没在运行 → 查库区分"不存在"（`-32001`）与"存在但没跑"（`-32005`）。
///
/// 用 `state.blocking` 走 `spawn_blocking`：`get_bot` 是同步 SQLite 调用，直接在
/// `async fn` 里跑会占住 reactor 线程（`commands/bots.rs` 里同一个理由）。
async fn ensure_running(state: &AppState, bot_id: BotId) -> Result<(), HbotError> {
    if state.bots().is_running(&bot_id) {
        return Ok(());
    }
    let store = std::sync::Arc::clone(state.store());
    let existing = state
        .blocking(move || store.get_bot(&bot_id).map_err(crate::app::store_failed))
        .await?;
    match existing {
        // 库里也没有：命令指向了一个不存在的 bot。
        None => Err(HbotError::not_found("bot", bot_id.to_string())),
        // bot 存在但没在跑：断点只能挂在活着的 isolate 上（见 not_running 的注释）。
        Some(_) => Err(not_running(bot_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_kinds_map_to_the_three_debug_actions() {
        assert_eq!(action_of(StepKind::Over), DebugAction::StepOver);
        assert_eq!(action_of(StepKind::Into), DebugAction::StepInto);
        assert_eq!(action_of(StepKind::Out), DebugAction::StepOut);
    }

    fn breakpoint(line: u32, condition: Option<&str>) -> DebugBreakpoint {
        DebugBreakpoint {
            line,
            condition: condition.map(str::to_owned),
            log: None,
        }
    }

    fn logpoint(line: u32, template: &str) -> DebugBreakpoint {
        DebugBreakpoint {
            line,
            condition: None,
            log: Some(template.to_owned()),
        }
    }

    #[test]
    fn blank_conditions_become_unconditional_and_line_zero_is_ignored() {
        let plan = plan_breakpoints(&[
            breakpoint(0, Some("a > 1")),
            breakpoint(3, None),
            breakpoint(4, Some("   ")),
            breakpoint(5, Some("  count > 3  ")),
        ])
        .expect("合法输入不该报错");
        assert_eq!(
            plan.lines,
            vec![
                (3, None),
                (4, None),
                // 两端空白被去掉：条件是表达式，首尾空白不改变含义。
                (5, Some("count > 3".to_owned())),
            ],
            "行号 0 必须被忽略；空白条件必须退化成无条件"
        );
        assert!(plan.logs.is_empty(), "没有 log 字段就不该产生日志点");
    }

    #[test]
    fn logpoints_go_to_their_own_table_and_are_trimmed() {
        let plan = plan_breakpoints(&[
            logpoint(2, "  {count} items  "),
            // 空白模板 = 没有模板：退回普通断点，不能在命中时白跑一次 JS。
            logpoint(3, "   "),
            logpoint(0, "{ignored}"),
        ])
        .expect("合法输入不该报错");
        assert_eq!(plan.logs, vec![(2, "{count} items".to_owned())]);
        assert_eq!(plan.lines, vec![(3, None)], "空白模板必须退回普通断点");
    }

    #[test]
    fn the_same_line_can_be_a_breakpoint_and_a_logpoint() {
        // wire 上同一行给两条：一条 `log: null`（停下），一条 `log: Some`（打印）。
        let plan =
            plan_breakpoints(&[breakpoint(7, None), logpoint(7, "{n}")]).expect("合法输入不该报错");
        assert_eq!(plan.lines, vec![(7, None)]);
        assert_eq!(plan.logs, vec![(7, "{n}".to_owned())]);
    }

    #[test]
    fn a_log_template_at_the_limit_is_accepted_and_one_past_it_is_invalid() {
        let at_limit = "a".repeat(MAX_LOG_CHARS);
        let plan = plan_breakpoints(&[logpoint(1, &at_limit)]).expect("正好等于上限必须接受");
        assert_eq!(plan.logs[0].1.as_str(), at_limit.as_str());

        let error = plan_breakpoints(&[logpoint(9, &"a".repeat(MAX_LOG_CHARS + 1))])
            .expect_err("超长模板必须是参数错误");
        assert_eq!(error.code(), -32002, "-32002 才是 invalid");
        let message = error.to_string();
        assert!(
            message.contains("上限") && message.contains('9'),
            "文案要说清是哪一行、上限是多少：{message}"
        );
    }

    #[test]
    fn the_log_limit_counts_characters_not_bytes() {
        let cjk = "名".repeat(MAX_LOG_CHARS);
        assert!(cjk.len() > MAX_LOG_CHARS * 2, "前提：这是多字节串");
        let plan = plan_breakpoints(&[logpoint(1, &cjk)]).expect("按字符数算，这正好等于上限");
        assert_eq!(plan.logs.len(), 1);
    }

    #[test]
    fn a_condition_at_the_limit_is_accepted_and_one_past_it_is_invalid() {
        let at_limit = "a".repeat(MAX_CONDITION_CHARS);
        let plan = plan_breakpoints(&[breakpoint(1, Some(&at_limit))])
            .expect("正好等于上限必须接受（上限是闭区间）");
        assert_eq!(plan.lines[0].1.as_deref(), Some(at_limit.as_str()));

        let error = plan_breakpoints(&[breakpoint(7, Some(&"a".repeat(MAX_CONDITION_CHARS + 1)))])
            .expect_err("超长条件必须是参数错误");
        assert_eq!(error.code(), -32002, "-32002 才是 invalid");
        let message = error.to_string();
        assert!(
            message.contains("上限") && message.contains('7'),
            "文案要说清是哪一行、上限是多少：{message}"
        );
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        // 每个中文字符 3 字节：按字节算会被误判超长。
        let cjk = "名".repeat(MAX_CONDITION_CHARS);
        assert!(cjk.len() > MAX_CONDITION_CHARS * 2, "前提：这是多字节串");
        let plan =
            plan_breakpoints(&[breakpoint(1, Some(&cjk))]).expect("按字符数算，这正好等于上限");
        assert_eq!(plan.lines.len(), 1);
    }
}
