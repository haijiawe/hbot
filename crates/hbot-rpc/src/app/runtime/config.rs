//! 项目根 `bots.json`：M0 阶段 bot 连接配置的唯一来源。
//!
//! 为什么是文件而不是数据库列：`bots.config_json` 已经在库里，但没有任何写入路径
//! （`project_open` 只建项目，建 bot 的界面属于 IDE，还没接线）。M0 要的是"改一个文件
//! 就能真机跑起来"，而不是先做配置 UI；等 IDE 有了写入路径，这个文件退化成导入来源即可。
//!
//! schema（项目根 `bots.json`，UTF-8，camelCase）：
//!
//! ```json
//! {
//!   "bots": [
//!     {
//!       "id": "22222222-2222-4222-8222-222222222222",
//!       "url": "ws://127.0.0.1:3001",
//!       "token": "可选：OneBot 的 access_token",
//!       "selfId": 10001,
//!       "name": "可选：展示名，缺省用项目目录名",
//!       "platform": "可选：onebot11（缺省）或 qq_official",
//!       "limits": { "memoryMb": 32, "handlerTimeoutMs": 2000 }
//!     },
//!     {
//!       "id": "33333333-3333-4333-8333-333333333333",
//!       "platform": "qq_official",
//!       "appId": "102000000",
//!       "clientSecret": "QQ 开放平台机器人后台的 AppSecret",
//!       "name": "QQ 官方机器人示例"
//!     }
//!   ]
//! }
//! ```
//!
//! 字段说明：
//!
//! * `id`（必填）— 必须是 `bots.id` 那个 UUID 的字符串形式；一个文件可以放多个 bot。
//! * `platform`（可选）— 平台标识，**精确匹配**：`onebot11`（缺省）或 `qq_official`。
//!   写错 / 写了个没实现的值一律 -32002 并列出允许值，**不静默降档成 OneBot**
//!   （与 `docs/02-architecture.md` "写错档位不许静默降档"同一条纪律）。
//! * `url`（`onebot11` 必填）— OneBot 11 正向 WebSocket 地址，`ws://` 与 `wss://` 都可以。
//!   `wss://` 走 rustls + `webpki-roots`，**证书校验始终开启**：证书不匹配报
//!   `AdapterError::Tls`（wire 上 -32006），不会降级成明文，也**没有**跳过校验的开关
//!   （私有 CA / 自签部署用 `OneBotConfig::with_extra_root_certificates` 追加根证书）。
//!   `platform = qq_official` 时**不用**这个字段（QQ 官方走固定端点），填了也不参与连接。
//! * `token`（可选，仅 `onebot11`）— 作为 `Authorization: Bearer <token>` 发出；日志里打码。
//! * `appId`（`qq_official` 必填）— QQ 开放平台机器人后台的 AppID。不是密钥，可以进日志，
//!   但仍不刷屏。
//! * `clientSecret`（`qq_official` 必填）— QQ 开放平台机器人后台的 AppSecret。**按密钥对待**：
//!   手写的 `Debug` 把它打成 `<redacted>`，错误消息与日志里都不出现原文。
//! * `selfId`（可选，仅 `onebot11`）— bot 自己的 QQ 号；只用于乐观校验收到的事件是不是发给自己的。
//! * `name`（可选）— 展示名；缺省取项目根目录名（`bots.name`）。
//! * `limits`（可选）— 交给沙箱的每 isolate 限额，缺省即
//!   [`hbot_code::Limits::default`]（32 MiB / 2 s）。
//!
//! 所有读盘都是阻塞的：调用方负责把它放进 `spawn_blocking` / `AppState::blocking`。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use hbot_code::Limits;
use hbot_core::{BotId, HbotError};
use serde::Deserialize;

use super::{onebot, platform_list, qq};

/// 配置文件名（位于项目根）。
pub const BOTS_FILE: &str = "bots.json";

