//! 工具抽象、注册表与参数校验。
//!
//! 一个工具要回答四件事：
//!
//! 1. **叫什么、长什么样**：[`Tool::name`] / [`Tool::description`] / [`Tool::schema`]，
//!    后者直接进 provider 的 `tools` 字段；
//! 2. **风险与最低沙箱档**：[`Tool::risk`] / [`Tool::min_sandbox`]，
//!    由网关在执行前判定，工具自己不做权限判断；
//! 3. **是否碰路径**：[`Tool::path_arg`] / [`Tool::may_create_path`]，
//!    让网关知道该对哪个参数做 realpath 归一化；
//! 4. **怎么执行**：[`Tool::run`]，只负责业务，不做审批、不写审计。
//!
//! 这样"顺序"只有网关一处，工具无法绕开。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::{HbotError, SandboxMode, SessionId};
use hbot_workspace::Workspace;
use serde_json::Value;

use crate::cancel::CancellationToken;
use crate::provider::ToolSpec;

pub mod fs;
pub mod http;
pub mod search;
pub mod shell;

pub use fs::{ListDirTool, ReadFileTool, WriteFileTool};
pub use http::HttpFetchTool;
pub use search::SearchTool;
pub use shell::RunCommandTool;

/// 工具默认超时。
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// 单个工具结果进入上下文前的字节上限。
///
/// 超限截断并标记 [`ToolOutput::truncated`]；不静默丢内容，也不让一个
/// `cat` 把整个上下文预算吃光。
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// 工具风险类别。
///
/// 类别只用于**展示与默认策略**，绝不用于"整类放行"——审批键是完整动作
/// （工具 + 规范化参数 + 沙箱档），见 [`crate::approvals`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// 只读：读文件、列目录、搜索。
    Read,
    /// 写：改文件、建目录。
    Write,
    /// 执行命令。
    Command,
    /// 访问网络。
    Network,
    /// 外部内容（别的进程/服务给的不可信数据）。
    External,
}

impl Risk {
    /// 稳定的字符串形式（写审计、UI、日志用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Command => "command",
            Self::Network => "network",
            Self::External => "external",
        }
    }
}

impl std::fmt::Display for Risk {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 工具执行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// 回填到上下文的内容。
    pub content: String,
    /// 是否因为超过 [`MAX_TOOL_OUTPUT_BYTES`] 被截断。
    pub truncated: bool,
    /// 是否是"工具跑完了但结果表示失败"（区别于参数/审批错误）。
    pub is_error: bool,
}

impl ToolOutput {
    /// 成功输出。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            is_error: false,
        }
    }

    /// 失败输出（作为工具结果回填给模型，让它自己纠正）。
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            is_error: true,
        }
    }
}

/// 工具执行上下文。
///
/// 只给工具它需要的东西：工作区、沙箱档、取消令牌、会话与调用 id（审计/日志
/// 关联用）。**不给审批器、不给审计器**——工具不该能自己决定放行。
pub struct ToolCtx<'a> {
    /// 工作区（`Arc` 便于 `spawn_blocking` 里持有）。
    pub workspace: Arc<Workspace>,
    /// 当前沙箱档。
    pub sandbox: SandboxMode,
    /// 取消令牌。
    pub cancel: &'a CancellationToken,
    /// 会话 id。
    pub session_id: SessionId,
    /// 本次工具调用 id。
    pub call_id: &'a str,
}

/// 一个可被模型调用的工具。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（对应 `function.name`）。
    fn name(&self) -> &str;

    /// 给模型看的一句话说明。
    fn description(&self) -> &str;

    /// JSON Schema 参数声明（`type: object` + `properties` + `required`）。
    fn schema(&self) -> Value;

    /// 风险类别。
    fn risk(&self) -> Risk;

    /// 最低沙箱档；低于它直接拒（不进审批）。
    fn min_sandbox(&self) -> SandboxMode {
        match self.risk() {
            Risk::Read => SandboxMode::ReadOnly,
            _ => SandboxMode::WorkspaceWrite,
        }
    }

    /// 哪个参数是路径（网关据此做 realpath 归一化）。
    fn path_arg(&self) -> Option<&'static str> {
        None
    }

    /// 该路径是否允许"还不存在"（新建文件）。
    fn may_create_path(&self) -> bool {
        false
    }

    /// 是否需要审批。
    ///
    /// 默认只读免审批；写/命令/网络必须审批。工具可以覆写，但不能覆写成
    /// "按类别放行"——是否放行由 [`crate::approvals`] 的完整键决定。
    fn requires_approval(&self, sandbox: SandboxMode, args: &Value) -> bool {
        let _ = (sandbox, args);
        self.risk() != Risk::Read
    }

    /// 单次执行超时。
    fn timeout(&self) -> Duration {
        DEFAULT_TOOL_TIMEOUT
    }

    /// 执行（不做权限/审批/审计）。
    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError>;
}

/// 工具注册表（静态表：注册后不变，读用 `&self`）。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工具；重名直接替换旧项（并记录告警）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|item| item.name() == tool.name())
        {
            tracing::warn!(tool = tool.name(), "工具重名，替换旧注册");
            *existing = tool;
            return;
        }
        self.tools.push(tool);
    }

    /// 按名字取工具。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name).cloned()
    }

    /// 是否存在。
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// 工具声明（给 provider 的 `tools` 字段）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }

    /// 工具名列表（进 system prompt）。
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    /// 已注册数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 默认工具集。
    ///
    /// `run_command` 与 `http_fetch` 也在这里注册——能力矩阵要求"声明存在但
    /// 执行时明确拒绝"，而不是偷偷藏起来（藏起来模型会编造调用）。
    pub fn default_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(ListDirTool));
        registry.register(Arc::new(SearchTool));
        registry.register(Arc::new(RunCommandTool));
        registry.register(Arc::new(HttpFetchTool));
        registry
    }
}

