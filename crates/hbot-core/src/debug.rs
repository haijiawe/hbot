//! 调试端口：`hbot-code`（实现）与 `hbot-rpc`（适配）之间的**唯一**契约。
//!
//! 为什么放在 `hbot-core`：`hbot-code` 不许依赖 `hbot-rpc`（依赖方向），而 `hbot-rpc`
//! 要能把"命中断点"变成 UI 事件。两边都依赖 `hbot-core`，所以端口必须在这里。
//!
//! 设计要点（改之前先读）：
//!
//! 1. **调试的单位是 bot，不是 Agent 会话**：断点跟着某个 bot 的 isolate 走。
//! 2. **暂停发生在 isolate 自己的 OS 线程上**，所以 [`DebugSink::paused`] 是**同步阻塞**的。
//!    实现方（`hbot-rpc`）在里面发事件给 UI 然后等恢复指令；它自己负责超时，不能让
//!    isolate 永远卡住。
//! 3. **不暂停时零开销**：release 构建没有插桩；debug 构建里没有断点时，`__hbot_trace`
//!    只做一次查找。
//! 4. 这一层**不出现 `serde_json::Value` 之外的类型**：变量值是不可信的 JS 值，用
//!    JSON 传，不可序列化的值由实现方替换成字符串 `<unserializable>`。
//! 5. **条件在这里只是数据**：断点可以带一段同步 JS 表达式（[`Breakpoints::condition_of`]），
//!    但本层**不负责求值**。求值必须发生在 isolate 的 JS 侧（`hbot-code`），那里才有
//!    作用域与变量；在宿主里 eval 既拿不到变量，也会把调试器变成"在宿主进程里跑用户代码"。

use std::collections::BTreeMap;

/// 断点集合：项目内相对路径（正斜杠）→ 1-based 行号 → 条件。
///
/// 空集合 = 不暂停。行号 `0` 无意义，实现方应忽略（按 1-based 语义）。
///
/// 条件是 `Option<String>`：`Some` 是一段**同步 JS 表达式**（`null`/`undefined` 之外
/// 的真值才命中），`None` 是无条件断点。条件只被原样存下来、原样取出，求值在
/// `hbot-code` 的 isolate 里（见模块文档第 5 点）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Breakpoints {
    files: BTreeMap<String, BTreeMap<u32, Option<String>>>,
}

impl Breakpoints {
    /// 空集合（不暂停）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 整体替换某个文件的断点。`entries` 为空表示"这个文件不再有断点"。
    ///
    /// `entries` 是 `(行号, 条件)` 对：`None` = 无条件断点。空白条件（`"  "`）按
    /// `None` 处理——`Some("")` 只会让 isolate 每次命中都编译失败一次，没有任何语义。
    /// 行号 `0` 被忽略。同一行重复出现时后者覆盖前者。
    ///
    /// 路径按调用方给的字符串原样保存；比较时不规范化（实现方负责传项目内相对路径，
    /// 且与插桩时用的 `fileId` 一致）。
    pub fn set(
        &mut self,
        file: impl Into<String>,
        entries: impl IntoIterator<Item = (u32, Option<String>)>,
    ) {
        let mut lines = BTreeMap::new();
        for (line, condition) in entries {
            if line > 0 {
                lines.insert(line, condition.filter(|text| !text.trim().is_empty()));
            }
        }
        let file = file.into();
        if lines.is_empty() {
            self.files.remove(&file);
        } else {
            self.files.insert(file, lines);
        }
    }