/// 一个 bot 的连接参数 + 沙箱限额。
///
/// 字段按平台分流：`onebot11` 用 [`BotConfig::ws_url`] / [`BotConfig::access_token`]，
/// `qq_official` 用 [`BotConfig::app_id`] / [`BotConfig::client_secret`]。
/// **两个平台的字段都留在同一个结构里**而不是搞两层 enum：`AdapterFactory::create`
/// 收的是 `&BotConfig`，平台路由在工厂那一层决定用哪几个字段；这比让 callers 先
/// 解 enum 再传更少的改动面，也不会在 `bots.json` 里造出两套 id/limits。
///
/// **`Debug` 是手写的**，不派生：`access_token` / `client_secret` 一旦被
/// `tracing::warn!(?config)` 打印出来就会进日志文件。同一仓库的 `OneBotConfig` /
/// `QqConfig` 早就是这个口径（`crates/hbot-adapters/src/platforms/onebot/client.rs`
/// 的 `config_debug_redacts_token`、`qq/config.rs` 的 `QqConfig::fmt`）。
#[derive(Clone, PartialEq, Eq)]
pub struct BotConfig {
    /// OneBot 11 正向 WebSocket 地址（`platform = onebot11` 时必填；QQ 官方用不到）。
    pub ws_url: Option<String>,
    /// 可选的 OneBot access token，按 `Authorization: Bearer` 发送。
    pub access_token: Option<String>,
    /// QQ 官方机器人的 AppID（`platform = qq_official` 时必填）。
    pub app_id: Option<String>,
    /// QQ 官方机器人的 AppSecret（`platform = qq_official` 时必填）；日志里打码。
    pub client_secret: Option<String>,
    /// bot 自己的平台上 id（QQ 号）。
    pub self_id: Option<i64>,
    /// 展示名（`bots.name`）。缺省时调用方用项目目录名兜底。
    pub name: Option<String>,
    /// 平台标识（`bots.platform`）。`None` = 文件里没写，等价于 `onebot11`。
    pub platform: Option<String>,
    /// 交给沙箱的每 isolate 限额。
    pub limits: Limits,
}

impl BotConfig {
    /// 只有 OneBot 地址、其余字段取缺省值。
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: Some(ws_url.into()),
            access_token: None,
            app_id: None,
            client_secret: None,
            self_id: None,
            name: None,
            platform: None,
            limits: Limits::default(),
        }
    }

    /// 只有 QQ 官方凭据、其余字段取缺省值（`platform` 自动填 [`qq::PLATFORM`]）。
    ///
    /// 注意 `ws_url` 是 `None`：QQ 官方机器人没有用户可配的 WebSocket 地址
    /// （网关地址由适配器用 access token 换），所以这里**不造一个空串**去占位
    /// —— 空串会让人误以为"连了一个空地址"。
    pub fn qq(app_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            ws_url: None,
            access_token: None,
            app_id: Some(app_id.into()),
            client_secret: Some(client_secret.into()),
            self_id: None,
            name: None,
            platform: Some(qq::PLATFORM.to_owned()),
            limits: Limits::default(),
        }
    }

    /// 生效的平台标识：文件里写了就用写的，没写就是 `onebot11`。
    ///
    /// 这是"缺省平台"的**唯一**判定点：`read_bot_config` 用它做字段校验，
    /// `PlatformRouter` 用它跟 `bots.platform` 列对账。
    pub fn effective_platform(&self) -> &str {
        self.platform.as_deref().unwrap_or(onebot::PLATFORM)
    }
}

/// [`BOTS_FILE`] 在项目里的完整路径（不保证存在）。
pub fn bots_file(root: &Path) -> PathBuf {
    root.join(BOTS_FILE)
}

impl std::fmt::Debug for BotConfig {
    /// `access_token` 与 `client_secret` 恒为 `<redacted>`；其余字段照常打印。
    ///
    /// 这不是洁癖：`BotConfig` 会被塞进错误上下文与启动日志，派生 `Debug`
    /// 会在任何一次 `tracing::warn!(?config)` 里把密钥写进日志文件。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotConfig")
            .field("ws_url", &self.ws_url)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("app_id", &self.app_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("self_id", &self.self_id)
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("limits", &self.limits)
            .finish()
    }
}

