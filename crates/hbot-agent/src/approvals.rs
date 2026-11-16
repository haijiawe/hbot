//! 审批：规则键、策略与策略存储。
//!
//! 硬约束（`AGENTS.md` §0.7、§7 第 5 条）：审批规则键必须是**完整动作**
//! `(tool_name, 规范化参数, sandbox_mode)`；**禁止**按工具类别、风险类别或路径前缀
//! 整类放行——那样会出现旧项目的 bug：用户对 `a.txt` 点"别再问"，同一轮里
//! 任意路径都被放行。
//!
//! 因此：
//!
//! - 只有 [`ApprovalPolicy::AlwaysAllowThisExactKey`] 携带键，且键是完整的；
//! - [`ApprovalStore`] 的键就是 [`ApprovalKey`]，不做任何前缀/类别合并；
//! - 参数先规范化（对象键排序、去空白），路径在网关里先 realpath 成项目内相对路径，
//!   所以 `a.txt` 与 `./a.txt` 是同一个键，而 `a.txt` 与 `b.txt` **不是**。

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use hbot_core::{ApprovalDecision, HbotError, SandboxMode};
use serde_json::Value;

use crate::tools::Risk;

/// 规范化参数：对象键递归排序、紧凑序列化。
///
/// 用显式排序而不是依赖 `serde_json::Map` 的底层容器，这样即使某天打开
/// `preserve_order` 特性，审批键仍然稳定。
pub fn canonical_json(value: &Value) -> String {
    let sorted = sort_value(value);
    match serde_json::to_string(&sorted) {
        Ok(text) => text,
        // `Value` 一定能序列化；这里保留显式分支而不是 `expect`，库代码不允许 panic。
        Err(err) => format!("<unserializable:{err}>"),
    }
}

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::with_capacity(map.len());
            for key in keys {
                if let Some(item) = map.get(key) {
                    sorted.insert(key.clone(), sort_value(item));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// 把字符串截到字符边界（用于提示文本，不用于数据本身）。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// 参数摘要的字符上限（键很多时别把描述撑成一大段）。
const MAX_DESCRIBE_ARGS_CHARS: usize = 300;

/// 参数摘要：键名 + 值的**类型/长度**，永不出现值本身。
///
/// 拒绝文案与日志都会用到它，而参数里可能是消息正文甚至密钥：所以这里只回答
/// 「哪个参数、值有多长」，不回答「值是什么」。反序列化失败（`from_canonical`
/// 的调用方给了非 JSON）时整体隐藏——连键名都不可信，不能拿它拼进日志。
fn summarize_args(canonical_args: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(canonical_args) else {
        return "<参数不可解析，已隐藏>".to_string();
    };
    let Value::Object(map) = value else {
        return summarize_value(&value);
    };
    if map.is_empty() {
        return "无参数".to_string();
    }
    let mut parts: Vec<String> = Vec::with_capacity(map.len());
    for (key, value) in &map {
        let summary = if is_secret_key(key) {
            "<redacted>".to_string()
        } else {
            summarize_value(value)
        };
        parts.push(format!("{key}: {summary}"));
    }
    truncate_chars(&parts.join(", "), MAX_DESCRIBE_ARGS_CHARS)
}

/// 一个值只描述类型与规模（长度 / 项数 / 键数），不含内容。
fn summarize_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(number) => format!("number({} 位)", number.to_string().chars().count()),
        Value::String(text) => format!("string({} 字符)", text.chars().count()),
        Value::Array(items) => format!("array({} 项)", items.len()),
        Value::Object(map) => format!("object({} 键)", map.len()),
    }
}

