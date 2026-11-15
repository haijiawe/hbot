//! 文件类工具：`read_file`、`write_file`、`edit_file`、`list_dir`。
//!
//! 全部走 [`hbot_workspace::Workspace`]：路径越界、`.hbot`/`.git` 过滤、
//! 原子写、乐观并发校验都在那里，工具层不重复实现（也就不会两处不一致）。
//!
//! 写类工具（`write_file` / `edit_file`）有两条路径，由 [`ToolCtx::edits`] 决定：
//!
//! - **装配了待审编辑端口**（Agent 回合）：读旧内容 → 产出 [`ProposedEdit`] →
//!   交给端口登记，**不落盘**。真正写入由用户 `edit_apply` 触发，落盘前再比对
//!   一次基线哈希（`AGENTS.md` §0.8：禁止静默覆盖）。
//! - **未装配**（网关自测、非 Agent 场景）：保持原来的直接写盘 + `expected_hash`
//!   乐观并发校验。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{HbotError, SandboxMode};
use hbot_workspace::{MAX_READ_BYTES, Workspace};
use serde_json::{Value, json};

use super::{Risk, Tool, ToolCtx, ToolOutput, blocking, normalize_relative, required_str};
use crate::edits::ProposedEdit;

/// 读当前内容：`Ok(Some((内容, 哈希)))`；文件不存在返回 `Ok(None)`。
async fn read_current(
    workspace: &Arc<Workspace>,
    rel: &Path,
) -> Result<Option<(String, String)>, HbotError> {
    let workspace = Arc::clone(workspace);
    let rel = rel.to_path_buf();
    match blocking(move || workspace.read_file(&rel, MAX_READ_BYTES)).await {
        Ok(file) => Ok(Some((file.content, file.hash))),
        Err(HbotError::NotFound { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// 登记待审编辑，返回给模型看的工具结果。
fn proposed_output(edit_id: hbot_core::EditId, edit: &ProposedEdit) -> ToolOutput {
    let removed: u64 = edit
        .hunks
        .iter()
        .map(|hunk| u64::from(hunk.old_lines))
        .sum();
    let added: u64 = edit
        .hunks
        .iter()
        .map(|hunk| u64::from(hunk.new_lines))
        .sum();
    ToolOutput::text(format!(
        "已生成待审编辑 {edit_id}\npath: {}\n-{removed} +{added} 行\n等待用户 edit_apply 后才会真正写入（当前磁盘内容未变）。",
        edit.path
    ))
}

/// 判断 `expected_hash` 是否与磁盘一致（大小写不敏感，哈希是十六进制）。
fn hash_matches(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

/// 读取 UTF-8 文本文件。
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取工作区内的 UTF-8 文本文件，返回内容与内容哈希（可作 write_file 的 expected_hash）。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "工作区相对路径，如 src/main.rs"
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "最多读取的字节数，默认 256 KiB，上限 2 MiB"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn path_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn requires_approval(&self, _sandbox: SandboxMode, _args: &Value) -> bool {
        false
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let path = required_str(self.name(), &args, "path")?;
        let requested = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(256 * 1024)
            .min(MAX_READ_BYTES);
        let workspace = Arc::clone(&ctx.workspace);
        let rel = PathBuf::from(path);
        let file = blocking(move || workspace.read_file(&rel, requested)).await?;
        let shown = normalize_relative(path);
        Ok(ToolOutput::text(format!(
            "path: {shown}\nhash: {hash}\nsize: {size}\n\n{content}",
            hash = file.hash,
            size = file.size,
            content = file.content,
        )))
    }
}

/// 写入（整文件覆盖）UTF-8 文本。
///
/// 带 `expected_hash` 时是乐观并发写：文件被别处改过就 `Conflict`，
/// 由模型重新读取后再写，不允许静默覆盖。
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "写入工作区内的 UTF-8 文本文件（整文件覆盖）。给了 expected_hash 时，若文件当前哈希不匹配则拒绝，避免覆盖他人改动。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "工作区相对路径；父目录必须已存在"
                },
                "content": {
                    "type": "string",
                    "description": "完整的新文件内容"
                },
                "expected_hash": {
                    "type": "string",
                    "description": "可选。上次 read_file 返回的 hash；不匹配则拒绝写入"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn path_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn may_create_path(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let path = required_str(self.name(), &args, "path")?.to_string();
        let content = required_str(self.name(), &args, "content")?.to_string();
        let expected_hash = args
            .get("expected_hash")
            .and_then(Value::as_str)
            .map(str::to_string);
        let workspace = Arc::clone(&ctx.workspace);
        let rel = PathBuf::from(&path);

        // 待审编辑路径：先核对基线，再登记（绝不直接写盘）。
        if let Some(proposer) = ctx.edits {
            let current = read_current(&workspace, &rel).await?;
            if let (Some(expected), Some((_, actual))) = (&expected_hash, &current)
                && !hash_matches(expected, actual)
            {
                return Err(HbotError::conflict(format!(
                    "文件 `{path}` 已被改动（期望哈希 {expected}，实际 {actual}）"
                )));
            }
            let edit = match current {
                Some((old, hash)) => {
                    ProposedEdit::modify(path.clone(), Some(old), Some(hash), content)
                }
                None => {
                    if expected_hash.is_some() {
                        return Err(HbotError::conflict(format!(
                            "文件 `{path}` 不存在，但给了 expected_hash"
                        )));
                    }
                    ProposedEdit::create(path.clone(), content)
                }
            };
            if edit.hunks.is_empty() {
                return Ok(ToolOutput::text(format!(
                    "内容没有变化，未生成编辑：{path}"
                )));
            }
            let edit_id = proposer.propose(ctx.session_id, ctx.call_id, edit.clone())?;
            return Ok(proposed_output(edit_id, &edit));
        }

        let written =
            blocking(move || workspace.write_file(&rel, &content, expected_hash.as_deref()))
                .await?;
        Ok(ToolOutput::text(format!(
            "已写入 {path}\nhash: {}\nsize: {}",
            written.hash, written.size
        )))
    }
}

/// 定点替换（把 `old_string` 换成 `new_string`）。
///
/// 比 `write_file` 更安全：不需要模型重述整个文件，也就不会顺手改掉别处。
/// 出现次数不唯一时必须显式 `replace_all`，否则拒绝——静默替换第一处
/// 会把"改了哪一处"变得不可复核。
#[derive(Clone, Copy, Debug, Default)]
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "把工作区文件里的 old_string 替换成 new_string（必须唯一匹配，或显式 replace_all）。只改这一段，不需要重述整个文件。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "工作区相对路径；文件必须已存在（新建请用 write_file）"
                },
                "old_string": {
                    "type": "string",
                    "description": "要被替换的原文（含足够上下文以保证唯一）"
                },
                "new_string": {
                    "type": "string",
                    "description": "替换后的新文本"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "是否替换全部匹配；默认 false（多处匹配时报错）"
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn path_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let path = required_str(self.name(), &args, "path")?.to_string();
        let old_string = required_str(self.name(), &args, "old_string")?.to_string();
        let new_string = required_str(self.name(), &args, "new_string")?.to_string();
        if old_string.is_empty() {
            return Err(HbotError::invalid("old_string 不能为空"));
        }
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let workspace = Arc::clone(&ctx.workspace);
        let rel = PathBuf::from(&path);
        let (old_content, base_hash) = read_current(&workspace, &rel).await?.ok_or_else(|| {
            HbotError::invalid(format!("文件不存在：{path}（新建请用 write_file）"))
        })?;

        let occurrences = old_content.matches(&old_string).count();
        if occurrences == 0 {
            return Err(HbotError::invalid(format!("在 {path} 中找不到 old_string")));
        }
        if occurrences > 1 && !replace_all {
            return Err(HbotError::invalid(format!(
                "old_string 在 {path} 中出现 {occurrences} 次：请加长上下文，或显式 replace_all=true"
            )));
        }
        let updated = if replace_all {
            old_content.replace(&old_string, &new_string)
        } else {
            old_content.replacen(&old_string, &new_string, 1)
        };
        if updated == old_content {
            return Ok(ToolOutput::text(format!(
                "内容没有变化，未生成编辑：{path}"
            )));
        }

        if let Some(proposer) = ctx.edits {
            let edit =
                ProposedEdit::modify(path.clone(), Some(old_content), Some(base_hash), updated);
            let edit_id = proposer.propose(ctx.session_id, ctx.call_id, edit.clone())?;
            return Ok(proposed_output(edit_id, &edit));
        }

        let write_path = rel.clone();
        let written =
            blocking(move || workspace.write_file(&write_path, &updated, Some(base_hash.as_str())))
                .await?;
        Ok(ToolOutput::text(format!(
            "已替换 {occurrences} 处 {path}\nhash: {}\nsize: {}",
            written.hash, written.size
        )))
    }
}

/// 列目录（一层，不递归）。
#[derive(Clone, Copy, Debug, Default)]
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "列出工作区内某个目录的直接子项（目录在前、按名排序）。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "工作区相对目录，默认工作区根"
                }
            },
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn path_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn requires_approval(&self, _sandbox: SandboxMode, _args: &Value) -> bool {
        false
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let workspace = Arc::clone(&ctx.workspace);
        let rel = PathBuf::from(&path);
        let entries = blocking(move || workspace.list_dir(&rel)).await?;
        if entries.is_empty() {
            return Ok(ToolOutput::text(format!("{path}: （空目录或全部被过滤）")));
        }
        let mut lines = Vec::with_capacity(entries.len() + 1);
        lines.push(format!("{path}:"));
        for entry in entries {
            let suffix = if entry.is_dir { "/" } else { "" };
            lines.push(format!(
                "  {}{suffix}    {} B",
                normalize_relative(&entry.path),
                entry.size
            ));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hbot_core::{EditId, SandboxMode};
    use serde_json::json;

    use super::*;
    use crate::cancel::CancellationToken;
    use crate::edits::EditProposer;
    use crate::test_support::{TestDir, session_id};

    fn ctx<'a>(
        workspace: &Arc<hbot_workspace::Workspace>,
        cancel: &'a CancellationToken,
    ) -> ToolCtx<'a> {
        ToolCtx {
            workspace: Arc::clone(workspace),
            sandbox: SandboxMode::WorkspaceWrite,
            cancel,
            session_id: session_id(),
            call_id: "call_test",
            edits: None,
            command: None,
        }
    }

    /// 记录提案，返回固定 id 的假端口。
    #[derive(Default)]
    struct RecordingProposer {
        edits: Mutex<Vec<ProposedEdit>>,
    }

    impl RecordingProposer {
        fn proposed(&self) -> Vec<ProposedEdit> {
            self.edits
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }
    }

    impl EditProposer for RecordingProposer {
        fn propose(
            &self,
            _session_id: hbot_core::SessionId,
            _call_id: &str,
            edit: ProposedEdit,
        ) -> Result<EditId, HbotError> {
            if let Ok(mut guard) = self.edits.lock() {
                guard.push(edit);
            }
            "11111111-1111-1111-1111-111111111111"
                .parse()
                .map_err(|_| HbotError::internal("测试 edit id"))
        }
    }

    #[tokio::test]
    async fn read_file_returns_content_and_hash() {
        let dir = TestDir::new("read");
        dir.write("a.txt", "hello");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = ReadFileTool
            .run(json!({ "path": "a.txt" }), &ctx(&workspace, &cancel))
            .await
            .expect("read");
        assert!(!output.is_error);
        assert!(output.content.contains("hello"));
        assert!(output.content.contains("hash: "));
    }

    #[tokio::test]
    async fn write_file_creates_and_reports_hash() {
        let dir = TestDir::new("write");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = WriteFileTool
            .run(
                json!({ "path": "new.txt", "content": "abc" }),
                &ctx(&workspace, &cancel),
            )
            .await
            .expect("write");
        assert!(output.content.contains("已写入"));
        assert_eq!(dir.read("new.txt"), "abc");
        assert!(dir.path().join("new.txt").exists(), "文件必须真的落盘");
    }

    #[tokio::test]
    async fn write_file_conflicts_on_stale_hash() {
        let dir = TestDir::new("write-conflict");
        dir.write("a.txt", "v1");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let error = WriteFileTool
            .run(
                json!({ "path": "a.txt", "content": "v2", "expected_hash": "deadbeef" }),
                &ctx(&workspace, &cancel),
            )
            .await
            .expect_err("conflict");
        assert!(matches!(error, HbotError::Conflict { .. }));
        assert_eq!(dir.read("a.txt"), "v1", "冲突时不得写入");
    }

    #[tokio::test]
    async fn list_dir_lists_entries() {
        let dir = TestDir::new("list");
        dir.write("a.txt", "x");
        dir.write("b.txt", "yy");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = ListDirTool
            .run(json!({}), &ctx(&workspace, &cancel))
            .await
            .expect("list");
        assert!(output.content.contains("a.txt"));
        assert!(output.content.contains("b.txt"));
    }

    /// 装配待审编辑端口后，`write_file` **不得**落盘。
    #[tokio::test]
    async fn write_file_proposes_instead_of_writing() {
        let dir = TestDir::new("write-propose");
        dir.write("a.txt", "v1");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let proposer = RecordingProposer::default();
        let mut tool_ctx = ctx(&workspace, &cancel);
        tool_ctx.edits = Some(&proposer);

        let output = WriteFileTool
            .run(json!({ "path": "a.txt", "content": "v2" }), &tool_ctx)
            .await
            .expect("propose");

        assert!(output.content.contains("待审编辑"), "{}", output.content);
        assert!(!output.content.contains("已写入"));
        assert_eq!(dir.read("a.txt"), "v1", "Agent 写入不得直接落盘");
        let proposed = proposer.proposed();
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0].path, "a.txt");
        assert_eq!(proposed[0].old_content.as_deref(), Some("v1"));
        assert_eq!(proposed[0].new_content, "v2");
        assert!(proposed[0].base_hash.is_some(), "必须记录基线哈希");
        assert_eq!(proposed[0].hunks.len(), 1);
    }

    /// 新建文件同样只出提案。
    #[tokio::test]
    async fn write_file_proposes_creation_without_base_hash() {
        let dir = TestDir::new("write-propose-new");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let proposer = RecordingProposer::default();
        let mut tool_ctx = ctx(&workspace, &cancel);
        tool_ctx.edits = Some(&proposer);

        WriteFileTool
            .run(json!({ "path": "new.txt", "content": "abc" }), &tool_ctx)
            .await
            .expect("propose");
        assert!(!dir.path().join("new.txt").exists(), "不得落盘");
        let proposed = proposer.proposed();
        assert_eq!(proposed[0].base_hash, None);
        assert!(proposed[0].is_new());
    }

    /// 内容没变时不出空提案（否则用户会收到一张无意义的 Diff 卡）。
    #[tokio::test]
    async fn write_file_same_content_makes_no_edit() {
        let dir = TestDir::new("write-same");
        dir.write("a.txt", "v1");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let proposer = RecordingProposer::default();
        let mut tool_ctx = ctx(&workspace, &cancel);
        tool_ctx.edits = Some(&proposer);

        let output = WriteFileTool
            .run(json!({ "path": "a.txt", "content": "v1" }), &tool_ctx)
            .await
            .expect("no-op");
        assert!(output.content.contains("没有变化"));
        assert!(proposer.proposed().is_empty());
    }

    #[tokio::test]
    async fn edit_file_proposes_patch() {
        let dir = TestDir::new("edit-propose");
        dir.write("a.txt", "hello world\nbye\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let proposer = RecordingProposer::default();
        let mut tool_ctx = ctx(&workspace, &cancel);
        tool_ctx.edits = Some(&proposer);

        let output = EditFileTool
            .run(
                json!({ "path": "a.txt", "old_string": "world", "new_string": "rust" }),
                &tool_ctx,
            )
            .await
            .expect("edit");
        assert!(output.content.contains("待审编辑"));
        assert_eq!(dir.read("a.txt"), "hello world\nbye\n", "不得落盘");
        let proposed = proposer.proposed();
        let last = proposed.last().expect("至少一条提案");
        assert_eq!(last.new_content, "hello rust\nbye\n");
        assert!(last.base_hash.is_some());
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = TestDir::new("edit-ambiguous");
        dir.write("a.txt", "x x x\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let error = EditFileTool
            .run(
                json!({ "path": "a.txt", "old_string": "x", "new_string": "y" }),
                &ctx(&workspace, &cancel),
            )
            .await
            .expect_err("必须拒绝歧义匹配");
        assert_eq!(error.code(), -32002);
        assert_eq!(dir.read("a.txt"), "x x x\n");
    }

    /// 未装配端口时保持直接写盘（网关自测与独立 CLI 依赖这条）。
    #[tokio::test]
    async fn edit_file_writes_directly_without_proposer() {
        let dir = TestDir::new("edit-direct");
        dir.write("a.txt", "hello world\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = EditFileTool
            .run(
                json!({ "path": "a.txt", "old_string": "world", "new_string": "rust" }),
                &ctx(&workspace, &cancel),
            )
            .await
            .expect("edit");
        assert!(output.content.contains("已替换"));
        assert_eq!(dir.read("a.txt"), "hello rust\n");
    }
}
