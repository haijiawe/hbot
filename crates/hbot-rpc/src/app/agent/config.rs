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

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

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

/// 解释器/外壳类程序的**逃生阀**环境变量名（RT-05）。
///
/// 默认（不设，或设成 `0`/`false`/`no`/任何别的值）时，白名单里出现
/// [`INTERPRETER_PROGRAMS`] 里的名字会让解析直接失败（`-32002`）；只有显式设成
/// `1`/`true`/`yes`/`on`（大小写不敏感）才放行，且放行时**一定**打一条
/// `tracing::warn!`（每进程每条清单一次）。
///
/// 为什么给逃生阀而不是纯拒绝：`hbot-command` 的白名单是"能跑什么"的**唯一**判据，
/// 而"要不要跑 `python`/`node`"是产品决策，不该由配置文件单方面决定（也不该逼用户改代码
/// 重编译）。逃生阀的价值在于它是**显式**的：环境变量是一眼可见的启动参数，日志里还有一条
/// warn，比"清单里悄悄多一行 cmd"可审计得多。
pub const ENV_ALLOW_SHELL: &str = "HBOT_ALLOW_SHELL";

/// 命令白名单里**不许出现**的解释器/外壳类程序（RT-05）。
///
/// 判据（见 [`normalize_program_name`]）：取 basename → 去扩展名 → 全部小写，再与这张表
/// **精确**比较。所以 `cmd` / `cmd.exe` / `CMD.EXE` /
/// `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe` 落在同一条判据上；而
/// `ssh` / `shellcheck` / `bashful-tool` / `node_modules` 不会被误杀（归一后不等于表里任何一项，
/// "含 `sh` 子串"根本不是判据）。
///
/// 为什么这一层能判：这些程序的**参数本身就是"再启动一个程序"**（`cmd /c …`、
/// `powershell -Command …`、`node -e …`、`wscript x.vbs`、`rundll32 x.dll,Entry`）。
/// 把任何一个放进白名单，`run_command` 的"argv 直传、不套 shell"保证就当场失效 ——
/// 那正是 RT-05。
///
/// 为什么不放在 `hbot-command` 判：那一层只认"这份清单里有没有这个名字"，不该知道
/// "哪些名字危险"（`docs/adr/0003-command-isolation.md` §5）。危险名字的解释权在配置层，
/// 也就是这里。
///
/// 这张表是**判据**不是**完备集合**：`env`/`busybox`/`xargs`/`npm exec` 这类"给个参数就能
/// 起别的程序"的启动器同样危险，但它们也确实是正常的开发工具，逐个拉黑会把白名单变成
/// 黑名单的补集。所以这里只钉住最常见的解释器家族，其余靠"清单只放具体程序"这条纪律
/// （ADR-0003 §5）与红队用例 `b03` 的可执行证据。
const INTERPRETER_PROGRAMS: &[&str] = &[
    // Windows 自带 shell / 命令宿主
    "cmd",
    "conhost",
    // PowerShell（含跨平台版）
    "powershell",
    "pwsh",
    // POSIX 系 shell
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    // WSL：`wsl -e <任意程序>` 一步就跨出隔离
    "wsl",
    // Windows 脚本宿主与"用参数加载可执行代码"的老牌 LOLBin
    "cscript",
    "wscript",
    "mshta",
    "rundll32",
    "regsvr32",
    // 带解释器/REPL 的运行时：`-e` / `-c` / `-r` 后面就是任意代码
    "python",
    "python3",
    "py",
    "node",
    // Debian/Ubuntu 上 `node` 的可执行名就是 `nodejs`，同一个东西。
    "nodejs",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "java",
    // macOS 的 AppleScript 宿主：`osascript -e '<任意脚本>'`
    "osascript",
];