/// 从项目根的 `bots.json` 读出某个 bot 的配置。
///
/// 失败一律是 [`HbotError::invalid`]（wire 码 -32002）：这是**用户配置**问题，
/// 消息里必须带上文件路径、bot id 和可照抄的 schema，否则用户只能猜。
///
/// 字段按 `platform` **必填校验**（缺哪个点哪个名，不许退化成空串去连）：
///
/// * `onebot11`（含不写 `platform`）— 必须有非空 `url`；
/// * `qq_official` — 必须有 `appId` 与 `clientSecret`；
/// * 其它值 — 直接报错并列出允许值。
///
/// # Errors
///
/// 文件不存在、JSON 不合法、没有该 bot 的条目、缺该平台必填的字段、`platform` 不认识
/// —— 都是 -32002。
pub fn read_bot_config(root: &Path, bot_id: &BotId) -> Result<BotConfig, HbotError> {
    let path = bots_file(root);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        HbotError::invalid(format!(
            "bot {bot_id} 没有可用配置：读不到 {}（{error}）。在项目根建 bots.json，\
             格式见 docs/02-architecture.md 与 crates/hbot-rpc/src/app/runtime/config.rs：\
             {{\"bots\":[{{\"id\":\"{bot_id}\",\"url\":\"ws://127.0.0.1:3001\"}}]}}（OneBot 11）\
             或 {{\"bots\":[{{\"id\":\"{bot_id}\",\"platform\":\"qq_official\",\
             \"appId\":\"...\",\"clientSecret\":\"...\"}}]}}（QQ 官方机器人）",
            path.display()
        ))
    })?;

    // 未知字段只**告警**、不失败（`deserialize_with_warnings` 的文档写了为什么不用
    // `deny_unknown_fields`）；字段类型错、缺必填、JSON 语法错仍然是 -32002。
    let parsed: BotsFile =
        deserialize_with_warnings(&path, &raw, bots_known_fields).map_err(|error| {
            HbotError::invalid(format!(
                "bot {bot_id} 的配置无法解析：{} 不是合法的 bots.json（{error}）",
                path.display()
            ))
        })?;

    let entry = parsed
        .bots
        .into_iter()
        .find(|entry| entry.id.trim() == bot_id.to_string())
        .ok_or_else(|| {
            HbotError::invalid(format!(
                "bot {bot_id} 没有可用配置：{} 里没有 id 等于 {bot_id} 的条目",
                path.display()
            ))
        })?;

    let BotEntry {
        id: _,
        url,
        token,
        app_id,
        client_secret,
        self_id,
        name,
        platform,
        limits: limits_entry,
    } = entry;

    // `platform` 的"没写 = onebot11"只在 `effective_platform` 判定一次。
    let platform = trimmed(platform);
    let effective = platform.as_deref().unwrap_or(onebot::PLATFORM).to_owned();
    let ws_url = match effective.as_str() {
        onebot::PLATFORM => Some(require_onebot_url(&path, bot_id, url)?),
        qq::PLATFORM => {
            // QQ 官方不用 ws:// 地址（网关靠 access token 换），填了也不参与连接。
            // 这里**不**把空串塞进 ws_url：空串的语义是"连一个空地址"，会被误解。
            None
        }
        other => {
            return Err(HbotError::invalid(format!(
                "bot {bot_id} 没有可用配置：{} 里该条目的 platform = `{other}` 不认识；\
                 允许的取值是 {}（不写这个字段等价于 `{}`）",
                path.display(),
                platform_list(),
                onebot::PLATFORM
            )));
        }
    };

    let app_id = trimmed(app_id);
    let client_secret = trimmed(client_secret);
    if effective == qq::PLATFORM {
        if app_id.is_none() {
            return Err(HbotError::invalid(format!(
                "bot {bot_id} 没有可用配置：{} 里该条目缺 appId（platform = `{}` 时必须同时给 \
                 appId 与 clientSecret：QQ 开放平台机器人后台的 AppID / AppSecret）",
                path.display(),
                qq::PLATFORM
            )));
        }
        if client_secret.is_none() {
            return Err(HbotError::invalid(format!(
                "bot {bot_id} 没有可用配置：{} 里该条目缺 clientSecret（platform = `{}` 时 \
                 appId 与 clientSecret 都是必填；clientSecret 就是 QQ 开放平台机器人后台的 \
                 AppSecret，只在后台重置时显示）",
                path.display(),
                qq::PLATFORM
            )));
        }
    }

    let limits = match limits_entry {
        None => Limits::default(),
        Some(limits) => {
            let mut base = Limits::default();
            if let Some(megabytes) = limits.memory_mb {
                // 0 不是"无限制"，是一个会让 isolate 立刻 OOM 的配置；夹到 1 MiB。
                let bytes = usize::try_from(megabytes)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(1024 * 1024)
                    .max(1024 * 1024);
                base = base.with_memory_bytes(bytes);
            }
            if let Some(millis) = limits.handler_timeout_ms {
                let millis = millis.max(1);
                base = base.with_handler_timeout(Duration::from_millis(millis));
            }
            base
        }
    };

    Ok(BotConfig {
        ws_url,
        access_token: trimmed(token),
        app_id,
        client_secret,
        self_id,
        name: trimmed(name),
        platform,
        limits,
    })
}