/// 参数校验（网关第一步）。
///
/// 只做**必填 + 类型**两件事：这是本地能确定性判定、且模型最容易搞错的部分。
/// 语义约束（路径是否越界、内容是否合法）留给工具自己，报错也更好懂。
pub fn validate_args(tool_name: &str, schema: &Value, args: &Value) -> Result<(), HbotError> {
    let Some(object) = args.as_object() else {
        return Err(HbotError::invalid(format!(
            "工具 `{tool_name}` 的参数必须是 JSON 对象"
        )));
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required {
            let Some(key) = key.as_str() else { continue };
            if !object.contains_key(key) {
                return Err(HbotError::invalid(format!(
                    "工具 `{tool_name}` 缺少必填参数 `{key}`"
                )));
            }
        }
    }
    let deny_unknown = matches!(schema.get("additionalProperties"), Some(Value::Bool(false)));
    for (key, value) in object {
        match properties.get(key) {
            Some(property) => {
                if let Some(expected) = property.get("type").and_then(Value::as_str)
                    && !matches_type(value, expected)
                {
                    return Err(HbotError::invalid(format!(
                        "工具 `{tool_name}` 的参数 `{key}` 类型应为 {expected}"
                    )));
                }
            }
            None => {
                if deny_unknown {
                    return Err(HbotError::invalid(format!(
                        "工具 `{tool_name}` 不认识参数 `{key}`"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // 不认识的类型声明不阻塞（避免比模型还严格）。
        _ => true,
    }
}

/// 按 [`MAX_TOOL_OUTPUT_BYTES`] 截断内容，返回 `(内容, 是否截断)`。
///
/// 按 UTF-8 边界切，避免产生半个汉字。
pub fn clamp_output(content: String) -> (String, bool) {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return (content, false);
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let mut clamped = content[..end].to_string();
    clamped.push_str("\n…（结果过长已截断）");
    (clamped, true)
}

/// 在阻塞线程池里跑同步工具逻辑（`hbot-workspace` 的文件/搜索 API 是同步的）。
///
/// 直接在大 async 里调会堵住 reactor；统一走 `spawn_blocking`，
/// 线程 panic 也转成 [`HbotError::Internal`]，不把 panic 抛给循环。
pub(crate) async fn blocking<T, F>(task: F) -> Result<T, HbotError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, HbotError> + Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result,
        Err(err) => Err(HbotError::internal(format!("工具线程异常：{err}"))),
    }
}

/// 从参数里取必填字符串。
pub fn required_str<'a>(tool: &str, args: &'a Value, key: &str) -> Result<&'a str, HbotError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HbotError::invalid(format!("工具 `{tool}` 缺少字符串参数 `{key}`")))
}

/// 把路径字符串统一成工作区相对形式（`/` 分隔）。
pub fn normalize_relative(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn validate_reports_missing_required() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let error = validate_args("read_file", &schema, &json!({})).expect_err("missing");
        assert!(matches!(error, HbotError::Invalid { .. }));
        assert!(error.to_string().contains("path"));
    }

    #[test]
    fn validate_reports_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let error = validate_args("read_file", &schema, &json!({ "path": 1 })).expect_err("type");
        assert!(matches!(error, HbotError::Invalid { .. }));
    }

    #[test]
    fn validate_accepts_integer_floats() {
        let schema = json!({
            "type": "object",
            "properties": { "max_results": { "type": "integer" } },
        });
        validate_args("search", &schema, &json!({ "max_results": 10.0 })).expect("integer");
    }

    #[test]
    fn validate_rejects_unknown_when_closed() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "additionalProperties": false,
        });
        let error = validate_args("read_file", &schema, &json!({ "path": "a", "x": 1 }))
            .expect_err("extra");
        assert!(matches!(error, HbotError::Invalid { .. }));
    }

    #[test]
    fn validate_rejects_non_object_args() {
        let schema = json!({ "type": "object" });
        let error = validate_args("read_file", &schema, &json!("x")).expect_err("not object");
        assert!(matches!(error, HbotError::Invalid { .. }));
    }

    #[test]
    fn clamp_output_is_utf8_safe() {
        let content = "汉".repeat(MAX_TOOL_OUTPUT_BYTES);
        let (clamped, truncated) = clamp_output(content);
        assert!(truncated);
        assert!(clamped.len() <= MAX_TOOL_OUTPUT_BYTES + 64);
        assert!(clamped.contains("已截断"));
    }

    #[test]
    fn registry_registers_and_replaces() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadFileTool));
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("read_file"));
        assert!(registry.get("nope").is_none());
        assert_eq!(registry.names(), vec!["read_file".to_string()]);
        assert_eq!(registry.specs()[0].name, "read_file");
    }

    #[test]
    fn normalize_relative_uses_forward_slashes() {
        assert_eq!(normalize_relative("src\\main.rs"), "src/main.rs");
        assert_eq!(normalize_relative("src/main.rs"), "src/main.rs");
    }
}
