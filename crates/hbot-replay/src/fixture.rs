//! 夹具（fixture）：把一批历史入站存成**能直接重跑**的事件数组，并在当前代码上重跑。
//!
//! # 文件格式：顶层就是事件数组
//!
//! ```json
//! [
//!   {
//!     "botId": "0f0f0f0f-...",
//!     "conversationId": "1111...",
//!     "messageId": "2222...",
//!     "externalId": "m1",
//!     "senderId": "10086",
//!     "text": "你好",
//!     "kind": "group",
//!     "conversationExternalId": "30001",
//!     "receivedAt": 1700000000000,
//!     "payload": { "text": "你好", "senderId": "10086", "...": "..." }
//!   }
//! ]
//! ```
//!
//! 元素形状**就是** [`hbot_bot::event_json`] 的产出（也就是生产链路塞进 isolate 的那份
//! JSON 原文），所以 [`run_fixture`] 不需要任何翻译层，直接
//! [`hbot_code::Sandbox::dispatch_collecting`] 喂进去即可。元素多出来的键被忽略（前向
//! 兼容：将来事件加字段，旧夹具照样能跑），缺任一必备键都算"形状不对"。
//!
//! 顶层刻意**不是** `{"version":1,"events":[...]}` 包装对象：契约里"坏文件"的判据是
//! **内容不是 JSON 数组**，包装一下这条判据就得拆成两段（先查包装、再查数组），而且
//! `jq '.[0].payload'` 这类人肉排查会多一层 `events`。真要加版本字段，元素级加（元素
//! 里多键被忽略）比顶层套壳更省事。
//!
//! # 与 [`crate::replay`] 的 `save_fixture` 不是一回事
//!
//! `ReplayOptions::save_fixture`（`hbot replay --save-fixture`）落的是 **payload 数组**
//! ——历史遗留格式，只给人看，喂不回 isolate。本模块的 [`save_fixture`] 落的是**完整
//! 事件**，能直接被 [`run_fixture`] 重跑。两份格式不同是有意的：前者已经发布，改它等于
//! 改既有行为（见 `hbot-cli/tests/replay.rs`）。
//!
//! # 两份库、两条路径
//!
//! - [`save_fixture`] 读的是**调用方打开的那份库**（IDE 场景是 `AppState` 的应用库，
//!   CLI 场景是 `<project>/.hbot/hbot.db`）——与 [`crate::replay`] 同一条规矩。
//! - [`run_fixture`] **只读夹具文件，一次库都不查**：这正是它存在的理由（把别处导出的
//!   夹具当回归用例跑，不需要历史）。
//!
//! # 路径守卫
//!
//! 夹具路径必须落在项目内，判据**复用** [`hbot_workspace::Workspace`]（`../`、绝对
//! 路径、盘符、`.git/`、Windows 保留名、符号链接逃逸、原子写 + Windows rename 重试都在
//! 那边，本模块不另造一份）。这里只做一件事：把"相对项目根的路径或项目内的绝对路径"
//! 解析成一个**已规范化**的绝对路径，好让 `Workspace` 的相对路径守卫能吃下它。
//!
//! 父目录**不存在时自动逐级创建**（`Workspace::write_file` 本来就是这个语义）：默认
//! 夹具目录 `.hbot/fixtures/` 第一次保存时必然不存在，要求调用方先手工建目录只会让功能
//! 开箱不可用。

use std::path::{Component, Path, PathBuf};
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use hbot_code::Limits;
use hbot_code::manifest;
use hbot_code::{CompiledProject, Sandbox, compile_project_with_entry};
use hbot_store::{BotId, Store};
use hbot_workspace::{Workspace, WorkspaceConfig};

use crate::{ReplayOptions, display_path, event_of_row, select_inbound};

/// 保底：一条事件的必备字符串字段。
const REQUIRED_STRINGS: [&str; 7] = [
    "botId",
    "conversationId",
    "messageId",
    "externalId",
    "senderId",
    "kind",
    "conversationExternalId",
];

/// 夹具保存 / 重跑的错误。
///
/// 三个变体**直接对应 RPC 的三个失败通道**（`-32002` / `-32001` / `-32006`），
/// 所以调用方不需要解析文案就能映射（见 `hbot-rpc` 的 `commands::replay`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureError {
    /// 调用方给的路径或参数不成立（要 `-32002`）：空路径、逃出项目、不是 JSON 数组、
    /// 元素形状不对、文件不存在。
    Invalid(String),
    /// 库里没有这个 bot，或没有可保存的历史入站（要 `-32001`）。
    NotFound(String),
    /// 读库失败、写盘失败、编译失败、isolate 起不来（要 `-32006`）。
    Engine(String),
}