/// OneBot 11 的 `url`：必填，且空串与"没写"分开报（用户能一眼看出是哪种）。
fn require_onebot_url(
    path: &Path,
    bot_id: &BotId,
    url: Option<String>,
) -> Result<String, HbotError> {
    match url {
        Some(raw) if !raw.trim().is_empty() => Ok(raw.trim().to_owned()),
        Some(_) => Err(HbotError::invalid(format!(
            "bot {bot_id} 没有可用配置：{} 里该条目的 url 为空（platform = `{}` 时必须给 \
             OneBot 11 正向 WebSocket 地址，如 \"ws://127.0.0.1:3001\"）",
            path.display(),
            onebot::PLATFORM
        ))),
        None => Err(HbotError::invalid(format!(
            "bot {bot_id} 没有可用配置：{} 里该条目缺 url（platform = `{}` 时必须给 \
             OneBot 11 正向 WebSocket 地址，如 \"ws://127.0.0.1:3001\"）",
            path.display(),
            onebot::PLATFORM
        ))),
    }
}

/// 去掉首尾空白；空串当成"没给"（与 `read_bot_config` 的历史口径一致）。
fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// `bots.json` 最外层已知字段（camelCase，与 [`BotsFile`] 的 serde 名一致）。
///
/// 这几张表只用来给"你是不是想写 XXX"提建议，**不参与解析**：真正的 schema 仍然是
/// [`BotsFile`] / [`BotEntry`] / [`LimitsEntry`] 的 derive。表里多一条或少一条只会让
/// 建议变差，不会让解析结果变。
const BOTS_ROOT_FIELDS: &[&str] = &["bots"];

/// `bots[]` 每个条目已知的字段。
const BOT_ENTRY_FIELDS: &[&str] = &[
    "id",
    "url",
    "token",
    "appId",
    "clientSecret",
    "selfId",
    "name",
    "platform",
    "limits",
];

/// `bots[].limits` 里已知的字段。
const BOT_LIMITS_FIELDS: &[&str] = &["memoryMb", "handlerTimeoutMs"];

/// 按被忽略键所在的**层级**挑建议用的字段表。
///
/// 路径是 [`render_path`] 渲染出来的形态（`bots[0].limits.memoryMB`），所以这里靠
/// 前缀/中缀判断层级；判断错了只会让建议指向另一层，不影响"未知字段"这条告警本身。
fn bots_known_fields(rendered_path: &str) -> &'static [&'static str] {
    if !rendered_path.starts_with("bots[") {
        return BOTS_ROOT_FIELDS;
    }
    if rendered_path.contains(".limits.") {
        return BOT_LIMITS_FIELDS;
    }
    BOT_ENTRY_FIELDS
}

/// 一条被 `serde_ignored` 报上来的、被忽略的键。
struct IgnoredField {
    /// 渲染后的完整路径，如 `bots[0].clientSecert`。
    path: String,
    /// 最近似的已知字段；给不出就是 `None`（此时只报"已忽略"，不猜）。
    suggestion: Option<&'static str>,
}

