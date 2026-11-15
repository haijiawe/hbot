//! 文件类工具：`read_file`、`write_file`、`list_dir`。
//!
//! 全部走 [`hbot_workspace::Workspace`]：路径越界、`.hbot`/`.git` 过滤、
//! 原子写、乐观并发校验都在那里，工具层不重复实现（也就不会两处不一致）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{HbotError, SandboxMode};
use hbot_workspace::MAX_READ_BYTES;
use serde_json::{Value, json};

use super::{Risk, Tool, ToolCtx, ToolOutput, blocking, normalize_relative, required_str};

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
        let written =
            blocking(move || workspace.write_file(&rel, &content, expected_hash.as_deref()))
                .await?;
        Ok(ToolOutput::text(format!(
            "已写入 {path}\nhash: {}\nsize: {}",
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
    use hbot_core::SandboxMode;
    use serde_json::json;

    use super::*;
    use crate::cancel::CancellationToken;
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
}