impl FixtureError {
    /// 给人看的说明（不含错误码前缀）。
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(message) | Self::NotFound(message) | Self::Engine(message) => message,
        }
    }
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for FixtureError {}

/// 一条夹具事件：原文 + 报告里要用的两个字段。
struct FixtureEvent {
    /// 原封不动喂给 isolate 的那份 JSON。
    raw: Value,
    external_id: String,
    text: Option<String>,
}

/// 一条事件的执行结果（对应 `hbot-rpc` DTO 里 `FixtureEntry` 的 wire 形状）。
///
/// 序列化形状（snake_case）是给 CLI 的 `--json` 用的；RPC 那边映射成 camelCase 的
/// `dto::FixtureEntry`（wire 契约在 `hbot-rpc`，不在这里）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureEntry {
    /// 在夹具数组里的下标（从 0 开始）。
    pub index: u32,
    /// 事件里的 `externalId`。
    pub external_id: String,
    /// 事件里的 `text`（没有就是 `None`）。
    pub text: Option<String>,
    /// 这条事件跑出来的回复（顺序与 handler 调用 `reply` 的顺序一致）。
    pub replies: Vec<String>,
    /// handler 抛异常时的说明；正常为 `None`。
    pub error: Option<String>,
}

/// 一次夹具重跑的报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureRunReport {
    /// 夹具文件路径（已去掉 Windows 的 `\\?\` 前缀，方便直接回显）。
    pub path: String,
    /// 夹具里有几条事件。
    pub events: u32,
    /// 跑出来的回复总条数（所有事件相加）。
    pub replies: u32,
    /// 出错的事件条数。
    pub errors: u32,
    /// 逐条结果，顺序与夹具一致。
    pub entries: Vec<FixtureEntry>,
}

/// 存下来的夹具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFixture {
    /// 落盘路径（已规范化、去掉 `\\?\` 前缀）。
    pub path: String,
    /// 写进去的事件条数。
    pub events: u32,
}

/// 把一批历史入站存成夹具文件。
///
/// `store` 是调用方打开的那份历史库（与 [`crate::replay`] 同一规矩，本函数不打开库）。
/// `options` 复用回放的那一份：`project_dir` 用来解析路径与守卫、`bot` / `from` / `limit`
/// 决定选哪一批入站；`options.save_fixture` 与 `options.database` 在这里**不参与**
/// （前者是历史遗留的 payload 落盘，后者只是给人看的库名）。
///
/// `path` 是命令给的原始字符串：相对路径按项目根解析，绝对路径必须在项目内。
pub fn save_fixture(
    store: &Store,
    options: &ReplayOptions,
    path: &str,
) -> Result<SavedFixture, FixtureError> {
    // 路径先判：越界 / 空路径是调用方的错，比"库里有没有历史"更早暴露。
    let target = resolve_fixture_path(&options.project_dir, path)?;
    if target.is_dir() {
        return Err(FixtureError::Invalid(format!(
            "夹具路径指向一个目录（{}）：给一个文件名",
            display_path(&target)
        )));
    }
    if options.limit == Some(0) {
        // `select_inbound` 里也有这条，但那里返回 `String`，分不出该映射成哪个码。
        return Err(FixtureError::Invalid(
            "limit=0 会一条都不存；要全量就别给 limit".to_owned(),
        ));
    }

    let bot_id = BotId::from_str(&options.bot).map_err(|error| {
        FixtureError::Invalid(format!(
            "bot 不是合法的 bot id（要 UUID）：{}：{error}",
            options.bot
        ))
    })?;
    let database = options.database.as_path();
    let bot = store.get_bot(&bot_id).map_err(|error| {
        FixtureError::Engine(format!("读 bot 失败（{}）：{error}", database.display()))
    })?;
    if bot.is_none() {
        return Err(FixtureError::NotFound(format!(
            "库里没有 bot {}（库：{}）",
            options.bot,
            database.display()
        )));
    }

    let rows = select_inbound(store, &bot_id, options, database).map_err(FixtureError::Engine)?;
    if rows.is_empty() {
        let mut message = format!(
            "bot {} 在 {} 里没有可保存的历史入站",
            options.bot,
            database.display()
        );
        if let Some(millis) = options.from {
            message.push_str(&format!("（`from` 下界 {millis} 毫秒之后一条都没有）"));
        }
        message.push_str("；先让它真跑几条消息再存夹具");
        return Err(FixtureError::NotFound(message));
    }

    // 事件 JSON 走与回放**同一条**构造路径（`event_of_row`），确保"存下来的"与
    // "回放时喂进去的"永远是同一个形状。
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        let payload =
            serde_json::from_str::<Value>(&row.message.payload_json).map_err(|error| {
                FixtureError::Engine(format!(
                    "入站 {}（external_id={}）的 payload_json 不是合法 JSON，没法存夹具：{error}",
                    row.message.id, row.message.external_id
                ))
            })?;
        events.push(event_of_row(row, bot_id, &payload));
    }

    let text = serde_json::to_string_pretty(&events)
        .map_err(|error| FixtureError::Engine(format!("序列化夹具失败：{error}")))?;
    let text = format!("{text}\n");
    write_fixture_file(&options.project_dir, &target, &text)?;
    let events_count = count(events.len());
    tracing::info!(
        path = %display_path(&target),
        events = events_count,
        bot = %options.bot,
        "夹具已保存"
    );
    Ok(SavedFixture {
        path: display_path(&target),
        events: events_count,
    })
}

