//! `http_fetch`：**占位壳**，当前一律拒绝。
//!
//! 外部内容规则（architecture §11.1）要求所有进出网内容都按不可信数据处理，
//! 而网络出口、域名策略、脱敏都还没接入。所以这里只声明能力、执行即拒绝，
//! 避免"看起来能联网"造成模型重试或编造结果。

use std::time::Duration;

use async_trait::async_trait;
use hbot_core::HbotError;
use serde_json::{Value, json};

use super::{Risk, Tool, ToolCtx, ToolOutput};

/// 未接入的 `http_fetch` 占位。
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpFetchTool;

/// 网络工具不可用的统一解释。
pub const HTTP_UNAVAILABLE: &str =
    "网络访问尚未接入（http_fetch 为能力占位，未接入 DNS/脱敏/审计前一律拒绝）";

#[async_trait]
impl Tool for HttpFetchTool {
    fn name(&self) -> &str {
        "http_fetch"
    }

    fn description(&self) -> &str {
        "抓取一个 URL 的文本内容。（当前环境未接入网络出口，调用会被拒绝。）"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "http/https URL"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP 方法，默认 GET"
                },
                "headers": {
                    "type": "object",
                    "description": "附加请求头（键值均为字符串）"
                },
                "body": {
                    "type": "string",
                    "description": "可选请求体"
                }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn run(&self, _args: Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        Err(HbotError::denied(HTTP_UNAVAILABLE))
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
    async fn http_fetch_denies() {
        let dir = TestDir::new("http");
        let workspace: Arc<hbot_workspace::Workspace> = dir.workspace();
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            workspace,
            sandbox: SandboxMode::Danger,
            cancel: &cancel,
            session_id: session_id(),
            call_id: "call",
            edits: None,
            command: None,
        };
        let error = HttpFetchTool
            .run(json!({ "url": "https://example.com" }), &ctx)
            .await
            .expect_err("must deny");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert!(error.to_string().contains("网络访问尚未接入"));
    }

    #[test]
    fn http_fetch_schema_is_declared() {
        assert_eq!(HttpFetchTool.schema()["required"][0], "url");
        assert_eq!(HttpFetchTool.risk(), Risk::Network);
    }
}