/// 键名是否属于密钥类（值一律打码）。
///
/// 按词边界判断而不是子串包含：`api_key` / `accessToken` / `X-Api-Key` 都命中，
/// `monkey` / `keyboard` 不命中（否则会把无关参数也打码，摘要就没用了）。
/// 不分隔的连写（`apikey`）词边界切不出来，另列一小张表。
fn is_secret_key(name: &str) -> bool {
    const SECRET_WORDS: [&str; 7] = [
        "token",
        "secret",
        "key",
        "password",
        "passwd",
        "authorization",
        "credential",
    ];
    const SECRET_COMPOUNDS: [&str; 10] = [
        "apikey",
        "accesskey",
        "privatekey",
        "publickey",
        "secretkey",
        "signingkey",
        "clientsecret",
        "accesstoken",
        "refreshtoken",
        "bearertoken",
    ];

    fn words_of(name: &str) -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        for part in name.split(|character: char| !character.is_alphanumeric()) {
            if part.is_empty() {
                continue;
            }
            let mut word = String::new();
            let mut previous_is_lower_or_digit = false;
            for character in part.chars() {
                if character.is_uppercase() && previous_is_lower_or_digit {
                    words.push(word.to_lowercase());
                    word = String::new();
                }
                previous_is_lower_or_digit = character.is_lowercase() || character.is_ascii_digit();
                word.push(character);
            }
            words.push(word.to_lowercase());
        }
        words
    }

    if words_of(name)
        .iter()
        .any(|word| SECRET_WORDS.contains(&word.as_str()))
    {
        return true;
    }
    let compact: String = name
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect();
    SECRET_COMPOUNDS.contains(&compact.to_lowercase().as_str())
}

/// 审批规则键：工具名 + 规范化参数 + 沙箱档。
///
/// 三个部分全部参与比较；任意一项不同就是不同的键。
///
/// `Debug` **手写**而不是 derive：派生实现会把 `canonical_args` 原样打出来，
/// 而键的 Debug 会经 `tracing` 的 `?error`、`error_chain` 的 `{item:?}` 进入
/// 日志/UI（红队 F 面 RT-03）。这里只打脱敏摘要。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApprovalKey {
    tool: String,
    canonical_args: String,
    sandbox: &'static str,
}

impl std::fmt::Debug for ApprovalKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalKey")
            .field("tool", &self.tool)
            .field("args", &summarize_args(&self.canonical_args))
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

impl ApprovalKey {
    /// 由工具名、原始参数与沙箱档构造（内部做参数规范化）。
    pub fn new(tool: &str, args: &Value, sandbox: SandboxMode) -> Self {
        Self {
            tool: tool.to_string(),
            canonical_args: canonical_json(args),
            sandbox: sandbox.as_str(),
        }
    }

    /// 由已规范化的参数串构造（反序列化 / 跨进程恢复用）。
    ///
    /// 调用方必须保证 `canonical_args` 已规范化，否则同一个动作会得到两个键。
    pub fn from_canonical(tool: &str, canonical_args: &str, sandbox: SandboxMode) -> Self {
        Self {
            tool: tool.to_string(),
            canonical_args: canonical_args.to_string(),
            sandbox: sandbox.as_str(),
        }
    }

    /// 工具名。
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// 规范化后的参数 JSON。
    pub fn canonical_args(&self) -> &str {
        &self.canonical_args
    }

    /// 沙箱档的稳定字符串（`read_only` / `workspace_write` / `danger`）。
    pub fn sandbox(&self) -> &'static str {
        self.sandbox
    }

    /// 给用户看的单行摘要（不用于比较）。
    ///
    /// **绝不回显参数值原文**：这段文案会进拒绝消息、审计与 UI，参数里可能是
    /// 消息正文甚至密钥。只保留「哪个工具、哪些参数、值有多长」，密钥类键名
    /// （`token` / `secret` / `key` / `password`）的值一律 `<redacted>`。
    pub fn describe(&self) -> String {
        format!(
            "{} {} [{}]",
            self.tool,
            summarize_args(&self.canonical_args),
            self.sandbox
        )
    }
}

