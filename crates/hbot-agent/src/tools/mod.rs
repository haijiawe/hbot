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
use hbot_core::{
    CommandRunner, DeviceBackend, DeviceCapability, HbotError, SandboxMode, SessionId,
};
use hbot_workspace::Workspace;
use serde_json::Value;

use crate::cancel::CancellationToken;
use crate::provider::ToolSpec;

pub mod device;
pub mod fs;
pub mod http;
pub mod search;
pub mod shell;

pub use device::ReadScreenTool;
pub use fs::{CreateDirTool, EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
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
    /// 待审编辑端口；`Some` 时写类工具**不落盘**，改为登记待审编辑。
    ///
    /// 装配方（`hbot-rpc` 的 Agent 会话）总会设置它；网关自测与非 Agent 场景
    /// 保持 `None`（工具直接写盘）。
    pub edits: Option<&'a dyn crate::edits::EditProposer>,
    /// 受控命令执行端口（`Option` 是刻意的：装配缺失必须能被表达）。
    ///
    /// `None` 时 [`RunCommandTool`] 保持诚实拒绝，并且错误消息说的是
    /// "没有装配命令执行器"——不是"平台不支持"：前者该由组合根修，后者换机器才有用，
    /// 混成一句话就没法定位。
    pub command: Option<Arc<dyn CommandRunner>>,
    /// 设备执行端口（截屏 / OCR / 输入 / 窗口 / 剪贴板）。
    ///
    /// 只给**执行**能力，不给判据：要不要做由网关的 `Capability` 步与
    /// `DeviceGate` 决定。`None` 时 [`ReadScreenTool`] 诚实拒绝——同样的理由
    /// 和 `command` 一样，装配缺失要能被表达。
    pub device: Option<Arc<dyn DeviceBackend>>,
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

    /// 这个工具要求项目声明哪些设备能力（网关的第一步就判它）。
    ///
    /// 默认空 = 不需要任何能力：绝大多数工具（读写文件、搜索、跑命令）与设备
    /// 无关。**新增工具必须显式想一次**"它要不要能力"，而不是继承一个宽松默认
    /// 就了事——所以这里的默认值只在"新工具确实无关"时才成立，登记表
    /// （[`ToolRegistry::with_defaults`]）会为每个工具写一行注释。
    ///
    /// 判的是**声明层**（项目清单有没有写 `permissions.device.sub`），不是
    /// 运行时授权：后者由设备闸每次调用时判。
    fn required_capabilities(&self) -> &'static [DeviceCapability] {
        &[]
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
    ///
    /// 这里的 `run_command` **没有程序清单**（空清单 = 一条都跑不了）。要真跑命令，
    /// 用 [`ToolRegistry::default_tools_with_allowed_programs`]，让"允许跑什么"成为
    /// 装配处的一次显式决定。
    pub fn default_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(EditFileTool));
        registry.register(Arc::new(ListDirTool));
        registry.register(Arc::new(CreateDirTool));
        registry.register(Arc::new(SearchTool));
        registry.register(Arc::new(RunCommandTool::new()));
        registry.register(Arc::new(HttpFetchTool));
        // 第 9 个工具，也是唯一一个需要设备能力的：它要项目清单声明 `ocr`
        // （网关的 `Capability` 步判），并且每次调用都要审批（`Risk::External`）。
        // 前 8 个工具都不需要能力，登记时不必写 `required_capabilities`——
        // 但新增工具时请先想一次"它要不要能力"，别默认按"不要"处理。
        registry.register(Arc::new(ReadScreenTool));
        registry
    }

    /// 默认工具集，但 `run_command` 带上给定的程序清单。
    ///
    /// 组合根（`hbot-rpc` 的 Agent 装配点）用它把"允许跑什么"写成一次显式决定；
    /// 策略层不自带任何清单（`docs/adr/0003-command-isolation.md` §5：清单是调用方的
    /// 责任，工具实现不许内置一张含解释器的表）。清单为空等价于
    /// [`ToolRegistry::default_tools`]——一条命令都不许跑。
    pub fn default_tools_with_allowed_programs<I, S>(programs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut registry = Self::default_tools();
        registry.register(Arc::new(RunCommandTool::with_allowed_programs(programs)));
        registry
    }
}

/// 参数校验（网关第一步）。
///
/// 做三类**本地能确定性判定、且模型最容易搞错**的事：必填、类型、值域
/// （`enum` / `minimum` / `maximum`）。语义约束（路径是否越界、内容是否合法）
/// 留给工具自己，报错也更好懂。
///
/// 值域必须在这里查，不能在工具实现里各查一遍：网关只许有**一处**参数闸，
/// 多开一条路径就一定会有一条分支漏掉（红队 D 面 RT-02 就是"只查了必填与类型"）。
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
                // 值域校验与类型校验在同一个循环里：同一个字段、同一处闸门。
                check_value_constraints(tool_name, key, property, value)?;
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

