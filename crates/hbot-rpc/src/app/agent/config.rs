//! Provider 配置解析：环境变量优先，其次 `<data_dir>/agent.json`。
//!
//! ## 为什么不用默认值兜底
//!
//! 没配模型时必须**明确报错**（`-32002`），不能悄悄退到一个假 provider：那会让 UI
//! 显示"已发送"而实际什么都没发生（`AGENTS.md` §0.8、`docs/00-assessment.md` P0-1）。
//! 缺配置的唯一出口是 [`MISSING_CONFIG_MESSAGE`]。
//!
//! ## 优先级
//!
//! **逐字段**：`HBOT_LLM_*` 给了就用环境变量，没给才读 `agent.json` 的同名字段。
//! 不是"整体二选一"——否则只改 `HBOT_LLM_MODEL` 调试时，会连文件里的 `baseUrl`
//! 一起丢掉，排查成本凭空翻倍。
//!
//! ## 密钥
//!
//! `apiKey` 只留在内存与 provider 请求头里：不打印、不进错误、不进日志。
//! 本类型的 `Debug` 一律显示 `<redacted>`。

use std::fmt;
use std::path::Path;

use hbot_agent::provider::LlmProvider;
use hbot_core::{HbotError, SandboxMode};

/// 基础 URL 的环境变量名。
pub const ENV_BASE_URL: &str = "HBOT_LLM_BASE_URL";
/// API key 的环境变量名。
pub const ENV_API_KEY: &str = "HBOT_LLM_API_KEY";
/// 模型名的环境变量名。
pub const ENV_MODEL: &str = "HBOT_LLM_MODEL";
/// 最大步数的环境变量名。
pub const ENV_MAX_STEPS: &str = "HBOT_LLM_MAX_STEPS";
/// 上游协议的环境变量名（`openai` / `anthropic`，缺省 `openai`）。
pub const ENV_PROVIDER: &str = "HBOT_LLM_PROVIDER";

/// `<data_dir>` 下的配置文件名。
pub const CONFIG_FILE: &str = "agent.json";

/// 缺配置时的**唯一**错误文案（UI 与测试都按这一句对齐）。
pub const MISSING_CONFIG_MESSAGE: &str = "未配置模型：设置 HBOT_LLM_* 或 agent.json";

/// 一轮对话可用的 provider 配置（已解析、已校验）。
#[derive(Clone)]
pub struct ProviderConfig {
    base_url: String,
    api_key: String,
    model: String,
    max_steps: u32,
    provider: LlmProvider,
}

impl ProviderConfig {
    /// 从进程环境变量 + `<data_dir>/agent.json` 解析。
    ///
    /// 文件读是同步 IO：调用方在 `async fn` 里应走 [`crate::app::AppState::blocking`]。
    pub fn resolve(data_dir: &Path) -> Result<Self, HbotError> {
        Self::resolve_with(data_dir, &|name| std::env::var(name).ok())
    }

    /// 解析的纯函数版本：环境变量通过 `get_env` 注入（测试不碰进程环境，避免并行串味）。
    pub fn resolve_with(
        data_dir: &Path,
        get_env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, HbotError> {
        let file = read_file_config(data_dir)?;

        let base_url = pick(get_env, ENV_BASE_URL, file.base_url);
        let api_key = pick(get_env, ENV_API_KEY, file.api_key).unwrap_or_default();
        let model = pick(get_env, ENV_MODEL, file.model);

        let (Some(base_url), Some(model)) = (base_url, model) else {
            return Err(HbotError::invalid(MISSING_CONFIG_MESSAGE));
        };

        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(HbotError::invalid(format!(
                "baseUrl 必须是 http(s) 地址，当前是 `{base_url}`"
            )));
        }
        if model.trim().is_empty() {
            return Err(HbotError::invalid(MISSING_CONFIG_MESSAGE));
        }

        let max_steps = match pick(
            get_env,
            ENV_MAX_STEPS,
            file.max_steps.map(|v| v.to_string()),
        ) {
            Some(raw) => raw.trim().parse::<u32>().map_err(|_| {
                HbotError::invalid(format!("maxSteps 必须是正整数，当前是 `{raw}`"))
            })?,
            None => hbot_agent::DEFAULT_MAX_STEPS,
        };
        if max_steps == 0 {
            return Err(HbotError::invalid("maxSteps 必须大于 0"));
        }

        // provider 缺省 openai（向后兼容旧配置）；非法值必须报错，不许静默退回。
        let provider = match pick(get_env, ENV_PROVIDER, file.provider) {
            Some(raw) => LlmProvider::from_name(&raw).map_err(|raw| {
                HbotError::invalid(format!(
                    "provider 只能是 {}，当前是 `{raw}`",
                    LlmProvider::allowed_names()
                ))
            })?,
            None => LlmProvider::default(),
        };

        Ok(Self {
            base_url,
            api_key,
            model: model.trim().to_string(),
            max_steps,
            provider,
        })
    }

