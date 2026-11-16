//! `search`：在工作区内按文本/正则搜索。
//!
//! [`hbot_workspace::Workspace::search`] 是**同步** API（遍历文件、跳过二进制），
//! 这里必须放进 `spawn_blocking`，否则会阻塞异步 reactor；取消通过
//! [`CancellationToken::flag`] 传进去，遍历中途即可停下。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{HbotError, SandboxMode};
use hbot_workspace::{DEFAULT_MAX_RESULTS, MAX_SEARCH_FILE_BYTES, SearchHit, SearchOptions};
use serde_json::{Value, json};

use super::{Risk, Tool, ToolCtx, ToolOutput, blocking, normalize_relative, required_str};

/// 文本 / 正则搜索。
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "在工作区内搜索文本（默认按字面量、忽略大小写）；返回 路径:行号: 内容。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "要搜索的文本或正则"
                },
                "regex": {
                    "type": "boolean",
                    "description": "是否按正则解释 query，默认 false"
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "是否区分大小写，默认 false"
                },
                "globs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可选的文件过滤，如 [\"*.rs\"]"
                },
                "max_results": {
                    "type": "integer",
                    "description": "最多返回多少条命中，默认 1000"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn requires_approval(&self, _sandbox: SandboxMode, _args: &Value) -> bool {
        false
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let query = required_str(self.name(), &args, "query")?.to_string();
        let query_for_search = query.clone();
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(DEFAULT_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, DEFAULT_MAX_RESULTS);
        let options = SearchOptions {
            regex: args.get("regex").and_then(Value::as_bool).unwrap_or(false),
            case_sensitive: args
                .get("case_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            globs: args
                .get("globs")
                .and_then(Value::as_array)
                .map(|globs| {
                    globs
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            max_results,
            max_file_size: MAX_SEARCH_FILE_BYTES,
        };
        let workspace = Arc::clone(&ctx.workspace);
        let cancel = ctx.cancel.flag();
        let hits: Arc<Mutex<Vec<SearchHit>>> = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&hits);
        let summary = blocking(move || {
            workspace.search(&query_for_search, &options, cancel, |hit| {
                match collected.lock() {
                    Ok(mut guard) => guard.push(hit),
                    // 锁中毒只影响展示，不该让搜索整体失败；标注后继续收。
                    Err(poisoned) => poisoned.into_inner().push(hit),
                }
            })
        })
        .await?;
        let hits = hits.lock().map(|guard| guard.clone()).unwrap_or_default();

        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "没有命中：{query:?}（扫描 {} 个文件）",
                summary.files_scanned
            )));
        }
        let mut lines = Vec::with_capacity(hits.len() + 1);
        for hit in &hits {
            lines.push(format!(
                "{}:{}: {}",
                normalize_relative(&hit.path),
                hit.line_number,
                hit.line.trim_end()
            ));
        }
        let mut footer = format!(
            "共 {} 条；扫描 {} 个文件",
            hits.len(),
            summary.files_scanned
        );
        if summary.limit_reached {
            footer.push_str("；已达上限，结果可能不完整");
        }
        if summary.cancelled {
            footer.push_str("；搜索被取消");
        }
        lines.push(footer);
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
            edits: None,
            command: None,
            device: None,
        }
    }

    #[tokio::test]
    async fn search_finds_text_with_line_numbers() {
        let dir = TestDir::new("search");
        dir.write("a.rs", "fn main() {}\nlet needle = 1;\n");
        dir.write("b.txt", "nothing here\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = SearchTool
            .run(json!({ "query": "needle" }), &ctx(&workspace, &cancel))
            .await
            .expect("search");
        assert!(!output.is_error);
        assert!(output.content.contains("a.rs:2"), "{}", output.content);
        assert!(!output.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn search_reports_no_hits() {
        let dir = TestDir::new("search-empty");
        dir.write("a.txt", "hello\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = SearchTool
            .run(json!({ "query": "zzz" }), &ctx(&workspace, &cancel))
            .await
            .expect("search");
        assert!(output.content.contains("没有命中"));
    }

    #[tokio::test]
    async fn search_honours_globs() {
        let dir = TestDir::new("search-glob");
        dir.write("a.rs", "needle\n");
        dir.write("b.md", "needle\n");
        let workspace = dir.workspace();
        let cancel = CancellationToken::new();
        let output = SearchTool
            .run(
                json!({ "query": "needle", "globs": ["*.rs"] }),
                &ctx(&workspace, &cancel),
            )
            .await
            .expect("search");
        assert!(output.content.contains("a.rs:1"));
        assert!(!output.content.contains("b.md"));
    }
}