/// 用当前代码把一份夹具从头跑一遍（**不查库**）。
///
/// 编译与 isolate 的启动都在这里做（与 [`crate::replay`] 同一条路），所以调用方只需要
/// 给项目根与夹具路径两个参数。
pub fn run_fixture(project_dir: &Path, path: &Path) -> Result<FixtureRunReport, FixtureError> {
    if !project_dir.is_dir() {
        return Err(FixtureError::Invalid(format!(
            "项目目录不存在或不是目录：{}",
            display_path(project_dir)
        )));
    }
    // 先验夹具再编译：坏文件不该浪费一次编译（几秒）才报出"不是 JSON 数组"。
    let events = read_fixture_events(path)?;
    if events.is_empty() {
        return Ok(empty_report(display_path(path)));
    }
    let manifest = manifest::load(project_dir).map_err(|error| {
        // 与"编译当前代码失败"同一档：项目清单坏了也是引擎侧起不来，不是夹具文件的错。
        FixtureError::Engine(format!(
            "读不了项目清单 {}：{error}",
            display_path(&project_dir.join("hbot.json"))
        ))
    })?;
    let entry_name = manifest.entry_name();
    let compiled = compile_project_with_entry(project_dir, &entry_name).map_err(|error| {
        FixtureError::Engine(format!(
            "编译当前代码失败（{}）：{error}",
            display_path(project_dir)
        ))
    })?;
    run_events(compiled, manifest.sandbox_limits(), path, events)
}

/// [`run_fixture`] 的下半截：编译产物已经在手上时直接跑。
///
/// 拆出来是给"一次编译、多份夹具"的调用方用的（也方便单测跳过编译）。
pub fn run_fixture_compiled(
    compiled: CompiledProject,
    limits: Limits,
    path: &Path,
) -> Result<FixtureRunReport, FixtureError> {
    let events = read_fixture_events(path)?;
    run_events(compiled, limits, path, events)
}

/// 拿到（已验证的）事件之后真正干活的那一段。
fn run_events(
    compiled: CompiledProject,
    limits: Limits,
    path: &Path,
    events: Vec<FixtureEvent>,
) -> Result<FixtureRunReport, FixtureError> {
    let display = display_path(path);
    if events.is_empty() {
        return Ok(empty_report(display));
    }
    let events_count = count(events.len());
    let sandbox = Sandbox::spawn(compiled, limits)
        .map_err(|error| FixtureError::Engine(format!("启动新 isolate 失败：{error}")))?;
    let mut entries = Vec::with_capacity(events.len());
    let mut replies = 0u32;
    let mut errors = 0u32;
    for (index, event) in events.into_iter().enumerate() {
        let mut entry = FixtureEntry {
            index: count(index),
            external_id: event.external_id,
            text: event.text,
            replies: Vec::new(),
            error: None,
        };
        match sandbox.dispatch_collecting(&event.raw) {
            Ok(outcome) => {
                replies = replies.saturating_add(count(outcome.replies.len()));
                entry.replies = outcome.replies;
            }
            Err(failure) => {
                errors = errors.saturating_add(1);
                entry.error = Some(format!("handler 抛异常：{}", failure.error));
            }
        }
        entries.push(entry);
    }
    Ok(FixtureRunReport {
        path: display,
        events: events_count,
        replies,
        errors,
        entries,
    })
}