    /// 上游协议实现（[`LlmProvider::OpenAi`] / [`LlmProvider::Anthropic`]）。
    pub fn provider(&self) -> LlmProvider {
        self.provider
    }

    /// 基础 URL（已去掉尾部 `/`，不含 `/chat/completions`）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// API key；空串表示"不带 Authorization 头"（本地推理常见）。
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// 模型名。
    pub fn model(&self) -> &str {
        &self.model
    }

    /// 单轮最大步数。
    pub fn max_steps(&self) -> u32 {
        self.max_steps
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 密钥永不出现在任何可打印形态里。
        let key = if self.api_key.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        };
        f.debug_struct("ProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &key)
            .field("model", &self.model)
            .field("max_steps", &self.max_steps)
            .field("provider", &self.provider.as_str())
            .finish()
    }
}

impl ProviderConfig {
    /// 仅供测试与装配方便：不改环境变量构造一份配置（provider 缺省 `openai`）。
    #[cfg(test)]
    pub fn for_test(base_url: &str, api_key: &str, model: &str, max_steps: u32) -> Self {
        Self::for_test_with_provider(base_url, api_key, model, max_steps, LlmProvider::default())
    }

    /// 仅供测试与装配方便：显式指定 provider 的版本。
    #[cfg(test)]
    pub fn for_test_with_provider(
        base_url: &str,
        api_key: &str,
        model: &str,
        max_steps: u32,
        provider: LlmProvider,
    ) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_steps,
            provider,
        }
    }
}

/// 沙箱档的环境变量名。
pub const ENV_SANDBOX: &str = "HBOT_SANDBOX";
/// 命令白名单的环境变量名（逗号分隔的程序名）。
pub const ENV_ALLOWED_PROGRAMS: &str = "HBOT_COMMAND_ALLOW";

/// 一轮对话的沙箱与命令策略。
///
/// 默认 `workspace_write`（`docs/02-architecture.md` §11.3）。注意：该档当前
/// **没有写隔离**（`docs/adr/0003-command-isolation.md`），所以命令工具在默认档
/// 下会被 `IsolationUnavailable` 拒绝 —— 这是刻意的 fail-closed，不是 bug。
/// 要真跑命令必须显式把档位切到 `danger` 并给出白名单程序。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxConfig {
    mode: SandboxMode,
    allowed_programs: Vec<String>,
}

impl SandboxConfig {
    /// 从进程环境变量 + `<data_dir>/agent.json` 解析。
    pub fn resolve(data_dir: &Path) -> Result<Self, HbotError> {
        Self::resolve_with(data_dir, &|name| std::env::var(name).ok())
    }

    /// 纯函数版本（测试注入环境变量，不碰进程环境）。
    pub fn resolve_with(
        data_dir: &Path,
        get_env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, HbotError> {
        let file = read_file_config(data_dir)?;

        let mode = match pick(get_env, ENV_SANDBOX, file.sandbox) {
            Some(raw) => parse_sandbox_mode(&raw)?,
            None => SandboxMode::default(),
        };

        let allowed_programs = match pick(
            get_env,
            ENV_ALLOWED_PROGRAMS,
            file.allowed_programs.map(|list| list.join(",")),
        ) {
            Some(raw) => split_programs(&raw),
            None => Vec::new(),
        };

        Ok(Self {
            mode,
            allowed_programs,
        })
    }

    /// 沙箱档。
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// 允许执行的程序（空 = 一个都不许，fail-closed）。
    pub fn allowed_programs(&self) -> &[String] {
        &self.allowed_programs
    }
}

/// 解析档位字符串：接受 `as_str()` 的写法以及 `-` / 大小写变体。
fn parse_sandbox_mode(raw: &str) -> Result<SandboxMode, HbotError> {
    let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "read_only" | "readonly" => Ok(SandboxMode::ReadOnly),
        "workspace_write" | "workspacewrite" => Ok(SandboxMode::WorkspaceWrite),
        "danger" | "danger_full_access" => Ok(SandboxMode::Danger),
        other => Err(HbotError::invalid(format!(
            "sandbox 只能是 read_only / workspace_write / danger，当前是 `{other}`"
        ))),
    }
}