/// 校验 `enum` / `minimum` / `maximum` 这三条值域约束。
///
/// - `enum`：字符串与数值都生效（按 JSON 值判等，不做隐式类型转换，
///   `"1"` 不等于 `1`）；
/// - `minimum` / `maximum`：只对数值生效（JSON Schema 语义）；声明了上下限
///   但收到非数值时交给 `type` 判定，这里不重复报错；
/// - `NaN` / `Infinity` 不算合法数值：即使将来数值表示放宽（如打开
///   `serde_json/arbitrary_precision`），带上下限的字段也不会让它们过去。
/// - 错误一律用 [`HbotError::invalid`]（-32002），文案带字段名与被拒的值
///   （有长度上限，别把整段正文抄进日志）。
fn check_value_constraints(
    tool_name: &str,
    key: &str,
    property: &Value,
    value: &Value,
) -> Result<(), HbotError> {
    if let Some(choices) = property.get("enum").and_then(Value::as_array)
        && !choices.iter().any(|choice| choice == value)
    {
        let allowed: Vec<String> = choices.iter().map(echo_rejected).collect();
        return Err(HbotError::invalid(format!(
            "工具 `{tool_name}` 的参数 `{key}` 必须是 {} 之一，收到 {}",
            truncate_chars(&allowed.join(" | "), MAX_ECHO_CHARS),
            echo_rejected(value)
        )));
    }

    let minimum = property.get("minimum").and_then(Value::as_f64);
    let maximum = property.get("maximum").and_then(Value::as_f64);
    if minimum.is_none() && maximum.is_none() {
        return Ok(());
    }
    if !value.is_number() {
        // 上下限只约束数值；类型不对已经由 `matches_type` 报过了。
        return Ok(());
    }
    let Some(number) = value.as_f64().filter(|number| number.is_finite()) else {
        return Err(HbotError::invalid(format!(
            "工具 `{tool_name}` 的参数 `{key}` 不是有限数值（NaN / Infinity 不算合法数值）"
        )));
    };
    if let Some(minimum) = minimum
        && number < minimum
    {
        return Err(HbotError::invalid(format!(
            "工具 `{tool_name}` 的参数 `{key}` 不得小于 {minimum}，收到 {}",
            echo_rejected(value)
        )));
    }
    if let Some(maximum) = maximum
        && number > maximum
    {
        return Err(HbotError::invalid(format!(
            "工具 `{tool_name}` 的参数 `{key}` 不得大于 {maximum}，收到 {}",
            echo_rejected(value)
        )));
    }
    Ok(())
}

/// 错误文案里回显一个值/枚举项的上限（字符数）。
///
/// 够看清"被拒的是什么"，又不至于把消息正文复制进日志与 UI。
const MAX_ECHO_CHARS: usize = 120;

/// 把被拒的值渲染成一行短文本（字符串带引号，其它类型用 JSON 表示）。
fn echo_rejected(value: &Value) -> String {
    let text = match value {
        Value::String(text) => format!("\"{text}\""),
        other => other.to_string(),
    };
    truncate_chars(&text, MAX_ECHO_CHARS)
}

/// 按字符截断并加省略号（错误文案用，不用于数据本身）。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
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
    fn validate_rejects_value_outside_enum() {
        let schema = json!({
            "type": "object",
            "properties": { "mode": { "type": "string", "enum": ["read", "write"] } },
            "required": ["mode"],
        });
        let error =
            validate_args("probe", &schema, &json!({ "mode": "delete" })).expect_err("enum");
        assert!(matches!(error, HbotError::Invalid { .. }));
        assert_eq!(error.code(), -32002);
        let text = error.to_string();
        assert!(text.contains("mode"), "{text}");
        assert!(text.contains("delete"), "错误文案要带被拒的值：{text}");
    }

    #[test]
    fn validate_accepts_enum_choice_for_string_and_number() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string", "enum": ["read", "write"] },
                "level": { "type": "integer", "enum": [1, 2] },
            },
        });
        validate_args("probe", &schema, &json!({ "mode": "read", "level": 2 })).expect("enum 命中");
    }

    #[test]
    fn validate_rejects_number_outside_bounds() {
        let schema = json!({
            "type": "object",
            "properties": { "count": { "type": "integer", "minimum": 1, "maximum": 3 } },
        });
        for value in [0, 99] {
            let error =
                validate_args("probe", &schema, &json!({ "count": value })).expect_err("越界数值");
            assert!(matches!(error, HbotError::Invalid { .. }));
            assert_eq!(error.code(), -32002);
            let text = error.to_string();
            assert!(text.contains("count"), "{text}");
            assert!(
                text.contains(&value.to_string()),
                "错误文案要带被拒的值：{text}"
            );
        }
    }

    #[test]
    fn validate_accepts_number_on_bounds() {
        let schema = json!({
            "properties": { "count": { "type": "integer", "minimum": 1, "maximum": 3 } },
        });
        validate_args("probe", &schema, &json!({ "count": 1 })).expect("下界含等于");
        validate_args("probe", &schema, &json!({ "count": 3 })).expect("上界含等于");
    }

    #[test]
    fn validate_bounds_only_apply_to_numbers() {
        // 上下限只约束数值：字符串该由 `type` 报，这里不该重复报越界。
        let schema = json!({
            "properties": { "count": { "minimum": 1 } },
        });
        validate_args("probe", &schema, &json!({ "count": "1" })).expect("非数值不受上下限约束");
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

    #[test]
    fn default_tools_include_run_command_and_the_allow_list_only_swaps_it() {
        let registry = ToolRegistry::default_tools();
        let tool = registry
            .get("run_command")
            .expect("run_command 必须声明存在");
        assert_eq!(tool.risk(), Risk::Command);
        assert_eq!(tool.min_sandbox(), SandboxMode::WorkspaceWrite);

        // 带清单的构造只替换 run_command，工具集合与顺序不变。
        let declared = ToolRegistry::default_tools_with_allowed_programs(["git"]);
        assert_eq!(declared.len(), registry.len());
        assert_eq!(declared.names(), registry.names());
        let swapped = declared.get("run_command").expect("run_command");
        assert_eq!(swapped.name(), "run_command");
        assert_eq!(swapped.risk(), Risk::Command);
    }
}