    /// 清空全部断点。
    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// 没有任何断点。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 某个文件的断点行号（升序）。没有这个文件时为 `None`。
    pub fn lines_of(&self, file: &str) -> Option<impl Iterator<Item = u32> + '_> {
        self.files.get(file).map(|lines| lines.keys().copied())
    }

    /// 全部断点（按文件名排序）。
    ///
    /// 值是"行号 → 条件"：`None` 是无条件断点。UI 与登记表只需要遍历行号时用
    /// [`Breakpoints::lines_of`]；这里给出条件是为了让登记表能整份快照转发而不丢条件。
    pub fn files(&self) -> impl Iterator<Item = (&str, &BTreeMap<u32, Option<String>>)> {
        self.files.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// 这一行是否命中。`file` 必须与 [`Breakpoints::set`] 时一致。
    ///
    /// **不看条件**：条件求值在 isolate 的 JS 侧，命中之后才做（这样"有没有断点"这一
    /// 问题仍然是一次 map 查找）。
    pub fn hits(&self, file: &str, line: u32) -> bool {
        self.files
            .get(file)
            .is_some_and(|lines| lines.contains_key(&line))
    }

    /// 这一行的条件：`Some(表达式)` = 有条件；`None` = 没有条件。
    ///
    /// `None` 同时覆盖"这一行没有断点"与"无条件断点"两种情况——调用方先用
    /// [`Breakpoints::hits`] 判断有没有断点，再决定要不要看条件。
    pub fn condition_of(&self, file: &str, line: u32) -> Option<&str> {
        self.files
            .get(file)
            .and_then(|lines| lines.get(&line))
            .and_then(|condition| condition.as_deref())
    }
}

/// 一个作用域里的局部变量。
///
/// 变量值是 JS 值序列化成 JSON 的结果；无法序列化（函数、循环引用、Proxy 等）时由
/// 实现方写成 `serde_json::Value::String("<unserializable>")`，**不要静默丢掉**——
/// UI 需要知道"这个变量存在但看不到"。
#[derive(Clone, Debug, PartialEq)]
pub struct DebugScope {
    /// 作用域名，例如 `"局部"`、`"闭包"`、`"全局"`。
    pub name: String,
    /// 变量表（按名字排序，避免 UI 每次渲染顺序抖动）。
    pub variables: BTreeMap<String, serde_json::Value>,
}

/// 逻辑调用栈的一帧（外层在前）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugFrame {
    /// 函数名；匿名函数用 `"<anonymous>"`。
    pub name: String,
    /// 项目内相对路径。
    pub file: String,
    /// 1-based。
    pub line: u32,
    /// 1-based。
    pub column: u32,
}

/// 暂停现场。
#[derive(Clone, Debug, PartialEq)]
pub struct PausedAt {
    /// 命中断点的项目内相对路径。
    pub file: String,
    /// 1-based。
    pub line: u32,
    /// 1-based。
    pub column: u32,
    /// 这次暂停"命中的条件"：`Some(表达式)` = 是一个条件断点自己算出为真才停的；
    /// `None` = 无条件断点，或单步停在某行。
    ///
    /// 条件求值在 isolate 的 JS 侧（`hbot-code`），这里只把**原文**带出来给 UI 显示
    /// （"我为什么停在这"）；`hbot-core` 不求值任何表达式。
    pub condition: Option<String>,
    /// 作用域（外层在前；最后一个通常是"局部"）。
    pub scopes: Vec<DebugScope>,
    /// 逻辑调用栈（外层在前，最后一帧是当前函数）。
    pub call_stack: Vec<DebugFrame>,
}

/// 恢复指令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugAction {
    /// 继续跑到下一个断点。
    Continue,
    /// 步过（同一帧的下一行）。
    StepOver,
    /// 步入（进入被调用函数）。
    StepInto,
    /// 步出（跑完当前帧）。
    StepOut,
}

/// 暂停回调。**同步阻塞**：实现方在里面发事件、等用户操作，然后返回指令。
///
/// 实现方必须在超时后返回 [`DebugAction::Continue`]（建议 5 分钟并在 UI 上提示），
/// 否则一次忘记点的断点会让这个 bot 永久卡住。
pub trait DebugSink: Send + Sync {
    /// isolate 已暂停，等一个恢复指令。
    fn paused(&self, at: PausedAt) -> DebugAction;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无条件断点条目。
    fn line(number: u32) -> (u32, Option<String>) {
        (number, None)
    }