/// 空夹具的报告：零事件零回复，**不起 isolate**（起一次要几十毫秒到几百毫秒）。
fn empty_report(path: String) -> FixtureRunReport {
    FixtureRunReport {
        path,
        events: 0,
        replies: 0,
        errors: 0,
        entries: Vec::new(),
    }
}

/// 把"相对项目根的路径 / 项目内的绝对路径"解析成项目内**已规范化**的绝对路径。
///
/// 两个调用方都会用它：`hbot-replay` 自己写盘前，以及 `hbot-rpc` 在进
/// `spawn_blocking` 之前先挡一次（便宜的那道闸）。
///
/// 判据（`..` 允许，只要**解析之后**仍在项目内——与契约一致）：
/// 1. 空路径、只有空白 → [`FixtureError::Invalid`]；
/// 2. 盘符相对路径（`C:foo`）→ [`FixtureError::Invalid`]（它不是"项目内相对路径"）；
/// 3. 词法归一化（`.` 丢弃、`..` 出栈）→ [`FixtureError::Invalid`] 之前先做；
/// 4. 取最深的已存在祖先 `canonicalize`（吃掉 Windows 的 `\\?\` 前缀与符号链接），
///    缺失的尾巴原样拼回；
/// 5. 结果必须 `starts_with(项目根)` 且不是项目根本身。
pub fn resolve_fixture_path(project_dir: &Path, path: &str) -> Result<PathBuf, FixtureError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(FixtureError::Invalid(
            "夹具路径不能为空：给一个项目内的相对路径（例如 .hbot/fixtures/my-bot.json）"
                .to_owned(),
        ));
    }
    let workspace = open_fixture_workspace(project_dir)?;
    let root = workspace.root().to_path_buf();
    let candidate = Path::new(trimmed);
    if matches!(candidate.components().next(), Some(Component::Prefix(_)))
        && !candidate.is_absolute()
    {
        return Err(FixtureError::Invalid(format!(
            "`{trimmed}` 是盘符相对路径（形如 `C:foo`），不是项目内路径：\
             给相对项目根的路径，或项目内的绝对路径"
        )));
    }
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let resolved = canonicalize_deepest(&lexical_normalize(&absolute))?;
    if resolved == root {
        return Err(FixtureError::Invalid(
            "夹具路径指向项目根目录本身：给一个文件名".to_owned(),
        ));
    }
    if !resolved.starts_with(&root) {
        return Err(FixtureError::Invalid(format!(
            "夹具必须落在项目内：`{trimmed}` 解析到项目外；项目根是 {}",
            display_path(&root)
        )));
    }
    Ok(resolved)
}

/// 用**放行 `.hbot/`** 的策略打开项目根：夹具默认就落在 `.hbot/fixtures/`。
fn open_fixture_workspace(project_dir: &Path) -> Result<Workspace, FixtureError> {
    let config = WorkspaceConfig {
        allow_dot_hbot: true,
        ..WorkspaceConfig::default()
    };
    Workspace::open_with_config(project_dir.to_path_buf(), config).map_err(|error| {
        FixtureError::Invalid(format!(
            "项目根不可用（{}），夹具路径必须相对项目根解析：{error}",
            display_path(project_dir)
        ))
    })
}

