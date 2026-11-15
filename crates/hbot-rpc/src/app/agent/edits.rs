//! 待审编辑登记表：Agent 写入在这里"停"一步，等用户 `edit_apply`。
//!
//! ## 为什么不直接落盘
//!
//! `AGENTS.md` §0.7：Agent 写入必须走 Diff 逐块接受/拒绝，禁止静默覆盖。所以
//! `write_file` / `edit_file` 在 Agent 会话里**只产出提案**（[`ProposedEdit`]），
//! 由本模块登记并推 `agent.diff`；真正落盘发生在
//! [`crate::app::commands::agent::edit_apply`]，并且带**提案时的**基线哈希
//! （`-32003` 表示文件在提案之后被别人改过）。
//!
//! ## 归属
//!
//! 每条编辑记着发起它的会话：取消会话不影响已产出的 Diff（用户仍可以接受/拒绝），
//! 但审计与调试要能说清"这条 Diff 是哪一轮出来的"。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use hbot_agent::{EditHunk, ProposedEdit};
use hbot_core::{EditId, SessionId};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一条等待用户裁决的编辑。
#[derive(Clone, Debug)]
pub struct PendingEdit {
    /// 产出这条编辑的会话。
    pub session_id: SessionId,
    /// 项目内相对路径（已由网关规范化）。
    pub path: String,
    /// 提案时磁盘上的内容；新建文件为 `None`。
    pub old_content: Option<String>,
    /// 提案后的完整新内容。
    pub new_content: String,
    /// 提案时的基线哈希（乐观并发）；新建文件为 `None`。
    pub base_hash: Option<String>,
    /// 分块 Diff（`accept_hunks` 按 `index` 选择）。
    pub hunks: Vec<EditHunk>,
}

impl PendingEdit {
    /// 由工具的提案构造。
    pub fn from_proposal(session_id: SessionId, edit: ProposedEdit) -> Self {
        Self {
            session_id,
            path: edit.path,
            old_content: edit.old_content,
            new_content: edit.new_content,
            base_hash: edit.base_hash,
            hunks: edit.hunks,
        }
    }

    /// 是否是"新建文件"提案。
    pub fn is_new(&self) -> bool {
        self.base_hash.is_none() && self.old_content.is_none()
    }
}

/// 进程级登记表。
#[derive(Default)]
pub struct EditRegistry {
    entries: Mutex<HashMap<EditId, PendingEdit>>,
}

impl EditRegistry {
    /// 空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条编辑。
    pub fn insert(&self, edit_id: EditId, edit: PendingEdit) {
        lock(&self.entries).insert(edit_id, edit);
    }

    /// 取一条（克隆；调用方要落盘时再 [`Self::remove`]）。
    pub fn get(&self, edit_id: EditId) -> Option<PendingEdit> {
        lock(&self.entries).get(&edit_id).cloned()
    }

    /// 取走一条（`edit_apply` / `edit_reject` 用）。
    pub fn remove(&self, edit_id: EditId) -> Option<PendingEdit> {
        lock(&self.entries).remove(&edit_id)
    }

    /// 登记表条数。
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 丢弃全部待审编辑（换项目时调用：旧项目的 Diff 不该落到新项目里）。
    pub fn clear(&self) {
        lock(&self.entries).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::HbotError;

    fn edit_id() -> EditId {
        EditId::from_uuid(uuid::Uuid::nil())
    }

    fn proposal() -> ProposedEdit {
        ProposedEdit::modify(
            "a.txt",
            Some("old\n".to_string()),
            Some("hash".to_string()),
            "new\n",
        )
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let registry = EditRegistry::new();
        assert!(registry.is_empty());
        registry.insert(
            edit_id(),
            PendingEdit::from_proposal(SessionId::from_uuid(uuid::Uuid::nil()), proposal()),
        );
        let stored = registry.get(edit_id()).expect("登记过");
        assert_eq!(stored.path, "a.txt");
        assert_eq!(stored.old_content.as_deref(), Some("old\n"));
        assert_eq!(stored.new_content, "new\n");
        assert_eq!(stored.base_hash.as_deref(), Some("hash"));
        assert!(!stored.is_new());
        assert_eq!(stored.hunks.len(), 1);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn remove_takes_it_out() {
        let registry = EditRegistry::new();
        registry.insert(
            edit_id(),
            PendingEdit::from_proposal(SessionId::from_uuid(uuid::Uuid::nil()), proposal()),
        );
        assert!(registry.remove(edit_id()).is_some());
        assert!(registry.get(edit_id()).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn unknown_edit_is_none() {
        let registry = EditRegistry::new();
        assert!(registry.get(edit_id()).is_none());
        assert!(registry.remove(edit_id()).is_none());
    }

    #[test]
    fn creation_proposal_is_new() {
        let registry = EditRegistry::new();
        registry.insert(
            edit_id(),
            PendingEdit::from_proposal(
                SessionId::from_uuid(uuid::Uuid::nil()),
                ProposedEdit::create("b.txt", "x\n"),
            ),
        );
        assert!(registry.get(edit_id()).expect("登记过").is_new());
        // 让 `HbotError` 参与编译：错误码映射在这里也有一份断言，防止改 dto 时漏掉。
        assert_eq!(HbotError::not_found("edit", "x").code(), -32001);
    }
}