/// 每个进程里已经告过警的 `(配置文件, 字段路径)`。
///
/// 为什么要去重：解析入口会被**重试路径反复调用**（bot 重连、每次 `agent_send` 都重新
/// 解析 provider 配置），同一批 typo 每次都刷一遍会把真正的运行日志淹掉。
///
/// 粒度是"文件 + 字段"而不是"整份文件"：用户改好一个 typo 又写错另一个字段时，新字段
/// 仍然会告警——把那一条也吞掉就会让"改了还是没反应"重现。重启进程后全部重新告警。
static WARNED_UNKNOWN_FIELDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// 允许给建议的最大编辑距离。判据只有两条：距离 ≤ 2，或已知字段是未知键的前缀。
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// 用 [`serde_ignored`] 反序列化：未知字段 → 每个键一条 `tracing::warn!`，**解析照常成功**。
///
/// # 为什么不是 `#[serde(deny_unknown_fields)]`
///
/// `bots.json` / `agent.json` 都是用户手写的，而 JSON 没有注释语法——想留一句说明，
/// 最自然的写法就是加一个 `_说明` 这样的键（演示项目 `projects/qqdemo/bots.json` 里
/// 就是这么写的）。硬拒会把"写一行备注"变成"整个应用拒绝启动"，代价与收益完全不对等。
/// 所以这里选择告警：拼错的字段名会被看见，备注键不挡路。
///
/// `_` 开头的键因此既不告警也不失败，这是**产品决定**，不是漏判。
///
/// # 与既有解析行为的边界
///
/// 只有"多出来的键"走这条路。字段类型错、缺必填、JSON 语法错、文档后面多余的尾随内容
/// 依旧返回 `serde_json::Error`，调用方照旧翻成 -32002——**一条既有行为都没改**。
///
/// 放在 `runtime::config` 而不是各写一份：`agent::config` 只是借用这套机制，schema 与
/// 建议用的字段表留在各自文件里（见 `agent_known_fields`）。
pub(crate) fn deserialize_with_warnings<T, F>(
    source: &Path,
    raw: &str,
    known_fields: F,
) -> Result<T, serde_json::Error>
where
    T: serde::de::DeserializeOwned,
    F: Fn(&str) -> &'static [&'static str],
{
    let mut ignored: Vec<IgnoredField> = Vec::new();
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let parsed: T =
        serde_ignored::deserialize(&mut deserializer, |path: serde_ignored::Path<'_>| {
            let Some(field) = ignored_map_key(&path) else {
                return;
            };
            // 产品决定：`_` 开头是用户备注（JSON 没注释语法），静默放过。
            if field.starts_with('_') {
                return;
            }
            let rendered = render_path(&path);
            let suggestion = closest_known_field(field, known_fields(&rendered));
            ignored.push(IgnoredField {
                path: rendered,
                suggestion,
            });
        })?;
    // `serde_ignored` 拿走了 `serde_json::from_str` 自带的"文档后面不许有多余内容"检查，
    // 这里补回来：否则 `{"bots":[]} 垃圾` 会从报错变成静默通过。
    deserializer.end()?;
    warn_ignored_fields(source, ignored);
    Ok(parsed)
}

/// 被忽略的键一定是 `Path::Map` 的 key（数组下标不是键）。
///
/// 返回的 `&str` 借的是 `path` 自己（`Path::Map.key` 是 `String`，由 `Path` 持有），
/// 不是 `Path<'a>` 里那个 `'a`，所以生命周期必须绑在 `&Path` 上而不是 `'a` 上。
fn ignored_map_key<'p>(path: &'p serde_ignored::Path<'_>) -> Option<&'p str> {
    match path {
        serde_ignored::Path::Map { key, .. } => Some(key.as_str()),
        _ => None,
    }
}

/// 把 [`serde_ignored::Path`] 渲染成用户能对着文件找的写法：`bots[0].clientSecert`。
///
/// 不用它自带的 `Display`：那个把数组下标也用 `.` 连起来（`bots.0.clientSecert`），
/// 用户会以为 key 里真有个点，反而更难定位。
fn render_path(path: &serde_ignored::Path<'_>) -> String {
    match path {
        serde_ignored::Path::Root => String::new(),
        serde_ignored::Path::Seq { parent, index } => {
            format!("{}[{index}]", render_path(parent))
        }
        serde_ignored::Path::Map { parent, key } => match *parent {
            serde_ignored::Path::Root => key.clone(),
            other => format!("{}.{key}", render_path(other)),
        },
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => render_path(parent),
    }
}

/// 去重后把每条被忽略的键打成一条 warn。
fn warn_ignored_fields(source: &Path, ignored: Vec<IgnoredField>) {
    if ignored.is_empty() {
        return;
    }
    let warned = WARNED_UNKNOWN_FIELDS.get_or_init(|| Mutex::new(HashSet::new()));
    let fresh: Vec<IgnoredField> = {
        let mut guard = match warned.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        ignored
            .into_iter()
            .filter(|field| {
                // `\u{1}` 分隔：Windows/POSIX 路径与 JSON 键名里都不可能出现它。
                guard.insert(format!("{}\u{1}{}", source.display(), field.path))
            })
            .collect()
    };
    for field in fresh {
        match field.suggestion {
            Some(suggestion) => tracing::warn!(
                "{} 里有未知字段 `{}`（已忽略，解析继续）：你是不是想写 `{}`？",
                source.display(),
                field.path,
                suggestion
            ),
            None => tracing::warn!(
                "{} 里有未知字段 `{}`（已忽略，解析继续，没有相近的已知字段）",
                source.display(),
                field.path
            ),
        }
    }
}