/// 一轮对话的沙箱与命令策略。
///
/// 默认 `workspace_write`（`docs/02-architecture.md` §11.3）。注意：该档当前
/// **没有写隔离**（`docs/adr/0003-command-isolation.md`），所以命令工具在默认档
/// 下会被 `IsolationUnavailable` 拒绝 —— 这是刻意的 fail-closed，不是 bug。
/// 要真跑命令必须显式把档位切到 `danger` 并给出白名单程序。
///
/// # 白名单的准入判据（RT-05）
///
/// [`SandboxConfig::resolve_with`] 在返回之前会逐条检查 `allowedPrograms`：命中
/// [`INTERPRETER_PROGRAMS`]（解释器/外壳类）就**直接拒绝解析**（`-32002`，列出被拒的名字与
/// 原因），除非显式设了 [`ENV_ALLOW_SHELL`] —— 那时放行并打一条 `warn`。
///
/// 这条判据刻意放在**配置解析层**而不是 `hbot-command`：后者只回答"这份清单里有没有这个
/// 名字"，"哪些名字等于把 shell 装回来"是策略与配置解释，属于这里
/// （`docs/adr/0003-command-isolation.md` §5、`docs/security/red-team-findings.md` RT-05）。
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

        let from_file = file.allowed_programs;
        let allowed_programs = match pick(
            get_env,
            ENV_ALLOWED_PROGRAMS,
            from_file.as_ref().map(|list| list.join(",")),
        ) {
            Some(raw) => split_programs(&raw),
            None => Vec::new(),
        };

        // RT-05：白名单里出现解释器/外壳 ⇒ 默认直接拒绝；只有显式打开逃生阀才放行（并 warn）。
        // 判据在配置层、不在 `hbot-command`：那一层只认"清单里有没有这个名字"。
        let rejected: Vec<String> = allowed_programs
            .iter()
            .filter(|name| is_interpreter_program(name))
            .cloned()
            .collect();
        if !rejected.is_empty() {
            let source = programs_source(get_env, from_file.is_some(), data_dir);
            if allow_shell(get_env) {
                warn_allow_shell(&source, &rejected);
            } else {
                return Err(HbotError::invalid(interpreter_denial(&source, &rejected)));
            }
        }

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

/// 归一化一条白名单条目，供 [`is_interpreter_program`] 比较（RT-05）。
///
/// 四步，每一步都对应一类真实的配置写法：
///
/// 1. **取 basename**（按 `/` 与 `\` 切）—— 清单里写全路径是合法的（`hbot-command` 最后按
///    canonicalize 出的绝对路径比较），所以
///    `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe` 必须与 `powershell.exe`
///    落在同一条判据上。
/// 2. **去掉首尾空白，再去掉尾部空白与点** —— Windows 的文件名尾部点和空格会被系统忽略，
///    `cmd.` 就是 `cmd`；首尾空格则常来自手写清单时的复制粘贴。
/// 3. **转小写**（`to_ascii_lowercase`）—— `CMD.EXE` / `PowerShell.EXE` 不能漏。
/// 4. **去最后一个扩展名**（只有 `stem` 非空时才切）—— `cmd.exe` → `cmd`、`sh` → `sh`，
///    而 `.cmd` 这种"只有扩展名"的条目保持原样，避免把 `a.cmd` 切成 `a`。
fn normalize_program_name(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let name = basename
        .trim()
        .trim_end_matches(['.', ' '])
        .to_ascii_lowercase();
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name,
    }
}

/// 这条白名单条目是不是解释器/外壳类程序（RT-05 的判据）。
///
/// 只比较**归一化后的完整名字**是否等于 [`INTERPRETER_PROGRAMS`] 里的一项（外加
/// `python3.11` / `python311` 这一族），**不做子串匹配**。子串匹配是错的判据：它会把
/// `ssh`（只做 SSH 客户端该做的事）、`shellcheck`（对 shell 脚本做静态检查，不执行参数）
/// 和 `bashful-tool`、`node_modules` 一起误杀，而那些都是正经的"具体程序"。
fn is_interpreter_program(raw: &str) -> bool {
    let name = normalize_program_name(raw);
    !name.is_empty()
        && (INTERPRETER_PROGRAMS.contains(&name.as_str()) || is_versioned_python(&name))
}

