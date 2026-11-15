//! 待审编辑（Agent 写文件必须走 Diff 逐块接受）。
//!
//! `AGENTS.md` §0.8：Agent 的写入**不许静默覆盖**，必须产出 Diff 让用户逐块接受
//! 或拒绝。但"谁保存待审编辑、谁落盘、冲突怎么判"是编排层的事（`hbot-rpc` 的
//! session），`hbot-agent` 只提供两份东西：
//!
//! - 纯数据：[`ProposedEdit`] / [`EditHunk`]（与 `agent.diff` 事件的 `files[].hunks`
//!   同构，字段一一对应）；
//! - 一个给工具用的端口：[`EditProposer`]——工具产出编辑后交给它登记，拿回
//!   `EditId`，**不直接写盘**。
//!
//! 工具层因此不需要知道 `EditId` 是谁生成的，也不需要知道审批规则（那是
//! [`crate::gateway`] 的事）：装配方把 [`EditProposer`] 塞进
//! [`crate::gateway::ToolGateway::with_edit_proposer`]，Agent 回合里每一次
//! `write_file` / `edit_file` 就自动变成待审编辑；没装配时保持直接写盘的老行为
//! （供网关自身测试与非 Agent 场景使用）。
//!
//! 分块接受用 [`apply_hunks`]：只接受部分块时，未被接受的块保留旧内容。

use std::collections::BTreeSet;

use hbot_core::{EditId, HbotError, SessionId};

/// 一个 Diff 块（与 `agent.diff` 事件的 `hunks` 逐字段对应）。
///
/// 行号从 1 开始；`lines` 是 unified diff 的正文行（`-` 删除 / `+` 新增），
/// 不带 hunk 头。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditHunk {
    /// 块序号（从 0 开始，`edit_apply` 的 `accept_hunks` 用它）。
    pub index: u32,
    /// 旧文件起始行（1 起）。
    pub old_start: u32,
    /// 旧文件覆盖行数。
    pub old_lines: u32,
    /// 新文件起始行（1 起）。
    pub new_start: u32,
    /// 新文件覆盖行数。
    pub new_lines: u32,
    /// unified diff 正文行。
    pub lines: Vec<String>,
}

impl EditHunk {
    /// 从 `lines` 里取旧内容（`-` 行去掉前缀）。
    fn old_text(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter(|line| line.starts_with('-'))
            .map(|line| line[1..].to_string())
            .collect()
    }

    /// 从 `lines` 里取新内容（`+` 行去掉前缀）。
    fn new_text(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter(|line| line.starts_with('+'))
            .map(|line| line[1..].to_string())
            .collect()
    }

    /// 去掉前缀后的正文（UI 展示用）。
    pub fn display_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .map(|line| line.get(1..).unwrap_or(""))
            .collect()
    }
}

/// 待审编辑：一次工具写入产出的完整提案。
#[derive(Clone, Debug)]
pub struct ProposedEdit {
    /// 工作区相对路径（已被网关规范化）。
    pub path: String,
    /// 提案时的旧内容；新建文件为 `None`。
    pub old_content: Option<String>,
    /// 提案的完整新内容（整文件，不是补丁）。
    pub new_content: String,
    /// 提案时旧文件的哈希（乐观并发基线）；新建文件为 `None`。
    pub base_hash: Option<String>,
    /// 分块 Diff。
    pub hunks: Vec<EditHunk>,
}

impl ProposedEdit {
    /// 修改已存在文件。
    pub fn modify(
        path: impl Into<String>,
        old_content: Option<String>,
        base_hash: Option<String>,
        new_content: impl Into<String>,
    ) -> Self {
        let new_content = new_content.into();
        let hunks = diff_hunks(old_content.as_deref().unwrap_or(""), &new_content);
        Self {
            path: path.into(),
            old_content,
            new_content,
            base_hash,
            hunks,
        }
    }

    /// 新建文件（旧内容视为空）。
    pub fn create(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self::modify(path, None, None, content)
    }

    /// 是否新建（用于 `edit_apply` 判断"文件是否已被别人创建"）。
    pub fn is_new(&self) -> bool {
        self.base_hash.is_none() && self.old_content.is_none()
    }
}

/// 登记待审编辑的端口（实现方决定存哪、怎么发 `agent.diff` 事件）。
///
/// 工具在**拿到返回值之后**才把"已生成待审编辑"写进工具结果；返回
/// `Err` 表示登记失败（例如会话已被取消），工具调用随之失败。
pub trait EditProposer: Send + Sync {
    /// 登记一个待审编辑，返回它的 id。
    fn propose(
        &self,
        session_id: SessionId,
        call_id: &str,
        edit: ProposedEdit,
    ) -> Result<EditId, HbotError>;
}

/// 按行求差：公共前缀/公共后缀之外的中间段合成**一个** Diff 块。
///
/// 只产出一个块是有意的：`edit_apply` 的接受粒度是块，块越碎，
/// "接受一半"的概率越高；整文件覆盖与"替换一段"都能用一个块表达。
/// 完全相同时返回空 `Vec`（没有可接受的改动）。
pub fn diff_hunks(old: &str, new: &str) -> Vec<EditHunk> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if prefix == old_lines.len() && prefix == new_lines.len() {
        return Vec::new();
    }
    let max_suffix = old_lines.len().min(new_lines.len()).saturating_sub(prefix);
    let mut suffix = 0;
    while suffix < max_suffix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    let mut lines = Vec::with_capacity(removed.len() + added.len());
    for line in removed {
        lines.push(format!("-{line}"));
    }
    for line in added {
        lines.push(format!("+{line}"));
    }
    vec![EditHunk {
        index: 0,
        old_start: u32::try_from(prefix + 1).unwrap_or(u32::MAX),
        old_lines: u32::try_from(removed.len()).unwrap_or(u32::MAX),
        new_start: u32::try_from(prefix + 1).unwrap_or(u32::MAX),
        new_lines: u32::try_from(added.len()).unwrap_or(u32::MAX),
        lines,
    }]
}