/// 词法归一化：丢掉 `.`、`..` 出栈（到根再 `..` 就停在根）。
///
/// 只做**文本**层面的收敛，所以后面还要 [`canonicalize_deepest`] 一次：符号链接必须真的
/// 解析过，才算"项目内"。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// 规范化"最深的已存在祖先"，把缺失的尾巴原样拼回去。
///
/// 为什么不能直接 `canonicalize`：`path` 指向的文件通常**还不存在**（第一次保存），
/// 而 `canonicalize` 对不存在的路径一律报错。为什么不能只看词法路径：Windows 的
/// `temp_dir()` 会给 8.3 短路径（`ADMINI~1`）、`canonicalize` 给 `\\?\` 长路径，两边不
/// 规范化就没法比 `starts_with`。
fn canonicalize_deepest(absolute: &Path) -> Result<PathBuf, FixtureError> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = absolute.to_path_buf();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) => {
                let Some(name) = cursor.file_name().map(std::ffi::OsStr::to_os_string) else {
                    return Err(FixtureError::Invalid(format!(
                        "夹具路径解析不了（{}）：{error}",
                        display_path(absolute)
                    )));
                };
                let Some(parent) = cursor.parent() else {
                    return Err(FixtureError::Invalid(format!(
                        "夹具路径解析不了（{}）：{error}",
                        display_path(absolute)
                    )));
                };
                missing.push(name);
                cursor = parent.to_path_buf();
            }
        }
    }
}

/// 读夹具文件并逐条校验形状；任何一条不合法都让整份夹具失败（**不静默跳过**）。
fn read_fixture_events(path: &Path) -> Result<Vec<FixtureEvent>, FixtureError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        FixtureError::Invalid(format!(
            "读不了夹具文件（{}）：{error}（文件不存在、或不是 UTF-8）",
            display_path(path)
        ))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        FixtureError::Invalid(format!(
            "夹具文件不是合法 JSON（{}）：{error}",
            display_path(path)
        ))
    })?;
    let Value::Array(items) = value else {
        return Err(FixtureError::Invalid(format!(
            "夹具文件的顶层必须是 JSON 数组（{}），实际是{}",
            display_path(path),
            json_type(&value)
        )));
    };
    let mut events = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        events.push(check_event(path, index, item)?);
    }
    Ok(events)
}

/// 校验一条元素是不是 [`hbot_bot::event_json`] 形状的事件。
fn check_event(path: &Path, index: usize, value: &Value) -> Result<FixtureEvent, FixtureError> {
    let Some(object) = value.as_object() else {
        return Err(element_error(
            path,
            index,
            format!("它不是 JSON 对象，而是{}", json_type(value)),
        ));
    };
    for field in REQUIRED_STRINGS {
        if object.get(field).and_then(Value::as_str).is_none() {
            return Err(element_error(
                path,
                index,
                format!("缺字段 `{field}`（或它不是字符串）"),
            ));
        }
    }
    let text = match object.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => {
            return Err(element_error(
                path,
                index,
                format!("`text` 应当是字符串或 null，实际是{}", json_type(other)),
            ));
        }
    };
    if object.get("receivedAt").and_then(Value::as_i64).is_none() {
        return Err(element_error(
            path,
            index,
            "缺字段 `receivedAt`（应当是整数毫秒时间戳）".to_owned(),
        ));
    }
    if object.get("payload").and_then(Value::as_object).is_none() {
        return Err(element_error(
            path,
            index,
            "缺字段 `payload`（应当是 JSON 对象）".to_owned(),
        ));
    }
    let external_id = object
        .get("externalId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(FixtureEvent {
        raw: value.clone(),
        external_id,
        text,
    })
}

/// 一条元素的形状错误。
fn element_error(path: &Path, index: usize, reason: String) -> FixtureError {
    FixtureError::Invalid(format!(
        "夹具 {} 第 {} 条不是合法的事件对象：{reason}；\
         夹具应当是 replay_save_fixture 导出的 JSON 数组（元素就是 isolate 收到的那份事件）",
        display_path(path),
        index + 1
    ))
}

/// 原子落盘：交给 [`Workspace::write_file`]（同目录 `.hbot-tmp-*` → fsync → rename，
/// Windows 的 EPERM 有界重试），并顺带复用它的路径守卫（`.git/`、保留名、符号链接逃逸）。
fn write_fixture_file(project_dir: &Path, target: &Path, text: &str) -> Result<(), FixtureError> {
    let workspace = open_fixture_workspace(project_dir)?;
    let relative = workspace
        .relative_path(target)
        .map_err(|error| FixtureError::Invalid(format!("夹具必须落在项目内：{error}")))?;
    workspace
        .write_file(Path::new(&relative), text, None)
        .map_err(|error| {
            FixtureError::Engine(format!(
                "写夹具文件失败（{}）：{error}",
                display_path(target)
            ))
        })?;
    Ok(())
}

/// `usize` → `u32`（wire 上就是 `u32`；超出就饱和，不要 panic）。
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// JSON 值的中文类型名（错误文案用）。
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}
