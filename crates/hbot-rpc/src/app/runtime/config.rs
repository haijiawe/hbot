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
//!       "platform": "可选：缺省 onebot11",
//!       "limits": { "memoryMb": 32, "handlerTimeoutMs": 2000 }
//!     }
//!   ]
//! }
//! ```
//!
//! 字段说明：
//!
//! * `id`（必填）— 必须是 `bots.id` 那个 UUID 的字符串形式；一个文件可以放多个 bot。
//! * `url`（必填）— OneBot 11 正向 WebSocket 地址，`ws://` 与 `wss://` 都可以。`wss://`
//!   走 rustls + `webpki-roots`，**证书校验始终开启**：证书不匹配报 `AdapterError::Tls`
//!   （wire 上 -32006），不会降级成明文，也**没有**跳过校验的开关（私有 CA / 自签部署用
//!   `OneBotConfig::with_extra_root_certificates` 追加根证书）。
//! * `token`（可选）— 作为 `Authorization: Bearer <token>` 发出；日志里打码。
//! * `selfId`（可选）— bot 自己的 QQ 号；只用于乐观校验收到的事件是不是发给自己的。
//! * `name`（可选）— 展示名；缺省取项目根目录名（`bots.name`）。
//! * `platform`（可选）— 平台标识；M0 只有 `onebot11`，缺省即它。
//! * `limits`（可选）— 交给沙箱的每 isolate 限额，缺省即
//!   [`hbot_code::Limits::default`]（32 MiB / 2 s）。
//!
//! 所有读盘都是阻塞的：调用方负责把它放进 `spawn_blocking` / `AppState::blocking`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use hbot_code::Limits;
use hbot_core::{BotId, HbotError};
use serde::Deserialize;

/// 配置文件名（位于项目根）。
pub const BOTS_FILE: &str = "bots.json";

/// 一个 bot 的连接参数 + 沙箱限额。
///
/// **`Debug` 是手写的**，不派生：`access_token` 一旦被 `tracing::warn!(?config)`
/// 打印出来就会进日志文件。同一仓库的 `OneBotConfig` 早就是这个口径
/// （`crates/hbot-adapters/src/platforms/onebot/client.rs` 的 `config_debug_redacts_token`）。
#[derive(Clone, PartialEq, Eq)]
pub struct BotConfig {
    /// OneBot 11 正向 WebSocket 地址（只接受 `ws://`）。
    pub ws_url: String,
    /// 可选的 access token，按 `Authorization: Bearer` 发送。
    pub access_token: Option<String>,
    /// bot 自己的平台上 id（QQ 号）。
    pub self_id: Option<i64>,
    /// 展示名（`bots.name`）。缺省时调用方用项目目录名兜底。
    pub name: Option<String>,
    /// 平台标识（`bots.platform`）。缺省时调用方填 `onebot11`。
    pub platform: Option<String>,
    /// 交给沙箱的每 isolate 限额。
    pub limits: Limits,
}

impl BotConfig {
    /// 只有地址、其余字段取缺省值。
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            access_token: None,
            self_id: None,
            name: None,
            platform: None,
            limits: Limits::default(),
        }
    }
}

/// [`BOTS_FILE`] 在项目里的完整路径（不保证存在）。
pub fn bots_file(root: &Path) -> PathBuf {
    root.join(BOTS_FILE)
}

impl std::fmt::Debug for BotConfig {
    /// `access_token` 恒为 `<redacted>`；其余字段照常打印。
    ///
    /// 这不是洁癖：`BotConfig` 会被塞进错误上下文与启动日志，派生 `Debug`
    /// 会在任何一次 `tracing::warn!(?config)` 里把 token 写进日志文件。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotConfig")
            .field("ws_url", &self.ws_url)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
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
/// # Errors
///
/// 文件不存在、JSON 不合法、没有该 bot 的条目、`url` 为空——都是 -32002。
pub fn read_bot_config(root: &Path, bot_id: &BotId) -> Result<BotConfig, HbotError> {
    let path = bots_file(root);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        HbotError::invalid(format!(
            "bot {bot_id} 没有可用配置：读不到 {}（{error}）。在项目根建 bots.json，\
             格式见 docs/02-architecture.md 与 crates/hbot-rpc/src/app/runtime/config.rs：\
             {{\"bots\":[{{\"id\":\"{bot_id}\",\"url\":\"ws://127.0.0.1:3001\"}}]}}",
            path.display()
        ))
    })?;

    let parsed: BotsFile = serde_json::from_str(&raw).map_err(|error| {
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

    let ws_url = entry.url.trim().to_owned();
    if ws_url.is_empty() {
        return Err(HbotError::invalid(format!(
            "bot {bot_id} 没有可用配置：{} 里该条目的 url 为空",
            path.display()
        )));
    }

    let limits = match entry.limits {
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
        access_token: entry.token.filter(|token| !token.trim().is_empty()),
        self_id: entry.self_id,
        name: entry
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty()),
        platform: entry
            .platform
            .map(|platform| platform.trim().to_owned())
            .filter(|platform| !platform.is_empty()),
        limits,
    })
}

#[derive(Debug, Deserialize)]
struct BotsFile {
    #[serde(default)]
    bots: Vec<BotEntry>,
}

/// **刻意不派生 `Debug`**：`token` 字段在里面，派生就等于给了任何一句
/// `tracing::warn!(?entry)` 一个泄漏点。手写 `Debug` 把 token 打成 `<redacted>`，
/// 这样 `BotsFile` 的 `#[derive(Debug)]` 仍然成立（错误上下文要用它）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotEntry {
    id: String,
    url: String,
    #[serde(default)]
    token: Option<String>,
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
    /// `token` 恒为 `<redacted>`。没有这一层，`BotsFile` 的 `Debug`（错误上下文里会用）
    /// 就是一条把用户 token 写进日志的路。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BotEntry")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
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
        assert_eq!(config.ws_url, "ws://127.0.0.1:3001");
        assert_eq!(config.access_token.as_deref(), Some("secret"));
        assert_eq!(config.self_id, Some(10001));
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
}