/// 逗号（或空白）分隔的程序清单，去空、去重、保持输入顺序。
fn split_programs(raw: &str) -> Vec<String> {
    let mut programs: Vec<String> = Vec::new();
    for item in raw.split([',', ' ', '\t', '\n']) {
        let name = item.trim();
        if name.is_empty() {
            continue;
        }
        if !programs.iter().any(|existing| existing == name) {
            programs.push(name.to_string());
        }
    }
    programs
}

/// 逐字段取值：环境变量（非空）优先，否则用文件里的值。
fn pick(
    get_env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    from_file: Option<String>,
) -> Option<String> {
    match get_env(name) {
        Some(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => from_file.and_then(|value| {
            let trimmed = value.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }),
    }
}

/// `agent.json` 的已知字段（camelCase，与 [`FileConfig`] 的 serde 名一致）。
///
/// 只用来给"你是不是想写 XXX"提建议，**不参与解析**。`agent.json` 是平的（没有嵌套
/// 对象），所以任何层级的未知键都用这一张表。
const AGENT_FILE_FIELDS: &[&str] = &[
    "baseUrl",
    "apiKey",
    "model",
    "maxSteps",
    "sandbox",
    "allowedPrograms",
    "provider",
];

/// 建议用的字段表：`agent.json` 只有一层，忽略路径。
fn agent_known_fields(_rendered_path: &str) -> &'static [&'static str] {
    AGENT_FILE_FIELDS
}

/// `agent.json` 的字段（全部可选：只配一半也算配置，缺的那半再报缺失）。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct FileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    max_steps: Option<u32>,
    sandbox: Option<String>,
    allowed_programs: Option<Vec<String>>,
    /// 上游协议：`openai` / `anthropic`；缺省 `openai`。
    provider: Option<String>,
}