/// 一次审批的策略。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// 每次都问用户（默认）。
    Ask,
    /// 只对**这一个完整键**放行；参数或沙箱档变了就重新问。
    AlwaysAllowThisExactKey(ApprovalKey),
    /// 只对**这一个完整键**拒绝。
    Deny(ApprovalKey),
}

impl ApprovalPolicy {
    /// 是否需要弹审批。
    pub fn requires_prompt(&self) -> bool {
        matches!(self, Self::Ask)
    }
}

/// 审批请求：交给 UI / 用户的完整动作描述。
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// 完整规则键。
    pub key: ApprovalKey,
    /// 工具名（冗余字段，便于 UI 分组）。
    pub tool: String,
    /// 原始参数（UI 展示 diff / 命令用）。
    pub args: Value,
    /// 风险类别（仅展示，不作为放行依据）。
    pub risk: Risk,
    /// 当前沙箱档。
    pub sandbox: SandboxMode,
}

/// 用户对一次审批请求的答复。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalReply {
    /// 批准还是拒绝。
    pub decision: ApprovalDecision,
    /// 是否记住这个**完整键**（下次同键不再问）。
    pub remember: bool,
}

impl ApprovalReply {
    /// 只批准这一次。
    pub fn approve_once() -> Self {
        Self {
            decision: ApprovalDecision::Approve,
            remember: false,
        }
    }

    /// 批准并记住这个完整键。
    pub fn approve_always() -> Self {
        Self {
            decision: ApprovalDecision::Approve,
            remember: true,
        }
    }

    /// 拒绝（不记住）。
    pub fn deny() -> Self {
        Self {
            decision: ApprovalDecision::Deny,
            remember: false,
        }
    }

    /// 是否批准。
    pub fn is_approved(&self) -> bool {
        self.decision == ApprovalDecision::Approve
    }
}

/// 审批询问出口；桌面端接到 UI，测试用记录器。
#[async_trait]
pub trait Approver: Send + Sync {
    /// 询问用户；实现方不得自行放行。
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalReply, HbotError>;
}

/// 总是拒绝的审批出口（无 UI 时的安全默认）。
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllApprover;

#[async_trait]
impl Approver for DenyAllApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        Ok(ApprovalReply::deny())
    }
}

/// 审批策略存储：键是完整动作，值是策略。
///
/// 内部用 `Mutex`（临界区只有一次 map 操作，不跨 `await`）。
#[derive(Debug, Default)]
pub struct ApprovalStore {
    rules: Mutex<BTreeMap<ApprovalKey, ApprovalPolicy>>,
}

impl ApprovalStore {
    /// 空存储：所有动作默认 [`ApprovalPolicy::Ask`]。
    pub fn new() -> Self {
        Self::default()
    }

    fn rules(&self) -> std::sync::MutexGuard<'_, BTreeMap<ApprovalKey, ApprovalPolicy>> {
        // 中毒说明别的线程 panic 过；这里的 map 是不变式简单的数据，直接取回继续用。
        match self.rules.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 查询某个完整键的策略；未登记时是 [`ApprovalPolicy::Ask`]。
    pub fn policy_for(&self, key: &ApprovalKey) -> ApprovalPolicy {
        self.rules()
            .get(key)
            .cloned()
            .unwrap_or(ApprovalPolicy::Ask)
    }

    /// 记住"只对这个完整键放行"。
    pub fn allow_exactly(&self, key: ApprovalKey) {
        self.rules()
            .insert(key.clone(), ApprovalPolicy::AlwaysAllowThisExactKey(key));
    }

    /// 记住"只对这个完整键拒绝"。
    pub fn deny_exactly(&self, key: ApprovalKey) {
        self.rules().insert(key.clone(), ApprovalPolicy::Deny(key));
    }

    /// 撤销某个完整键的规则；返回是否真的删掉了。
    pub fn revoke(&self, key: &ApprovalKey) -> bool {
        self.rules().remove(key).is_some()
    }