/// 近似建议：编辑距离 ≤ [`MAX_SUGGESTION_DISTANCE`]，或已知字段是未知键的前缀
/// （`clientSecret` 之于 `clientSecretTypo`）。两条都不满足就返回 `None`。
///
/// **宁可不猜，也不乱猜**：键名都很短（`url` / `id` / `name` 互相只差两三个字符），
/// 阈值放宽到 3 就会开始"建议"用户根本没打算写的字段，比不提示更误导。
///
/// 大小写不敏感（`baseURL` → `baseUrl` 的距离是 0）：写错大小写是最常见的一类 typo。
fn closest_known_field(field: &str, candidates: &[&'static str]) -> Option<&'static str> {
    let needle = field.to_ascii_lowercase();
    let mut best: Option<((usize, usize), &'static str)> = None;
    for &candidate in candidates {
        let lowered = candidate.to_ascii_lowercase();
        let is_prefix = lowered.len() < needle.len() && needle.starts_with(&lowered);
        let distance = edit_distance(&lowered, &needle);
        if !is_prefix && distance > MAX_SUGGESTION_DISTANCE {
            continue;
        }
        // 排序键：距离优先，同距离时前缀命中优先；完全同分则保留字段表里的先后顺序。
        let rank = (distance, usize::from(!is_prefix));
        if best.is_none_or(|(best_rank, _)| rank < best_rank) {
            best = Some((rank, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// 标准 Levenshtein 距离（按 `char` 算，键名里可能有非 ASCII 字符）。
fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current: Vec<usize> = vec![0; right.len() + 1];
    for (row, left_char) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right.iter().enumerate() {
            let substitute = previous[column] + usize::from(left_char != right_char);
            current[column + 1] = substitute
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[derive(Debug, Deserialize)]
struct BotsFile {
    #[serde(default)]
    bots: Vec<BotEntry>,
}

/// **刻意不派生 `Debug`**：`token` / `client_secret` 字段在里面，派生就等于给了任何一句
/// `tracing::warn!(?entry)` 一个泄漏点。手写 `Debug` 把密钥打成 `<redacted>`，
/// 这样 `BotsFile` 的 `#[derive(Debug)]` 仍然成立（错误上下文要用它）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotEntry {
    id: String,
    /// 仅 `onebot11` 需要；`qq_official` 用不到，所以不是必填字段。
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    self_id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    limits: Option<LimitsEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitsEntry {
    #[serde(default)]
    memory_mb: Option<u32>,
    #[serde(default)]
    handler_timeout_ms: Option<u64>,
}

impl std::fmt::Debug for BotEntry {
    /// `token` 与 `client_secret` 恒为 `<redacted>`。没有这一层，`BotsFile` 的 `Debug`
    /// （错误上下文里会用）就是一条把用户密钥写进日志的路。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotEntry")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("app_id", &self.app_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("self_id", &self.self_id)
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("limits", &self.limits)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Debug` 里绝不能出现 token 原文：日志、错误上下文、panic 消息都会用到它。
    #[test]
    fn bot_config_debug_redacts_token() {
        let mut config = BotConfig::new("ws://127.0.0.1:3001");
        config.access_token = Some("sk-live-SUPERSECRET-do-not-print".to_owned());

        let printed = format!("{config:?}");

        assert!(
            !printed.contains("SUPERSECRET"),
            "Debug 输出泄漏了 token：{printed}"
        );
        assert!(
            printed.contains("<redacted>"),
            "Debug 输出应标出存在 token：{printed}"
        );
        assert!(printed.contains("ws://127.0.0.1:3001"));
    }

    /// 没有 token 时也不能凭空长出一个。
    #[test]
    fn bot_config_debug_shows_none_without_token() {
        let config = BotConfig::new("ws://127.0.0.1:3001");
        let printed = format!("{config:?}");
        assert!(printed.contains("access_token: None"), "{printed}");
    }

    /// QQ 官方的 AppSecret 与 OneBot 的 token 是同一档密钥：`Debug` 一起打码。
    #[test]
    fn bot_config_debug_redacts_client_secret() {
        let config = BotConfig::qq("10001", "QQ-SECRET-do-not-print");
        let printed = format!("{config:?}");
        assert!(!printed.contains("QQ-SECRET-do-not-print"), "{printed}");
        assert!(
            printed.contains("client_secret: Some(\"<redacted>\")"),
            "{printed}"
        );
        // appId 不是密钥，允许明文出现在 Debug 里。
        assert!(printed.contains("10001"), "{printed}");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hbot-bots-json-{label}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, body: &str) {
            std::fs::write(bots_file(&self.0), body).expect("write bots.json");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bot_id() -> BotId {
        BotId::from_uuid(
            uuid::Uuid::parse_str("22222222-2222-4222-8222-222222222222").expect("uuid"),
        )
    }

    #[test]
    fn reads_the_matching_entry() {
        let dir = TempDir::new("match");
        dir.write(
            r#"{
                "bots": [
                    {"id": "11111111-1111-4111-8111-111111111111", "url": "ws://other:1"},
                    {"id": "22222222-2222-4222-8222-222222222222", "url": " ws://127.0.0.1:3001 ",
                     "token": "secret", "selfId": 10001,
                     "limits": {"memoryMb": 64, "handlerTimeoutMs": 2500}}
                ]
            }"#,
        );

        let config = read_bot_config(dir.path(), &bot_id()).expect("entry must be found");
        assert_eq!(config.ws_url.as_deref(), Some("ws://127.0.0.1:3001"));
        assert_eq!(config.access_token.as_deref(), Some("secret"));
        assert_eq!(config.self_id, Some(10001));
        assert_eq!(config.effective_platform(), onebot::PLATFORM);
        assert_eq!(config.limits.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(config.limits.handler_timeout, Duration::from_millis(2500));
    }

    #[test]
    fn missing_file_is_invalid_and_names_the_path() {
        let dir = TempDir::new("missing");
        let error = read_bot_config(dir.path(), &bot_id()).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        let message = error.to_string();
        assert!(message.contains("bots.json"), "{message}");
        assert!(
            message.contains("22222222-2222-4222-8222-222222222222"),
            "{message}"
        );
    }

    #[test]
    fn missing_entry_is_invalid() {
        let dir = TempDir::new("no-entry");
        dir.write(r#"{"bots":[{"id":"11111111-1111-4111-8111-111111111111","url":"ws://a:1"}]}"#);
        let error = read_bot_config(dir.path(), &bot_id()).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("没有 id 等于"), "{error}");
    }

    #[test]
    fn blank_url_is_invalid() {
        let dir = TempDir::new("blank-url");
        dir.write(r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"   "}]}"#);
        let error = read_bot_config(dir.path(), &bot_id()).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("url 为空"), "{error}");
    }

    #[test]
    fn broken_json_is_invalid() {
        let dir = TempDir::new("broken");
        dir.write("{ not json");
        let error = read_bot_config(dir.path(), &bot_id()).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        assert!(error.to_string().contains("无法解析"), "{error}");
    }

    /// `qq_official` 的条目不需要 `url`，凭据齐了就能解析。
    #[test]
    fn qq_entry_parses_without_url() {
        let dir = TempDir::new("qq");
        dir.write(
            r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222",
                          "platform":"qq_official","appId":"10001",
                          "clientSecret":"QQ-SECRET"}]}"#,
        );
        let config = read_bot_config(dir.path(), &bot_id()).expect("qq entry must parse");
        assert_eq!(config.ws_url, None);
        assert_eq!(config.effective_platform(), qq::PLATFORM);
        assert_eq!(config.app_id.as_deref(), Some("10001"));
        assert_eq!(config.client_secret.as_deref(), Some("QQ-SECRET"));
        let printed = format!("{config:?}");
        assert!(!printed.contains("QQ-SECRET"), "{printed}");
    }

    /// 未知平台值必须报错并列出允许值，不许静默当 OneBot。
    #[test]
    fn unknown_platform_lists_the_allowed_values() {
        let dir = TempDir::new("unknown-platform");
        dir.write(
            r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222",
                          "platform":"telegram","url":"ws://a:1"}]}"#,
        );
        let error = read_bot_config(dir.path(), &bot_id()).expect_err("must fail");
        assert_eq!(error.code(), -32002);
        let message = error.to_string();
        assert!(message.contains("onebot11"), "{message}");
        assert!(message.contains("qq_official"), "{message}");
    }
}