/// 读 `<data_dir>/agent.json`；文件不存在等同于"没配"。
fn read_file_config(data_dir: &Path) -> Result<FileConfig, HbotError> {
    let path = data_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileConfig::default());
        }
        Err(error) => {
            return Err(HbotError::invalid(format!(
                "读取 `{}` 失败：{error}",
                path.display()
            )));
        }
    };
    if text.trim().is_empty() {
        return Ok(FileConfig::default());
    }
    // 未知字段只告警、不失败：手写的 agent.json 里 `_说明` 这类备注键必须能过，
    // `baseURL` 这类拼写错误则要看得见。机制与判据见 `runtime::config::deserialize_with_warnings`。
    crate::app::runtime::config::deserialize_with_warnings(&path, &text, agent_known_fields)
        .map_err(|error| HbotError::invalid(format!("`{}` 不是合法 JSON：{error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::TempDir;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn empty_env() -> impl Fn(&str) -> Option<String> {
        |_name| None
    }

    #[test]
    fn missing_config_is_invalid_with_the_documented_message() {
        let dir = TempDir::new("agent-config-missing");
        let error =
            ProviderConfig::resolve_with(dir.path(), &empty_env()).expect_err("没有配置必须报错");
        assert_eq!(error.code(), -32002);
        assert_eq!(
            error.to_string(),
            format!("invalid: {MISSING_CONFIG_MESSAGE}")
        );
    }

    #[test]
    fn environment_only_config_resolves() {
        let dir = TempDir::new("agent-config-env");
        let env = env_of(&[
            (ENV_BASE_URL, "https://api.example.com/v1/"),
            (ENV_API_KEY, "sk-test-1234567890"),
            (ENV_MODEL, "gpt-test"),
            (ENV_MAX_STEPS, "7"),
        ]);
        let config = ProviderConfig::resolve_with(dir.path(), &env).expect("env 配置可用");
        assert_eq!(config.base_url(), "https://api.example.com/v1");
        assert_eq!(config.model(), "gpt-test");
        assert_eq!(config.max_steps(), 7);
    }

    #[test]
    fn file_config_is_used_when_environment_is_empty() {
        let dir = TempDir::new("agent-config-file");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"baseUrl":"http://127.0.0.1:8080/v1","apiKey":"sk-file-1234567890","model":"local-model","maxSteps":3}"#,
        )
        .expect("写配置");
        let config = ProviderConfig::resolve_with(dir.path(), &empty_env()).expect("文件配置可用");
        assert_eq!(config.base_url(), "http://127.0.0.1:8080/v1");
        assert_eq!(config.model(), "local-model");
        assert_eq!(config.max_steps(), 3);
        assert_eq!(config.api_key(), "sk-file-1234567890");
    }

    #[test]
    fn environment_overrides_only_what_it_sets() {
        let dir = TempDir::new("agent-config-merge");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"baseUrl":"http://127.0.0.1:8080/v1","model":"file-model"}"#,
        )
        .expect("写配置");
        let env = env_of(&[(ENV_MODEL, "env-model")]);
        let config = ProviderConfig::resolve_with(dir.path(), &env).expect("合并配置可用");
        assert_eq!(config.model(), "env-model");
        // 文件里的 baseUrl 没被环境变量挤掉。
        assert_eq!(config.base_url(), "http://127.0.0.1:8080/v1");
        assert_eq!(config.max_steps(), hbot_agent::DEFAULT_MAX_STEPS);
        assert!(config.api_key().is_empty());
    }

    #[test]
    fn non_http_base_url_is_rejected() {
        let dir = TempDir::new("agent-config-scheme");
        let env = env_of(&[(ENV_BASE_URL, "file:///etc/passwd"), (ENV_MODEL, "m")]);
        let error = ProviderConfig::resolve_with(dir.path(), &env).expect_err("非法 scheme");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("http(s)"));
    }

    #[test]
    fn malformed_max_steps_is_rejected_not_defaulted() {
        let dir = TempDir::new("agent-config-steps");
        let env = env_of(&[
            (ENV_BASE_URL, "https://api.example.com/v1"),
            (ENV_MODEL, "m"),
            (ENV_MAX_STEPS, "many"),
        ]);
        let error = ProviderConfig::resolve_with(dir.path(), &env).expect_err("非法步数");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("maxSteps"));
    }

    #[test]
    fn malformed_json_is_reported_with_the_path() {
        let dir = TempDir::new("agent-config-json");
        std::fs::write(dir.path().join(CONFIG_FILE), "{not json").expect("写配置");
        let error = ProviderConfig::resolve_with(dir.path(), &empty_env()).expect_err("非法 JSON");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("agent.json"));
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let config =
            ProviderConfig::for_test("https://api.example.com", "sk-secret-abcdef", "m", 5);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-secret-abcdef"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn provider_defaults_to_openai_when_unset() {
        // 向后兼容：老配置没有任何 provider 字段，必须还是走 OpenAI 那条。
        let dir = TempDir::new("agent-config-provider-default");
        let env = env_of(&[
            (ENV_BASE_URL, "https://api.example.com/v1"),
            (ENV_MODEL, "m"),
        ]);
        let config = ProviderConfig::resolve_with(dir.path(), &env).expect("默认配置");
        assert_eq!(config.provider(), LlmProvider::OpenAi);
    }

    #[test]
    fn provider_reads_environment_and_file() {
        let dir = TempDir::new("agent-config-provider-env");
        let env = env_of(&[
            (ENV_BASE_URL, "https://z1rry.cyou"),
            (ENV_MODEL, "deepseek-v4.1-flash-1017"),
            (ENV_PROVIDER, "anthropic"),
        ]);
        let config = ProviderConfig::resolve_with(dir.path(), &env).expect("env 指定 provider");
        assert_eq!(config.provider(), LlmProvider::Anthropic);

        // 文件里的 provider 在环境变量没给时生效。
        let dir = TempDir::new("agent-config-provider-file");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"baseUrl":"https://z1rry.cyou","model":"m","provider":"anthropic"}"#,
        )
        .expect("写配置");
        let config = ProviderConfig::resolve_with(dir.path(), &empty_env()).expect("文件 provider");
        assert_eq!(config.provider(), LlmProvider::Anthropic);
    }

    #[test]
    fn provider_environment_overrides_file() {
        let dir = TempDir::new("agent-config-provider-merge");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"baseUrl":"https://z1rry.cyou","model":"m","provider":"anthropic"}"#,
        )
        .expect("写配置");
        let env = env_of(&[(ENV_PROVIDER, "openai")]);
        let config = ProviderConfig::resolve_with(dir.path(), &env).expect("env 覆盖文件");
        assert_eq!(config.provider(), LlmProvider::OpenAi);
    }

    #[test]
    fn invalid_provider_is_rejected_with_allowed_values() {
        let dir = TempDir::new("agent-config-provider-bad");
        let env = env_of(&[
            (ENV_BASE_URL, "https://api.example.com"),
            (ENV_MODEL, "m"),
            (ENV_PROVIDER, "gemini"),
        ]);
        let error = ProviderConfig::resolve_with(dir.path(), &env).expect_err("非法 provider");
        assert_eq!(error.code(), -32002, "非法取值必须是 -32002");
        let message = error.to_string();
        assert!(
            message.contains("provider"),
            "错误要说清是 provider：{message}"
        );
        assert!(message.contains("gemini"), "要回显收到的值：{message}");
        // 必须列出允许值，否则用户不知道该填什么。
        assert!(message.contains("openai"), "要列出 openai：{message}");
        assert!(message.contains("anthropic"), "要列出 anthropic：{message}");
    }

    #[test]
    fn debug_includes_the_provider_but_not_the_key() {
        let config = ProviderConfig::for_test_with_provider(
            "https://z1rry.cyou",
            "sk-secret-abcdef",
            "m",
            5,
            LlmProvider::Anthropic,
        );
        let rendered = format!("{config:?}");
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(!rendered.contains("sk-secret-abcdef"));
    }

    #[test]
    fn sandbox_defaults_to_workspace_write_without_programs() {
        let dir = TempDir::new("sandbox-config-default");
        let config = SandboxConfig::resolve_with(dir.path(), &empty_env()).expect("默认档");
        assert_eq!(config.mode(), SandboxMode::WorkspaceWrite);
        assert!(
            config.allowed_programs().is_empty(),
            "默认一个程序都不许：fail-closed"
        );
    }

    #[test]
    fn sandbox_reads_env_and_normalises_spellings() {
        let dir = TempDir::new("sandbox-config-env");
        for (raw, expected) in [
            ("read_only", SandboxMode::ReadOnly),
            ("readonly", SandboxMode::ReadOnly),
            ("workspace-write", SandboxMode::WorkspaceWrite),
            ("DANGER", SandboxMode::Danger),
        ] {
            let pairs = [(ENV_SANDBOX, raw)];
            let env = env_of(&pairs);
            let config = SandboxConfig::resolve_with(dir.path(), &env).expect("合法档位");
            assert_eq!(config.mode(), expected, "`{raw}` 应解析成 {expected:?}");
        }
    }

    #[test]
    fn sandbox_rejects_unknown_mode_instead_of_defaulting() {
        let dir = TempDir::new("sandbox-config-bad");
        let env = env_of(&[(ENV_SANDBOX, "yolo")]);
        let error = SandboxConfig::resolve_with(dir.path(), &env).expect_err("非法档位");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("sandbox"));
    }

    #[test]
    fn allowed_programs_are_split_deduped_and_kept_in_order() {
        let dir = TempDir::new("sandbox-config-programs");
        let env = env_of(&[
            (ENV_SANDBOX, "danger"),
            (ENV_ALLOWED_PROGRAMS, " git, cargo ,,git\nnode "),
        ]);
        let config = SandboxConfig::resolve_with(dir.path(), &env).expect("白名单");
        assert_eq!(config.allowed_programs(), ["git", "cargo", "node"]);
    }

    #[test]
    fn sandbox_reads_agent_json_when_env_is_absent() {
        let dir = TempDir::new("sandbox-config-file");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"sandbox":"danger","allowedPrograms":["cargo"]}"#,
        )
        .expect("写配置");
        let config = SandboxConfig::resolve_with(dir.path(), &empty_env()).expect("文件配置");
        assert_eq!(config.mode(), SandboxMode::Danger);
        assert_eq!(config.allowed_programs(), ["cargo"]);
    }
}
