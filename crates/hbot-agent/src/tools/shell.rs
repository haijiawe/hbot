//! `run_command`：**占位壳**，当前一律拒绝。
//!
//! 命令隔离（Job Object / landlock / seatbelt）尚未接入，而 AGENTS.md §4.4
//! 明确要求"建不出隔离就拒绝执行，不回退名字白名单"。所以这里：
//!
//! - schema 照样声明，能力矩阵能如实看到"有这个工具"；
//! - [`Tool::run`] 直接返回 [`HbotError::Denied`]，**不伪造成功、不空跑**；
//! - 不注册任何"名字白名单"或"安全命令列表"——那正是旧项目的坑。
//!
//! 参数解析写在这里是为了让接入隔离的人有确定的入口，而不是留空 schema。

use std::time::Duration;

use async_trait::async_trait;
use hbot_core::HbotError;
use serde_json::{Value, json};

use super::{Risk, Tool, ToolCtx, ToolOutput};

/// 未接入的 `run_command` 占位。
#[derive(Clone, Copy, Debug, Default)]
pub struct RunCommandTool;

/// 命令工具不可用的统一解释（也用于审计/日志）。
pub const COMMAND_UNAVAILABLE: &str = "命令隔离尚未接入";

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "在项目内执行命令。（当前环境未接入命令隔离，调用会被拒绝；不要用其它方式绕过。）"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "可执行文件名，如 cargo"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "参数数组（不经 shell）"
                },
                "cwd": {
                    "type": "string",
                    "description": "工作区相对工作目录，默认根"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "超时毫秒数，默认 30000"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Command
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn run(&self, _args: Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        Err(HbotError::denied(COMMAND_UNAVAILABLE))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hbot_core::SandboxMode;
    use serde_json::json;

    use super::*;
    use crate::cancel::CancellationToken;
    use crate::test_support::{TestDir, session_id};

    #[tokio::test]
    async fn run_command_denies_and_never_pretends_success() {
        let dir = TestDir::new("run-command");
        let workspace: Arc<hbot_workspace::Workspace> = dir.workspace();
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            workspace,
            sandbox: SandboxMode::Danger,
            cancel: &cancel,
            session_id: session_id(),
            call_id: "call",
        };
        // 即使在最宽的沙箱档，也不接命令：回退白名单是禁止的。
        let error = RunCommandTool
            .run(json!({ "command": "cargo", "args": ["test"] }), &ctx)
            .await
            .expect_err("must deny");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert!(error.to_string().contains(COMMAND_UNAVAILABLE));
    }

    #[test]
    fn run_command_schema_is_declared() {
        let schema = RunCommandTool.schema();
        assert_eq!(schema["required"][0], "command");
        assert_eq!(RunCommandTool.risk(), Risk::Command);
        assert_eq!(RunCommandTool.min_sandbox(), SandboxMode::WorkspaceWrite);
    }
}