/// 去掉末尾换行后按 `\n` 切行，保证 `join("\n")` 能原样还原。
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').collect()
}

/// 按块接受结果重建文件内容。
///
/// `accepted` 里的序号必须存在于 `hunks`；未被接受的块保留旧内容（等于拒绝）。
/// 校验顺序（任何一条不满足都返回 `-32002`）：块序号存在、行号非 0、块按旧起始行
/// 升序且不重叠、块的正文行数与 `old_lines` / `new_lines` 自洽。
pub fn apply_hunks(
    old: &str,
    hunks: &[EditHunk],
    accepted: &BTreeSet<u32>,
) -> Result<String, HbotError> {
    for index in accepted {
        if !hunks.iter().any(|hunk| hunk.index == *index) {
            return Err(HbotError::invalid(format!("未知的 Diff 块序号 {index}")));
        }
    }

    let old_lines = split_lines(old);
    let mut out: Vec<String> = Vec::with_capacity(old_lines.len());
    let mut cursor = 0usize;
    let mut last_start = 0u32;

    for hunk in hunks {
        if hunk.old_start == 0 || hunk.new_start == 0 {
            return Err(HbotError::invalid(format!(
                "Diff 块 {} 的行号必须从 1 开始",
                hunk.index
            )));
        }
        if hunk.old_start <= last_start {
            return Err(HbotError::invalid(format!(
                "Diff 块 {} 的行号顺序不对（要求按旧起始行升序且不重叠）",
                hunk.index
            )));
        }
        last_start = hunk.old_start;

        let old_text = hunk.old_text();
        let new_text = hunk.new_text();
        if old_text.len() != hunk.old_lines as usize || new_text.len() != hunk.new_lines as usize {
            return Err(HbotError::invalid(format!(
                "Diff 块 {} 的正文行数与行号区间不一致",
                hunk.index
            )));
        }

        let start = (hunk.old_start - 1) as usize;
        if start < cursor || start + old_text.len() > old_lines.len() {
            return Err(HbotError::invalid(format!(
                "Diff 块 {} 超出旧文件范围",
                hunk.index
            )));
        }
        // 块之前的未改动行原样保留。
        out.extend(
            old_lines[cursor..start]
                .iter()
                .map(|line| (*line).to_string()),
        );
        cursor = start + old_text.len();

        if accepted.contains(&hunk.index) {
            out.extend(new_text);
        } else {
            out.extend(old_text);
        }
    }

    if cursor < old_lines.len() {
        out.extend(old_lines[cursor..].iter().map(|line| (*line).to_string()));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(indexes: &[u32]) -> BTreeSet<u32> {
        indexes.iter().copied().collect()
    }

    #[test]
    fn identical_content_has_no_hunks() {
        assert!(diff_hunks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn whole_file_creation_is_one_hunk() {
        let hunks = diff_hunks("", "a\nb");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 0);
        assert_eq!(hunks[0].new_lines, 2);
        assert_eq!(hunks[0].lines, vec!["+a".to_string(), "+b".to_string()]);
    }

    #[test]
    fn middle_change_keeps_context_out_of_the_hunk() {
        let hunks = diff_hunks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 2);
        assert_eq!(hunks[0].old_lines, 1);
        assert_eq!(hunks[0].lines, vec!["-b".to_string(), "+B".to_string()]);
    }

    #[test]
    fn applying_all_hunks_yields_new_content() {
        // 构造两个块：手工拼（`diff_hunks` 只产一个），用来验证多块逻辑。
        let old = "a\nb\nc\nd\n";
        let hunks = vec![
            EditHunk {
                index: 0,
                old_start: 2,
                old_lines: 1,
                new_start: 2,
                new_lines: 1,
                lines: vec!["-b".to_string(), "+B".to_string()],
            },
            EditHunk {
                index: 1,
                old_start: 4,
                old_lines: 1,
                new_start: 4,
                new_lines: 1,
                lines: vec!["-d".to_string(), "+D".to_string()],
            },
        ];
        let all = apply_hunks(old, &hunks, &accepted(&[0, 1])).expect("apply");
        assert_eq!(all, "a\nB\nc\nD\n");
    }

    #[test]
    fn partial_accept_keeps_rejected_hunks_old() {
        let old = "a\nb\nc\nd\n";
        let hunks = vec![
            EditHunk {
                index: 0,
                old_start: 2,
                old_lines: 1,
                new_start: 2,
                new_lines: 1,
                lines: vec!["-b".to_string(), "+B".to_string()],
            },
            EditHunk {
                index: 1,
                old_start: 4,
                old_lines: 1,
                new_start: 4,
                new_lines: 1,
                lines: vec!["-d".to_string(), "+D".to_string()],
            },
        ];
        let partial = apply_hunks(old, &hunks, &accepted(&[1])).expect("apply");
        assert_eq!(partial, "a\nb\nc\nD\n");
    }

    #[test]
    fn unknown_hunk_index_is_invalid() {
        let hunks = diff_hunks("a\n", "b\n");
        let error = apply_hunks("a\n", &hunks, &accepted(&[7])).expect_err("must reject");
        assert_eq!(error.code(), -32002);
    }

    #[test]
    fn hunk_body_must_match_line_counts() {
        let hunks = vec![EditHunk {
            index: 0,
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 1,
            lines: vec!["-a".to_string(), "+b".to_string()],
        }];
        let error = apply_hunks("a\n", &hunks, &accepted(&[0])).expect_err("must reject");
        assert_eq!(error.code(), -32002);
    }
}
