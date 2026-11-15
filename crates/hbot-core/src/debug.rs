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
//!    只做一次集合查找。
//! 4. 这一层**不出现 `serde_json::Value` 之外的类型**：变量值是不可信的 JS 值，用
//!    JSON 传，不可序列化的值由实现方替换成字符串 `<unserializable>`。

use std::collections::{BTreeMap, BTreeSet};

/// 断点集合：项目内相对路径（正斜杠）→ 1-based 行号。
///
/// 空集合 = 不暂停。行号 `0` 无意义，实现方应忽略（按 1-based 语义）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Breakpoints {
    files: BTreeMap<String, BTreeSet<u32>>,
}

impl Breakpoints {
    /// 空集合（不暂停）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 整体替换某个文件的断点。`lines` 为空表示"这个文件不再有断点"。
    ///
    /// 路径按调用方给的字符串原样保存；比较时不规范化（实现方负责传项目内相对路径，
    /// 且与插桩时用的 `fileId` 一致）。
    pub fn set(&mut self, file: impl Into<String>, lines: impl IntoIterator<Item = u32>) {
        let mut set = BTreeSet::new();
        for line in lines {
            if line > 0 {
                set.insert(line);
            }
        }
        let file = file.into();
        if set.is_empty() {
            self.files.remove(&file);
        } else {
            self.files.insert(file, set);
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

    /// 某个文件的断点行号。
    pub fn lines_of(&self, file: &str) -> Option<&BTreeSet<u32>> {
        self.files.get(file)
    }

    /// 全部断点（按文件名排序）。
    pub fn files(&self) -> impl Iterator<Item = (&str, &BTreeSet<u32>)> {
        self.files.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// 这一行是否命中。`file` 必须与 [`Breakpoints::set`] 时一致。
    pub fn hits(&self, file: &str, line: u32) -> bool {
        self.files
            .get(file)
            .is_some_and(|lines| lines.contains(&line))
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

    #[test]
    fn empty_breakpoints_never_hit() {
        let bp = Breakpoints::new();
        assert!(bp.is_empty());
        assert!(!bp.hits("src/main.ts", 1));
    }

    #[test]
    fn setting_an_empty_line_list_removes_the_file() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [3, 7]);
        assert!(bp.hits("src/main.ts", 7));
        bp.set("src/main.ts", Vec::<u32>::new());
        assert!(!bp.hits("src/main.ts", 7));
        assert!(bp.is_empty(), "空行表必须真的移除条目，否则会留下幽灵文件");
    }

    #[test]
    fn line_zero_is_ignored_not_stored() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [0, 5]);
        assert!(!bp.hits("src/main.ts", 0));
        assert!(bp.hits("src/main.ts", 5));
    }

    #[test]
    fn hits_is_per_file_not_global() {
        let mut bp = Breakpoints::new();
        bp.set("src/main.ts", [5]);
        assert!(!bp.hits("src/other.ts", 5));
    }

    #[test]
    fn files_is_sorted_for_stable_rendering() {
        let mut bp = Breakpoints::new();
        bp.set("src/z.ts", [1]);
        bp.set("src/a.ts", [1]);
        let names: Vec<&str> = bp.files().map(|(f, _)| f).collect();
        assert_eq!(names, vec!["src/a.ts", "src/z.ts"]);
    }
}