/// `python3.11` / `python311` / `python2`：Windows 上的 Python 发行版会把版本号拼进可执行名，
/// 归一化只能削掉最后一段扩展名，所以对 `python` + 纯数字/点单独放宽。
fn is_versioned_python(name: &str) -> bool {
    name.strip_prefix("python").is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

/// 逃生阀开没开：只认 `1`/`true`/`yes`/`on`（大小写不敏感），**其余一律算关**。
///
/// 刻意 fail-closed：`HBOT_ALLOW_SHELL=0`、空串、`false`、`off`、拼错的值都等于"没开"。
fn allow_shell(get_env: &dyn Fn(&str) -> Option<String>) -> bool {
    get_env(ENV_ALLOW_SHELL).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// 清单是从哪儿来的（只用于报错/告警文案与"每进程只告警一次"的去重键）。
fn programs_source(
    get_env: &dyn Fn(&str) -> Option<String>,
    from_file: bool,
    data_dir: &Path,
) -> String {
    let env_used = get_env(ENV_ALLOWED_PROGRAMS).is_some_and(|value| !value.trim().is_empty());
    if env_used {
        format!("环境变量 {}", ENV_ALLOWED_PROGRAMS)
    } else if from_file {
        format!(
            "`{}` 的 allowedPrograms",
            data_dir.join(CONFIG_FILE).display()
        )
    } else {
        "命令白名单".to_string()
    }
}

/// 把名字渲染成 `` `cmd`、`powershell` `` 这种形状（报错与告警共用）。
fn rendered_programs(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join("、")
}

/// 逃生阀没开时的拒绝文案：**说明来源、列出被拒的名字、说明原因、给出两条出口**。
fn interpreter_denial(source: &str, rejected: &[String]) -> String {
    format!(
        "{source} 里有解释器/外壳类程序：{names}。这类程序的参数本身就是「再启动一个程序」\
         （`cmd /c …`、`powershell -Command …`、`node -e …`、`wscript x.vbs`），把它们放进白名单\
         等于把 shell 装回来，`run_command`「argv 直传、不套 shell」的隔离当场失效\
         （RT-05，`docs/adr/0003-command-isolation.md` §5）。请从清单里删掉这些名字；确认风险后\
         仍要放行，请显式设置 {} = 1（每次运行会留一条 warn）。",
        ENV_ALLOW_SHELL,
        names = rendered_programs(rejected),
    )
}

/// 逃生阀生效时的告警。**每进程每条清单只打一次**：`agent.json` 每个回合都会被
/// `resolve_sandbox_config()` 读一遍，不去重就是一回合一条噪声。
fn warn_allow_shell(source: &str, rejected: &[String]) {
    static WARNED_INTERPRETERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let names = rendered_programs(rejected);
    let key = format!("{source}\u{1}{names}");
    let warned = WARNED_INTERPRETERS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut warned = warned
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !warned.insert(key) {
        return;
    }
    tracing::warn!(
        "{source} 里的命令白名单包含解释器/外壳类程序：{names}。因为 {} 已显式打开，本次放行 —— \
         这些程序的参数可以启动任意命令，命令隔离对它们不再成立（RT-05）。不需要就把它从清单里删掉。",
        ENV_ALLOW_SHELL,
    );
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
            // 夹具刻意不用 `node`：它命中 RT-05 的解释器判据，会被拒（见下面的用例）。
            (ENV_ALLOWED_PROGRAMS, " git, cargo ,,git\nrustc "),
        ]);
        let config = SandboxConfig::resolve_with(dir.path(), &env).expect("白名单");
        assert_eq!(config.allowed_programs(), ["git", "cargo", "rustc"]);
    }

    // ---- RT-05：白名单里不许出现解释器/外壳类程序 -------------------------------

    #[test]
    fn interpreter_names_normalise_to_a_bare_basename() {
        // 判据的四个步骤各来一条：basename / 去尾部点与空格 / 小写 / 去扩展名。
        for (raw, expected) in [
            (
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                "powershell",
            ),
            ("/usr/local/bin/bash", "bash"),
            ("  CMD.EXE  ", "cmd"),
            ("PowerShell", "powershell"),
            ("cmd.", "cmd"),
            (r"\\?\C:\Windows\System32\cmd.exe", "cmd"),
            ("python3.11", "python3"),
            ("node_modules", "node_modules"),
            ("a.cmd", "a"),
            // 只有扩展名的条目不能被切成空串。
            (".cmd", ".cmd"),
            ("", ""),
        ] {
            assert_eq!(normalize_program_name(raw), expected, "归一化 `{raw}`");
        }
    }

    #[test]
    fn interpreters_are_rejected_by_default_and_named_in_the_error() {
        let dir = TempDir::new("rt05-reject-env");
        let env = env_of(&[
            (ENV_SANDBOX, "danger"),
            (ENV_ALLOWED_PROGRAMS, "git,cmd,powershell,py"),
        ]);
        let error = SandboxConfig::resolve_with(dir.path(), &env).expect_err("解释器必须被拒");
        assert_eq!(error.code(), -32002, "拒绝走 -32002");
        let message = error.to_string();
        // 被拒的名字要逐个回显（用户得知道删哪一行）。
        for name in ["cmd", "powershell", "py"] {
            assert!(
                message.contains(&format!("`{name}`")),
                "要列出 `{name}`：{message}"
            );
        }
        // 通过的名字不该被点名。
        assert!(!message.contains("`git`"), "不该误报 git：{message}");
        // 要说清来源、原因与逃生阀，否则用户只能猜。
        assert!(
            message.contains(ENV_ALLOWED_PROGRAMS),
            "要说明来源：{message}"
        );
        assert!(message.contains(ENV_ALLOW_SHELL), "要给出逃生阀：{message}");
        assert!(message.contains("RT-05"), "要指向发现编号：{message}");
    }

    #[test]
    fn windows_full_path_interpreter_is_recognised_from_the_env_list() {
        let dir = TempDir::new("rt05-reject-fullpath");
        // 全路径（无空格，能过 split_programs）：白名单最后是按绝对路径比较的，
        // 所以"写全路径"是最容易被当成绕过手段的写法。
        let env = env_of(&[
            (ENV_SANDBOX, "danger"),
            (
                ENV_ALLOWED_PROGRAMS,
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            ),
        ]);
        let error = SandboxConfig::resolve_with(dir.path(), &env).expect_err("全路径也要拒");
        assert_eq!(error.code(), -32002);
        let message = error.to_string();
        assert!(
            message.contains("WindowsPowerShell"),
            "要回显用户写的那一行：{message}"
        );
    }

    #[test]
    fn windows_full_path_with_spaces_is_recognised_from_agent_json() {
        let dir = TempDir::new("rt05-reject-fullpath-file");
        // 带空格的全路径只能从 agent.json 的数组进来（环境变量按空白切分）。
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"sandbox":"danger","allowedPrograms":["C:/Program Files/PowerShell/7/pwsh.exe","git"]}"#,
        )
        .expect("写配置");
        let error =
            SandboxConfig::resolve_with(dir.path(), &empty_env()).expect_err("全路径 pwsh 要拒");
        assert_eq!(error.code(), -32002);
        let message = error.to_string();
        assert!(message.contains("pwsh.exe"), "要回显被拒的名字：{message}");
        // 来源要说清是哪个 agent.json，否则多 data_dir 的部署里没法定位。
        assert!(message.contains("agent.json"), "要说明来源文件：{message}");
        assert!(message.contains("allowedPrograms"), "{message}");
    }

    #[test]
    fn interpreter_matching_is_case_insensitive_and_extension_agnostic() {
        for raw in [
            "CMD.EXE",
            "Cmd",
            "PWSH.EXE",
            "PowerShell.exe",
            "sh",
            "BASH.EXE",
            "Python",
            "python3",
            "python311",
            "python3.11",
            "node.exe",
            "nodejs",
            "wsl.exe",
            "conhost.exe",
            "cscript",
            "wscript.exe",
            "mshta.exe",
            "rundll32.exe",
            "regsvr32.exe",
            "java.exe",
            "osascript",
        ] {
            assert!(is_interpreter_program(raw), "`{raw}` 应被判为解释器/外壳");
        }
    }

    #[test]
    fn concrete_programs_are_not_flagged() {
        // 反向不误杀：子串里含 "sh"/"node" 不等于"是解释器"。
        for raw in [
            "git",
            "cargo",
            "rustc",
            "ssh",
            "shellcheck",
            "bashful-tool",
            "node_modules",
            "shasum",
            "crashreporter",
            "C:\\tools\\git.exe",
            "/usr/bin/git",
        ] {
            assert!(!is_interpreter_program(raw), "`{raw}` 是具体程序，不该被拦");
        }

        // 并且真的能从解析层过：清单原样保留。
        let dir = TempDir::new("rt05-reverse");
        let env = env_of(&[
            (ENV_SANDBOX, "danger"),
            (
                ENV_ALLOWED_PROGRAMS,
                "git,cargo,ssh,shellcheck,bashful-tool",
            ),
        ]);
        let config = SandboxConfig::resolve_with(dir.path(), &env).expect("具体程序要能用");
        assert_eq!(
            config.allowed_programs(),
            ["git", "cargo", "ssh", "shellcheck", "bashful-tool"]
        );
    }

    #[test]
    fn escape_hatch_keeps_the_names_and_only_accepts_explicit_truthy_values() {
        let dir = TempDir::new("rt05-hatch");
        // 非显式的真值一律当"没开"：拼错、`0`、`false`、空串都是 fail-closed。
        for value in ["0", "false", "no", "off", "", "shell", "maybe"] {
            let pairs = [
                (ENV_SANDBOX, "danger"),
                (ENV_ALLOWED_PROGRAMS, "cmd"),
                (ENV_ALLOW_SHELL, value),
            ];
            let env = env_of(&pairs);
            let error = match SandboxConfig::resolve_with(dir.path(), &env) {
                Ok(_) => panic!("`{ENV_ALLOW_SHELL}={value}` 不该打开逃生阀"),
                Err(error) => error,
            };
            assert_eq!(error.code(), -32002);
        }
        // 显式真值才放行，而且放行**不删名字**（否则命令层拿到空清单会全拒，症状很迷惑）。
        for value in ["1", "true", "TRUE", "Yes", "on"] {
            let pairs = [
                (ENV_SANDBOX, "danger"),
                (ENV_ALLOWED_PROGRAMS, "cmd,git"),
                (ENV_ALLOW_SHELL, value),
            ];
            let env = env_of(&pairs);
            let config = match SandboxConfig::resolve_with(dir.path(), &env) {
                Ok(config) => config,
                Err(error) => panic!("`{ENV_ALLOW_SHELL}={value}` 应打开逃生阀：{error}"),
            };
            assert_eq!(config.allowed_programs(), ["cmd", "git"]);
        }
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