    /// 带条件的断点条目。
    fn cond(number: u32, text: &str) -> (u32, Option<String>) {
        (number, Some(text.to_owned()))
    }

    #[test]
    fn empty_breakpoints_never_hit() {
        let bp = Breakpoints::new();
        assert!(bp.is_empty());
        assert!(!bp.hits("src/main.ts", 1));
        assert_eq!(bp.condition_of("src/main.ts", 1), None);
    }

    #[test]
    fn setting_an_empty_line_list_removes_the_file() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [line(3), line(7)]);
        assert!(bp.hits("src/main.ts", 7));
        bp.set("src/main.ts", Vec::<(u32, Option<String>)>::new());
        assert!(!bp.hits("src/main.ts", 7));
        assert!(bp.is_empty(), "空行表必须真的移除条目，否则会留下幽灵文件");
    }

    #[test]
    fn line_zero_is_ignored_not_stored() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [line(0), line(5)]);
        assert!(!bp.hits("src/main.ts", 0));
        assert!(bp.hits("src/main.ts", 5));
    }

    #[test]
    fn hits_is_per_file_not_global() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [line(5)]);
        assert!(!bp.hits("src/other.ts", 5));
    }

    #[test]
    fn files_is_sorted_for_stable_rendering() {
        let mut bp = Breakpoints::new();
        bp.set("src/z.ts", [line(1)]);
        bp.set("src/a.ts", [line(1)]);
        let names: Vec<&str> = bp.files().map(|(f, _)| f).collect();
        assert_eq!(names, vec!["src/a.ts", "src/z.ts"]);
    }

    #[test]
    fn a_condition_is_stored_verbatim_and_returned() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [cond(4, "a > 1 && b === 'x'")]);
        assert!(bp.hits("src/main.ts", 4), "有条件也是断点");
        assert_eq!(
            bp.condition_of("src/main.ts", 4),
            Some("a > 1 && b === 'x'")
        );
        // 其它行没有条件。
        assert_eq!(bp.condition_of("src/main.ts", 5), None);
        assert_eq!(bp.condition_of("src/other.ts", 4), None);
    }

    #[test]
    fn an_unconditional_breakpoint_reports_no_condition() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [line(6)]);
        assert!(bp.hits("src/main.ts", 6));
        assert_eq!(
            bp.condition_of("src/main.ts", 6),
            None,
            "无条件断点必须与\"没断点\"一样返回 None，调用方靠 hits 区分"
        );
    }

    #[test]
    fn replacing_a_line_replaces_its_condition() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [cond(8, "a > 1")]);
        assert_eq!(bp.condition_of("src/main.ts", 8), Some("a > 1"));
        // 同一行改条件：旧值必须被覆盖，不能两条并存。
        bp.set("src/main.ts", [cond(8, "a > 2")]);
        assert_eq!(bp.condition_of("src/main.ts", 8), Some("a > 2"));
        // 改回无条件：条件必须真的消失。
        bp.set("src/main.ts", [line(8)]);
        assert!(bp.hits("src/main.ts", 8));
        assert_eq!(bp.condition_of("src/main.ts", 8), None);
    }

    #[test]
    fn a_blank_condition_is_an_unconditional_breakpoint() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [cond(2, "   ")]);
        assert!(bp.hits("src/main.ts", 2));
        assert_eq!(
            bp.condition_of("src/main.ts", 2),
            None,
            "空白条件没有语义，必须退化成无条件"
        );
    }

    #[test]
    fn the_last_entry_for_a_line_wins() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [line(9), cond(9, "n === 1")]);
        assert_eq!(bp.condition_of("src/main.ts", 9), Some("n === 1"));
        // `lines_of` 也要去重成一行。
        assert_eq!(
            bp.lines_of("src/main.ts").map(Iterator::count),
            Some(1),
            "同一行出现两次只能算一个断点"
        );
    }
}