    /// 清空全部规则（切会话 / 用户要求重置时用）。
    pub fn clear(&self) {
        self.rules().clear();
    }

    /// 已登记的规则数。
    pub fn len(&self) -> usize {
        self.rules().len()
    }

    /// 是否没有任何规则。
    pub fn is_empty(&self) -> bool {
        self.rules().is_empty()
    }

    /// 已登记的完整键（调试 / 审计用）。
    pub fn keys(&self) -> Vec<ApprovalKey> {
        self.rules().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_args(path: &str) -> Value {
        json!({ "path": path, "content": "hello" })
    }

    #[test]
    fn default_policy_is_ask() {
        let store = ApprovalStore::new();
        let key = ApprovalKey::new(
            "write_file",
            &write_args("a.txt"),
            SandboxMode::WorkspaceWrite,
        );
        assert_eq!(store.policy_for(&key), ApprovalPolicy::Ask);
        assert!(store.is_empty());
    }

    #[test]
    fn approval_does_not_share_across_paths() {
        let store = ApprovalStore::new();
        let approved = ApprovalKey::new(
            "write_file",
            &write_args("a.txt"),
            SandboxMode::WorkspaceWrite,
        );
        store.allow_exactly(approved.clone());

        assert!(matches!(
            store.policy_for(&approved),
            ApprovalPolicy::AlwaysAllowThisExactKey(_)
        ));

        let other_path = ApprovalKey::new(
            "write_file",
            &write_args("b.txt"),
            SandboxMode::WorkspaceWrite,
        );
        assert_eq!(
            store.policy_for(&other_path),
            ApprovalPolicy::Ask,
            "同工具不同路径不得共享已批准"
        );

        let other_content = ApprovalKey::new(
            "write_file",
            &json!({ "path": "a.txt", "content": "different" }),
            SandboxMode::WorkspaceWrite,
        );
        assert_eq!(
            store.policy_for(&other_content),
            ApprovalPolicy::Ask,
            "同路径不同内容不得共享已批准"
        );

        let other_sandbox =
            ApprovalKey::new("write_file", &write_args("a.txt"), SandboxMode::Danger);
        assert_eq!(
            store.policy_for(&other_sandbox),
            ApprovalPolicy::Ask,
            "不同沙箱档不得共享已批准"
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn deny_is_exact_key_only() {
        let store = ApprovalStore::new();
        let denied = ApprovalKey::new(
            "run_command",
            &json!({ "command": "rm -rf /" }),
            SandboxMode::Danger,
        );
        store.deny_exactly(denied.clone());
        assert!(matches!(store.policy_for(&denied), ApprovalPolicy::Deny(_)));
        let allowed = ApprovalKey::new(
            "run_command",
            &json!({ "command": "echo hi" }),
            SandboxMode::Danger,
        );
        assert_eq!(store.policy_for(&allowed), ApprovalPolicy::Ask);
    }

    #[test]
    fn canonical_json_sorts_nested_keys() {
        let a = json!({ "path": "a.txt", "content": "x", "nested": { "b": 1, "a": 2 } });
        let b = json!({ "nested": { "a": 2, "b": 1 }, "content": "x", "path": "a.txt" });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert!(canonical_json(&a).starts_with("{\"content\":\"x\",\"nested\":{\"a\":2,\"b\":1}"));
    }

    #[test]
    fn revoke_and_clear() {
        let store = ApprovalStore::new();
        let key = ApprovalKey::new(
            "write_file",
            &write_args("a.txt"),
            SandboxMode::WorkspaceWrite,
        );
        store.allow_exactly(key.clone());
        assert!(store.revoke(&key));
        assert_eq!(store.policy_for(&key), ApprovalPolicy::Ask);
        assert!(!store.revoke(&key));

        store.allow_exactly(key);
        store.clear();
        assert!(store.is_empty());
    }

    #[test]
    fn describe_truncates_long_args() {
        let long = "x".repeat(1000);
        let key = ApprovalKey::new(
            "write_file",
            &json!({ "path": "a.txt", "content": long }),
            SandboxMode::WorkspaceWrite,
        );
        let text = key.describe();
        assert!(text.contains("write_file"));
        assert!(text.contains("workspace_write"));
        assert!(text.chars().count() < 400);
    }

    #[test]
    fn describe_never_echoes_argument_values() {
        let secret = "sk-test-DEADBEEF";
        let key = ApprovalKey::new(
            "write_file",
            &json!({ "path": "notes.txt", "content": format!("API_KEY={secret}") }),
            SandboxMode::WorkspaceWrite,
        );
        let text = key.describe();
        assert!(text.contains("write_file"));
        assert!(text.contains("workspace_write"));
        assert!(!text.contains(secret), "描述里出现了参数值：{text}");
        assert!(!text.contains("notes.txt"), "路径值也是参数原文：{text}");
        assert!(text.contains("path"), "要能看出是哪个参数：{text}");
        assert!(text.contains("content"), "要能看出是哪个参数：{text}");
        assert!(text.contains("string("), "要能看出值的类型与长度：{text}");
    }

    #[test]
    fn describe_redacts_secret_key_names() {
        let key = ApprovalKey::new(
            "http_fetch",
            &json!({
                "api_key": "sk-live-123",
                "accessToken": "tok-456",
                "password": "hunter2",
                "authorization": "Bearer abc",
                "query": "safe",
            }),
            SandboxMode::Danger,
        );
        let text = key.describe();
        for secret in ["sk-live-123", "tok-456", "hunter2", "Bearer abc"] {
            assert!(!text.contains(secret), "密钥值泄漏：{text}");
        }
        assert_eq!(text.matches("<redacted>").count(), 4, "{text}");
        assert!(text.contains("api_key"), "键名要保留便于排查：{text}");
    }

    #[test]
    fn describe_does_not_redact_innocent_names() {
        let key = ApprovalKey::new(
            "probe",
            &json!({ "monkey": "x", "keyboard": "y" }),
            SandboxMode::ReadOnly,
        );
        let text = key.describe();
        assert!(!text.contains("<redacted>"), "{text}");
        assert!(text.contains("monkey"), "{text}");
    }

    #[test]
    fn debug_never_echoes_argument_values() {
        let secret = "sk-test-DEADBEEF";
        let key = ApprovalKey::new(
            "write_file",
            &json!({ "path": "notes.txt", "content": format!("API_KEY={secret}") }),
            SandboxMode::WorkspaceWrite,
        );
        let text = format!("{key:?}");
        assert!(!text.contains(secret), "Debug 里出现了参数值：{text}");
        assert!(!text.contains("notes.txt"), "Debug 里出现了路径值：{text}");
        assert!(text.contains("write_file"), "{text}");
        assert!(text.contains("ApprovalKey"), "{text}");
    }

    #[test]
    fn describe_hides_unparseable_canonical_args() {
        let key = ApprovalKey::from_canonical(
            "probe",
            "not-json-but-maybe-a-secret",
            SandboxMode::ReadOnly,
        );
        let text = key.describe();
        assert!(!text.contains("not-json-but-maybe-a-secret"), "{text}");
        assert!(text.contains("probe"), "{text}");
        assert!(text.contains("已隐藏"), "{text}");
    }

    #[test]
    fn secret_key_names_cover_underscore_and_camel_case() {
        for name in [
            "api_key",
            "apiKey",
            "API_KEY",
            "access_token",
            "client_secret",
            "password",
            "X-Api-Key",
            "apikey",
        ] {
            assert!(is_secret_key(name), "`{name}` 应判为密钥类");
        }
        for name in [
            "monkey",
            "keyboard",
            "path",
            "content",
            "query",
            "tokenizer",
        ] {
            assert!(!is_secret_key(name), "`{name}` 不该判为密钥类");
        }
    }
}
