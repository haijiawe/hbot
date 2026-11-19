//! `hbot.json` — the project manifest, and the skeleton `hbot new` writes.
//!
//! The manifest is the **only** place a project declares what it is and how much
//! sandbox it gets. It is parsed once at start-up / on save, never per message
//! (`docs/02-architecture.md` §6.1), so it can afford to be strict: every struct
//! here is `deny_unknown_fields`, because a typo in `limits.handlerMs` silently
//! doing nothing is worse than a refused start.
//!
//! Limits are **clamped by refusal, not by silence**: a manifest asking for more
//! than the host allows fails to load with [`CodeError::ManifestInvalid`]. The
//! conversion to [`Limits`] additionally clamps, but only as a second line of
//! defence for hand-built [`ManifestLimits`] values — a value that came from
//! [`load`] has already been checked.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use hbot_core::DeviceCapability;

use crate::compile::ENTRY_MODULE;
use crate::dts;
use crate::error::CodeError;
use crate::limits::Limits;

/// Manifest file name in the project root.
pub const MANIFEST_FILE: &str = "hbot.json";

/// Default entry module (`docs/06-ide.md`: 入口由 `hbot.json` 的 `entry` 指定).
pub const DEFAULT_ENTRY: &str = ENTRY_MODULE;

/// Default handler budget, matching [`crate::limits::DEFAULT_HANDLER_TIMEOUT`].
pub const DEFAULT_HANDLER_MS: u64 = 2000;

/// Hard cap for `limits.handlerMs`.
///
/// Equal to the host default on purpose: `docs/02-architecture.md` §6.1 says a
/// project may tune the handler budget **down**, never above what the host is
/// willing to spend on one message. Raising the cap means changing this constant
/// *and* [`Limits::default`], in one commit, with the timeout tests re-measured.
pub const MAX_HANDLER_MS: u64 = 2000;

/// Default heap ceiling in MiB, matching [`crate::limits::DEFAULT_MEMORY_BYTES`].
pub const DEFAULT_HEAP_MB: u64 = 32;

/// Hard cap for `limits.heapMb` (`docs/02-architecture.md` §6.1).
pub const MAX_HEAP_MB: u64 = 64;

/// Reserved bot name used by [`scaffold`] when the caller has no better idea.
pub const FALLBACK_NAME: &str = "my-bot";

/// A parsed, validated `hbot.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Project name, shown in the IDE and used for log files. Required.
    pub name: String,
    /// Entry module, project-relative, `/`-separated. Defaults to `src/main.ts`.
    #[serde(
        default = "default_entry",
        serialize_with = "serialize_entry",
        deserialize_with = "deserialize_entry"
    )]
    pub entry: PathBuf,
    /// Which platform this bot connects to. Required.
    pub platform: PlatformSpec,
    /// Capability grants. Absent means "everything off".
    #[serde(default)]
    pub permissions: Permissions,
    /// Sandbox budget. Absent means [`DEFAULT_HANDLER_MS`] / [`DEFAULT_HEAP_MB`].
    #[serde(default)]
    pub limits: ManifestLimits,
    /// 能力声明（`docs/07-bot-runtime-blueprint.md` §3.2）。缺席 = 全关。
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// 能力声明块。
///
/// **只列已经实现的能力。** 这份结构体带 `deny_unknown_fields`：把尚未实现的
/// 块（`events` / `egress` / `ai`）写进 `hbot.json` 会让项目**加载失败**，报
/// `unknown field`。这是故意的——一个"声明了但什么都不做"的字段会让人以为
/// 能力已经打开，而它其实静默无效，等真去调 `ctx.timer` 才发现没有。宁可当场
/// 拒绝，也不留一个看似生效的开关。
///
/// 与 `permissions` 的分工：`capabilities` 说"我要用哪一类能力"，
/// `permissions` 说"用的时候允许碰哪些具体的对手"（hosts、应用名、设备子能力）。
/// 两层都要过：声明了 `capabilities.state.kv` 不等于能读别人的数据，它只决定
/// `ctx.kv` 这个符号会不会出现在 `hbot.d.ts` 里。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// 状态能力（C2）：`kv` / `history`。
    #[serde(default)]
    pub state: StateCapabilities,
    /// 智能能力（C5）：`ctx.ai.complete`。
    #[serde(default)]
    pub ai: bool,
    /// 事件能力（C1）：`onSchedule(ctx, name)`（旧名 `onTimer`）的具名计划表。
    ///
    /// 与 `ai` / `state.kv` 不同，这里不是布尔而是**数组**：定时器必须有名字与规则，
    /// "true" 无法回答"几点触发"。空数组 = 没声明。
    #[serde(default)]
    pub timer: Vec<hbot_core::ScheduleSpec>,
    /// 后台作业（C4）：`ctx.jobs.enqueue(name, …)` 的具名作业类型表。
    ///
    /// 与 `timer` 同一个形状、同一个理由：作业类型必须有名字，还把预算与重试
    /// 次数一起写在这里，"true" 无法回答"跑多久、失败了还试不试"。
    /// 空数组 = 没声明 —— `ctx.jobs.enqueue` 会直接报 `HBOT_JOBS_NOT_DECLARED`，
    /// 而不是收下一个跑不起来的作业。
    #[serde(default)]
    pub jobs: Vec<hbot_core::JobSpec>,
    /// 出口能力（C3）：`ctx.send(conversationId, text, options?)`。
    ///
    /// 一个布尔就够：发到哪个会话由**调用点**决定（那是数据，不是声明）。
    /// 反过来说明「有没有权力主动发消息」才是清单该回答的问题 —— 它比 `reply`
    /// 大一级：`reply` 只能回到刚刚说话的那个会话，`send` 能开一个新对话。
    #[serde(default)]
    pub send: bool,
    /// 出口能力（C3）：`ctx.notify(text)` 的目标，**钉在清单里**。
    ///
    /// 与 `send` 相反：目标不来自参数，来自声明。这样「运维告警发给值班群」
    /// 这件事在代码里无从写错收件人，也不需要每次调用都查库。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<hbot_core::NotifyTarget>,
    /// 事件能力（C1）：`onWebhook(ctx)` 的入站路由表。
    ///
    /// 与 `timer` / `jobs` 同一个形状：每条路由都要有路径与密钥来源，
    /// "true" 无法回答"从哪条 URL 进、密钥在哪个环境变量"。空数组 = 没声明 ——
    /// 那时任何 `POST /hook/…` 都回 `HBOT_WEBHOOK_NOT_DECLARED`，而不是悄悄收下。
    #[serde(default)]
    pub webhook: Vec<hbot_core::WebhookRoute>,
}

/// 状态能力（`docs/07` §5）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCapabilities {
    /// `ctx.kv.*`：bot 级键值状态，走 SQLite，TTL 可选。
    #[serde(default)]
    pub kv: bool,
    /// `ctx.history(n)`：读本会话最近 n 条（含自己发过的），只读。
    #[serde(default)]
    pub history: bool,
    /// `ctx.sql.*`：每 bot 一套表（`docs/07` §5）。
    ///
    /// 表结构由项目的 `migrations/*.sql` 定义，**运行时禁 DDL** —— 所以这个布尔同时开两个口子：
    /// 启动时执行迁移、以及 handler 里的参数化读写。
    #[serde(default)]
    pub sql: bool,
    /// `ctx.files.*`：读写项目内的文件（`docs/07` §3.6）。
    ///
    /// 读写分开声明：只做导出的项目不该顺手拿到删除权限。路径只能是项目内的相对路径，
    /// 单次读写各有体积上限（见 `hbot_core::bot_file`）。
    #[serde(default)]
    pub files: hbot_core::FilesCapability,
    /// `ctx.session.*`：跨消息的多轮等待（`docs/07` §5.1）。
    ///
    /// 开着它项目才能 `ctx.session.prompt(...)` 注册一条等待、由下一条匹配消息
    /// 唤醒 `onResume`。缺省关：等待会跨重启存活，那是一个需要想清楚的状态。
    #[serde(default)]
    pub session: bool,
}

impl Capabilities {
    /// `ctx.kv` 是否可用（声明层判据；宿主装配再判一道）。
    #[must_use]
    pub fn kv(&self) -> bool {
        self.state.kv
    }

    /// `ctx.history` 是否可用（声明层判据；宿主装配再判一道）。
    #[must_use]
    pub fn history(&self) -> bool {
        self.state.history
    }

    /// `ctx.sql.*` 是否可用（声明层判据；宿主装配再判一道）。
    #[must_use]
    pub fn sql(&self) -> bool {
        self.state.sql
    }
    /// `ctx.files.*` 的声明（read / write 分开）。
    #[must_use]
    pub fn files(&self) -> hbot_core::FilesCapability {
        self.state.files
    }
    /// `ctx.session.*` 是否可用（声明层判据；宿主装配再判一道）。
    #[must_use]
    pub fn session(&self) -> bool {
        self.state.session
    }

    /// `ctx.ai.complete` 是否可用（声明层判据；宿主装配再判一道）。
    ///
    /// 注意这一层只回答"项目声明了要用模型"，不回答"宿主配了模型"：
    /// 没配就抛 `HBOT_AI_UNAVAILABLE`，两者分开报是为了让排障方向不同
    /// （改清单 还是 去设置里填 key）。
    #[must_use]
    pub fn ai(&self) -> bool {
        self.ai
    }

    /// 定时器计划表（`onTimer` 的 `name` 从这里来）。
    ///
    /// 返回空切片表示"没声明"——与 `state.kv` 那个布尔同一个语义：声明层只回答
    /// "项目要不要用定时器"，不回答"宿主能不能调度"（后者由 `TimerSlot` 装配回答）。
    #[must_use]
    pub fn timer(&self) -> &[hbot_core::ScheduleSpec] {
        &self.timer
    }

    /// 作业类型表（`ctx.jobs.enqueue` 的 `name` 从这里来）。
    ///
    /// 与 [`Self::timer`] 同一条分工：声明层只回答"项目要不要用后台作业"，
    /// 不回答"宿主能不能跑"（后者由 `hbot-code::jobs::JobSlot` 的装配回答，
    /// 再后面由 `hbot-rpc` 的作业循环回答）。
    #[must_use]
    pub fn jobs(&self) -> &[hbot_core::JobSpec] {
        &self.jobs
    }

    /// webhook 路由表（`onWebhook` 的路径与密钥来源从这里来）。
    ///
    /// 返回空切片表示"没声明"，与 `Self::timer` 同一语义：声明层只回答"项目要不要
    /// 被外部系统调用"，不回答"服务有没有在听"（后者由 `hbot-rpc` 的 `WebhookServer` 回答）。
    #[must_use]
    pub fn webhook(&self) -> &[hbot_core::WebhookRoute] {
        &self.webhook
    }

    /// `ctx.send` 是否可用（声明层判据；宿主装配再判一道）。
    #[must_use]
    pub fn send(&self) -> bool {
        self.send
    }

    /// `ctx.notify` 的目标（`None` = 没声明这个能力）。
    ///
    /// 返回的是引用：目标里有会话 ID，拷贝出去只会让两处各自演化。
    #[must_use]
    pub fn notify(&self) -> Option<&hbot_core::NotifyTarget> {
        self.notify.as_ref()
    }
}

/// The platform identifier that needs a forward WebSocket address (`ws`).
///
/// The manifest does **not** enumerate platforms: `kind` is an open string,
/// because "which platforms exist" is product knowledge, not core knowledge.
/// The one exception is this spelling, which the `ws` requirement is keyed on.
pub const ONE_BOT_KIND: &str = "onebot11";

/// 本地试跑平台的标识（`hbot-rpc` 的 `local` 适配器、示例项目与端到端测试都用它）。
///
/// 与 [`ONE_BOT_KIND`] 一样只是**一个拼写**，不是平台白名单：`hbot-code` 依旧不
/// 枚举平台，认识哪些平台由入口（`hbot check`、`bots.json` 的校验）自己决定。
pub const LOCAL_KIND: &str = "local";

/// The platform block of the manifest.
///
/// `kind` is deliberately a **string, not an enum**: adding a platform must not
/// require editing `hbot-code` (that would invert the dependency direction —
/// the sandbox core would have to know every platform product ever ships).
/// A platform identifier is metadata here, carried through untouched; which
/// values this build recognises is decided in `hbot check`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformSpec {
    /// Platform identifier, spelled the same as `bots.json`'s `platform`
    /// field: `"onebot11"` / `"qq_official"`.
    ///
    /// Not validated here — an unknown value parses fine, and `hbot check`
    /// reports it as a warning.
    pub kind: String,
    /// OneBot 11's forward WebSocket endpoint, e.g. `ws://127.0.0.1:3001`.
    ///
    /// **Only `onebot11` needs it.** QQ official has no address to put here:
    /// its gateway URL is discovered through the `/gateway` API. A `ws` on any
    /// other platform is tolerated (a hand-written draft must not be refused)
    /// and `hbot check` says it will be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
}

/// Capability grants. An absent field means "not granted".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Hosts `http.get` may reach. Empty = no network at all.
    ///
    /// Read by [`crate::permissions::authorize`] through
    /// [`crate::HttpConfig::from_manifest`]; the semantics of one entry (exact
    /// host, `*.subdomain`, `host:port`, `*`) are documented on
    /// [`crate::permissions`].
    #[serde(default)]
    pub http: Vec<String>,

    /// 设备能力（C6）。`None` = 没有任何设备能力，`ctx.device` 上不会长出符号。
    ///
    /// **这只是第一层闸**：清单声明 ≠ 被允许。真正的放行还要宿主设置
    /// （`<data_dir>/device.json`）与一个限时设备会话，三层缺一层就拒。
    /// 见 `docs/09-device-capability.md` §4。
    #[serde(default)]
    pub device: Option<DevicePermissions>,
}

/// 清单里的设备授权声明（第一层闸）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePermissions {
    /// 声明的子能力（`screen` / `ocr` / `input` / `window` / `clipboard`）。
    ///
    /// 空数组 = 声明了 `device` 块但没要任何子能力 —— 与 `None` 等价，但保留它
    /// 是为了让"我以后要加"有地方写，也为了让 `deny_unknown_fields` 能抓拼写错。
    /// 不认识的名字在这里就报错，不留到运行时。
    #[serde(default)]
    pub sub: Vec<DeviceCapability>,

    /// 请求的应用白名单（进程映像名，大小写不敏感）。
    ///
    /// 这是**提议**：宿主设置里也会有一份，两者的交集才生效
    /// （Agent 不能靠改清单给自己加应用）。
    #[serde(default)]
    pub apps: Vec<String>,
}

/// The `limits` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestLimits {
    /// Handler wall-clock budget in milliseconds.
    #[serde(default = "default_handler_ms", rename = "handlerMs")]
    pub handler_ms: u64,
    /// Isolate heap ceiling in MiB.
    #[serde(default = "default_heap_mb", rename = "heapMb")]
    pub heap_mb: u64,
}

impl Default for ManifestLimits {
    fn default() -> Self {
        Self {
            handler_ms: DEFAULT_HANDLER_MS,
            heap_mb: DEFAULT_HEAP_MB,
        }
    }
}

/// What [`scaffold`] created, relative to the project directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaffoldReport {
    /// The project directory.
    pub dir: PathBuf,
    /// Files created, project-relative and `/`-separated, in creation order.
    pub files: Vec<String>,
}

fn default_entry() -> PathBuf {
    PathBuf::from(DEFAULT_ENTRY)
}

const fn default_handler_ms() -> u64 {
    DEFAULT_HANDLER_MS
}

const fn default_heap_mb() -> u64 {
    DEFAULT_HEAP_MB
}

/// Render a path the way the manifest spells it: `/` separators, no `\\`.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn serialize_entry<S: serde::Serializer>(entry: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path_text(entry))
}

fn deserialize_entry<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<PathBuf, D::Error> {
    let raw = String::deserialize(deserializer)?;
    Ok(PathBuf::from(raw))
}

impl Manifest {
    /// The entry module as a `/`-separated project-relative module name.
    ///
    /// This is what [`crate::compile::Compiler::compile_project_with_entry`] and
    /// the QuickJS resolver use as a module id.
    #[must_use]
    pub fn entry_name(&self) -> String {
        path_text(&self.entry)
    }

    /// The sandbox budget this project asked for, validated against the host
    /// caps.
    #[must_use]
    pub fn sandbox_limits(&self) -> Limits {
        Limits::from(&self.limits)
    }

    /// Check the parsed manifest against the project directory.
    fn validate(&self, dir: &Path) -> Result<(), CodeError> {
        let manifest_path = dir.join(MANIFEST_FILE);

        if self.name.trim().is_empty() {
            return Err(invalid(&manifest_path, "`name` 不能为空"));
        }

        // The platform block: `kind` is open metadata (any spelling parses),
        // but the `ws` contract is keyed on it. Only `onebot11` connects over a
        // forward WebSocket; every other platform discovers its endpoint (QQ
        // official does it through `/gateway`), so a `ws` there is a draft
        // leftover to be ignored, not an error.
        if self.platform.kind.trim().is_empty() {
            return Err(invalid(&manifest_path, "`platform.kind` 不能为空"));
        }
        if self.platform.kind == ONE_BOT_KIND {
            let ws = self.platform.ws.as_deref().unwrap_or_default();
            if ws.trim().is_empty() {
                return Err(invalid(
                    &manifest_path,
                    "`platform.kind` 为 \"onebot11\" 时 `platform.ws` 必填且不能为空\
                     （OneBot 11 通过正向 WS 连接）",
                ));
            }
            if !ws.starts_with("ws://") && !ws.starts_with("wss://") {
                return Err(invalid(
                    &manifest_path,
                    format!("`platform.ws` 必须是 ws:// 或 wss:// 开头，收到 '{ws}'"),
                ));
            }
        }

        self.limits.validate(&manifest_path)?;

        // 定时器计划：声明写错在**加载时**就报，不等到第一次触发才静默不动。
        // 校验逻辑只有一份，在 `hbot-core::bot_timer::validate_schedules`。
        hbot_core::validate_schedules(&self.capabilities.timer).map_err(|error| {
            invalid(
                &manifest_path,
                format!("`capabilities.timer` 不合法：{error}"),
            )
        })?;

        // 作业类型表：与定时器同一个判据顺序、同一个理由（写错就在加载时报，
        // 不等到第一次入队才静默不动）。校验只有一份，在
        // `hbot-core::bot_jobs::validate_specs`。
        // 通知目标在加载时就校验，不留到第一次 `ctx.notify`：清单写错类型
        // 的后果是发给另一个人，那不该在半夜第一次告警时才暴露。
        if let Some(target) = &self.capabilities.notify {
            hbot_core::validate_notify_target(target).map_err(|error| {
                crate::error::CodeError::ManifestInvalid {
                    path: manifest_path.to_path_buf(),
                    message: format!("`capabilities.notify` 不合法：{error}"),
                }
            })?;
        }
        hbot_core::validate_specs(&self.capabilities.jobs).map_err(|error| {
            invalid(
                &manifest_path,
                format!("`capabilities.jobs` 不合法：{error}"),
            )
        })?;

        // webhook 路由：路径、密钥环境变量名、重复、数量全在这一份判据里
        // （`hbot-core::bot_webhook::validate_routes`）。写错就在加载时报 —— 等到第一次
        // 请求才发现，症状是「外部系统一直 401」或「一直 404」，两种都指向错误的方向。
        hbot_core::validate_routes(&self.capabilities.webhook).map_err(|error| {
            invalid(
                &manifest_path,
                format!("`capabilities.webhook` 不合法：{error}"),
            )
        })?;

        // The entry is a module id inside the project, never a path on the host.
        // Rejecting components rather than canonicalizing keeps this a pure
        // function of the manifest text (no filesystem race, testable anywhere).
        if self.entry.as_os_str().is_empty() {
            return Err(invalid(&manifest_path, "`entry` 不能为空"));
        }
        for component in self.entry.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(invalid(
                        &manifest_path,
                        format!("`entry` 不允许 `..`，收到 '{}'", self.entry_name()),
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid(
                        &manifest_path,
                        format!("`entry` 必须是项目内相对路径，收到 '{}'", self.entry_name()),
                    ));
                }
            }
        }
        let entry_path = dir.join(&self.entry);
        if !entry_path.is_file() {
            return Err(invalid(
                &manifest_path,
                format!("`entry` 指向的文件不存在：{}", self.entry_name()),
            ));
        }

        Ok(())
    }
}

impl ManifestLimits {
    fn validate(&self, manifest_path: &Path) -> Result<(), CodeError> {
        if self.handler_ms == 0 || self.handler_ms > MAX_HANDLER_MS {
            return Err(invalid(
                manifest_path,
                format!(
                    "`limits.handlerMs` 必须在 1..={MAX_HANDLER_MS} 之间，收到 {}；\
                     超过宿主硬上限直接报错，不做静默压缩",
                    self.handler_ms
                ),
            ));
        }
        if self.heap_mb == 0 || self.heap_mb > MAX_HEAP_MB {
            return Err(invalid(
                manifest_path,
                format!(
                    "`limits.heapMb` 必须在 1..={MAX_HEAP_MB} 之间，收到 {}；\
                     超过宿主硬上限直接报错，不做静默压缩",
                    self.heap_mb
                ),
            ));
        }
        Ok(())
    }
}

impl From<&ManifestLimits> for Limits {
    /// Map manifest limits onto the sandbox budget.
    ///
    /// Clamps as a last resort: values that went through [`load`] are already
    /// range-checked, so the clamp only matters for a `ManifestLimits` built by
    /// hand (tests, future callers). Clamping here is strictly safer than
    /// trusting the caller.
    fn from(limits: &ManifestLimits) -> Self {
        let handler_ms = limits.handler_ms.clamp(1, MAX_HANDLER_MS);
        let heap_mb = limits.heap_mb.clamp(1, MAX_HEAP_MB);
        let memory_bytes = usize::try_from(heap_mb.saturating_mul(1024 * 1024))
            .unwrap_or(Limits::default().memory_bytes);
        Limits::new()
            .with_handler_timeout(Duration::from_millis(handler_ms))
            .with_memory_bytes(memory_bytes)
    }
}

impl From<ManifestLimits> for Limits {
    fn from(limits: ManifestLimits) -> Self {
        Self::from(&limits)
    }
}

fn invalid(path: &Path, message: impl Into<String>) -> CodeError {
    CodeError::ManifestInvalid {
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Read and validate `<dir>/hbot.json`.
///
/// # Errors
///
/// * [`CodeError::ManifestMissing`] — no manifest in `dir`;
/// * [`CodeError::ManifestSyntax`] — not valid JSON, with line/column;
/// * [`CodeError::ManifestInvalid`] — valid JSON that breaks the contract;
/// * [`CodeError::Io`] — the file exists but cannot be read.
pub fn load(dir: &Path) -> Result<Manifest, CodeError> {
    let path = dir.join(MANIFEST_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CodeError::ManifestMissing { path });
        }
        Err(source) => return Err(CodeError::Io { path, source }),
    };

    let manifest: Manifest = serde_json::from_str(&text).map_err(|error| {
        let line = u32::try_from(error.line()).unwrap_or(0);
        let column = u32::try_from(error.column()).unwrap_or(0);
        if error.is_data() {
            // Syntactically fine, semantically wrong (unknown field, missing
            // field, wrong type): report it as a contract violation. serde's
            // message already names the field.
            CodeError::ManifestInvalid {
                path: path.clone(),
                message: format!("{error}（第 {line} 行第 {column} 列）"),
            }
        } else {
            CodeError::ManifestSyntax {
                path: path.clone(),
                line,
                column,
                message: error.to_string(),
            }
        }
    })?;

    manifest.validate(dir)?;
    Ok(manifest)
}

/// Create a minimal, compilable project skeleton in `dir`.
///
/// Writes: `hbot.json`, `tsconfig.json`, `hbot.d.ts`, `AGENTS.md`, `.gitignore`,
/// `src/main.ts` — in that order, so a failure part-way can be rolled back by
/// deleting exactly what was written.
///
/// **Never overwrites.** If any target already exists the whole call fails with
/// [`CodeError::Scaffold`] and nothing is written: a half-scaffolded project is
/// worse than a refusal, and "merge into an existing project" is a separate
/// feature (`hbot new --force` would be an explicit future CLI flag).
///
/// The generated manifest is an **OneBot 11** project (`platform.kind` =
/// `"onebot11"`, placeholder `ws`) because that is the reasonable default for a
/// new bot. To make a QQ official project instead, change `platform.kind` to
/// `"qq_official"` and delete `ws` — the connection settings (`appId` /
/// `clientSecret`) live in `bots.json`, not in this file.
///
/// # Errors
///
/// [`CodeError::Scaffold`] when a target exists or writing fails.
pub fn scaffold(dir: &Path, name: &str) -> Result<ScaffoldReport, CodeError> {
    let name = if name.trim().is_empty() {
        FALLBACK_NAME
    } else {
        name.trim()
    };

    let files: [(&str, String); 6] = [
        (MANIFEST_FILE, manifest_skeleton(name)?),
        ("tsconfig.json", TSCONFIG.to_owned()),
        (dts::HOST_DTS_FILE, dts::host_dts().to_owned()),
        ("AGENTS.md", agents_skeleton().to_owned()),
        (".gitignore", GITIGNORE.to_owned()),
        (ENTRY_MODULE, main_skeleton().to_owned()),
    ];

    // Phase 1: refuse before touching the disk.
    for (relative, _) in &files {
        let target = dir.join(relative);
        if target.exists() {
            return Err(CodeError::Scaffold {
                path: target,
                message: "目标已存在，拒绝覆盖；请换一个空目录，或先手工删除该文件".to_owned(),
            });
        }
    }

    // Phase 2: create, remembering what we made so a failure can be undone.
    let mut created_files: Vec<String> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for directory in [dir.to_path_buf(), dir.join("src")] {
        if directory.exists() {
            continue;
        }
        // `dir` may be nested (`hbot new ~/bots/echo`), so parents are created
        // too. Only the two levels this function owns are remembered for
        // rollback: deleting an ancestor the user already had would be worse than
        // leaving an empty parent directory behind after a failed write.
        let result = if directory == dir {
            fs::create_dir_all(&directory)
        } else {
            fs::create_dir(&directory)
        };
        if let Err(source) = result {
            rollback(dir, &created_files, &created_dirs);
            return Err(CodeError::Scaffold {
                path: directory,
                message: format!("创建目录失败：{source}"),
            });
        }
        created_dirs.push(directory);
    }

    for (relative, contents) in &files {
        let target = dir.join(relative);
        match fs::write(&target, contents) {
            Ok(()) => created_files.push((*relative).to_owned()),
            Err(source) => {
                rollback(dir, &created_files, &created_dirs);
                return Err(CodeError::Scaffold {
                    path: target,
                    message: format!("写入失败：{source}"),
                });
            }
        }
    }

    Ok(ScaffoldReport {
        dir: dir.to_owned(),
        files: created_files,
    })
}

/// Delete what a failed [`scaffold`] managed to create, best effort.
///
/// `AGENTS.md` forbids empty `catch`: a failed rollback is reported, not ignored.
fn rollback(dir: &Path, files: &[String], dirs: &[PathBuf]) {
    for relative in files.iter().rev() {
        let path = dir.join(relative);
        if let Err(error) = fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "scaffold 回滚未能删除文件");
        }
    }
    for directory in dirs.iter().rev() {
        if let Err(error) = fs::remove_dir(directory) {
            tracing::warn!(path = %directory.display(), %error, "scaffold 回滚未能删除目录");
        }
    }
}

fn manifest_skeleton(name: &str) -> Result<String, CodeError> {
    let manifest = Manifest {
        name: name.to_owned(),
        entry: default_entry(),
        platform: PlatformSpec {
            kind: ONE_BOT_KIND.to_owned(),
            ws: Some("ws://127.0.0.1:3001".to_owned()),
        },
        permissions: Permissions::default(),
        limits: ManifestLimits::default(),
        capabilities: Capabilities::default(),
    };
    let mut text =
        serde_json::to_string_pretty(&manifest).map_err(|error| CodeError::Scaffold {
            path: PathBuf::from(MANIFEST_FILE),
            message: format!("序列化清单失败：{error}"),
        })?;
    text.push('\n');
    Ok(text)
}

fn agents_skeleton() -> &'static str {
    r#"# 项目约定（给人和 Agent 看）

- 入口：`src/main.ts`，`export default defineBot({ onMessage })`。
- 依赖：只能相对 import（`./lib`、`../lib/x`）；裸包名只有 `'hbot'` 合法。
- 没有 fs / net / process / require / eval / 动态 import()。
- 类型：`hbot.d.ts` 由核心生成，**不要手改**；改了 `hbot check` 会报不一致。
- 预算：`hbot.json` 的 `limits` 只能调低，不能超过宿主硬上限。
- 出口：所有副作用都经 `ctx.reply` / `ctx.log`，没有隐式 IO。
"#
}

const GITIGNORE: &str = ".hbot/\nnode_modules/\n";

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "strict": true,
    "noEmit": true,
    "types": [],
    "paths": {}
  },
  "include": ["src/**/*.ts", "hbot.d.ts"]
}
"#;

fn main_skeleton() -> &'static str {
    r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`收到：${ctx.text}`);
  },
});
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory per test, removed by [`Scratch`]'s `Drop`.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hbot-manifest-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ));
            fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(path, contents).expect("写文件");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    const MINIMAL: &str = r#"{
      "name": "demo",
      "platform": { "kind": "onebot11", "ws": "ws://127.0.0.1:3001" }
    }"#;

    #[test]
    fn defaults_are_applied_and_pinned_to_the_runtime_constants() {
        let scratch = Scratch::new("defaults");
        scratch.write(MANIFEST_FILE, MINIMAL);
        scratch.write(ENTRY_MODULE, "export default {};\n");

        let manifest = load(&scratch.0).expect("应能加载");
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.entry_name(), "src/main.ts");
        assert_eq!(manifest.platform.kind, ONE_BOT_KIND);
        assert_eq!(
            manifest.platform.ws.as_deref(),
            Some("ws://127.0.0.1:3001"),
            "onebot11 的 ws 要读进来"
        );
        assert!(manifest.permissions.http.is_empty(), "默认全禁");
        assert_eq!(manifest.limits, ManifestLimits::default());

        // The manifest defaults must track `Limits::default()`, otherwise a
        // project that says nothing gets a different budget than the docs say.
        let limits = manifest.sandbox_limits();
        assert_eq!(limits, Limits::default());
        assert_eq!(limits.handler_timeout_ms(), DEFAULT_HANDLER_MS);
        assert_eq!(
            u64::try_from(limits.memory_bytes).unwrap_or(0),
            DEFAULT_HEAP_MB * 1024 * 1024
        );
        assert_eq!(MAX_HANDLER_MS, Limits::default().handler_timeout_ms());
    }

    #[test]
    fn entry_must_be_inside_the_project_and_exist() {
        let scratch = Scratch::new("entry");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        for bad in ["../out.ts", "/etc/passwd", "C:/x.ts", "src/missing.ts"] {
            scratch.write(
                MANIFEST_FILE,
                &format!(
                    r#"{{"name":"d","entry":"{bad}","platform":{{"kind":"onebot11","ws":"ws://h"}}}}"#
                ),
            );
            let error = load(&scratch.0).expect_err("应拒绝");
            assert_eq!(error.code(), "HBOT_MANIFEST_INVALID", "{bad}: {error}");
        }

        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","entry":"src/main.ts","platform":{"kind":"onebot11","ws":"ws://h"}}"#,
        );
        assert_eq!(
            load(&scratch.0).expect("相对入口应通过").entry_name(),
            "src/main.ts"
        );
    }

    #[test]
    fn unknown_fields_are_rejected_rather_than_ignored() {
        let scratch = Scratch::new("unknown");
        scratch.write(ENTRY_MODULE, "export default {};\n");
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","pltaform":{"kind":"onebot11","ws":"ws://h"},
                "platform":{"kind":"onebot11","ws":"ws://h"}}"#,
        );
        let error = load(&scratch.0).expect_err("拼错的字段必须报错");
        assert_eq!(error.code(), "HBOT_MANIFEST_INVALID");
        assert!(
            error.to_string().contains("unknown field"),
            "应指出未知字段：{error}"
        );

        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"ws://h","extra":1}}"#,
        );
        assert_eq!(
            load(&scratch.0).expect_err("嵌套未知字段也要报错").code(),
            "HBOT_MANIFEST_INVALID"
        );
    }

    #[test]
    fn syntax_errors_carry_a_json_position() {
        let scratch = Scratch::new("syntax");
        scratch.write(MANIFEST_FILE, "{\n  \"name\": \"d\",\n  bad\n}\n");

        let error = load(&scratch.0).expect_err("应报语法错");
        assert_eq!(error.code(), "HBOT_MANIFEST_INVALID");
        let position = error.position().expect("语法错要带行列");
        assert!(position.line >= 3, "位置应指向第 3 行，收到 {position}");
        assert!(error.to_string().contains("hbot.json:"), "{error}");
    }

    #[test]
    fn a_missing_manifest_has_its_own_code() {
        let scratch = Scratch::new("missing");
        let error = load(&scratch.0).expect_err("应报缺清单");
        assert_eq!(error.code(), "HBOT_MANIFEST_MISSING");
        assert!(error.to_string().contains("hbot.json"), "{error}");
    }

    #[test]
    fn limits_above_the_host_caps_are_refused_not_clamped() {
        let scratch = Scratch::new("caps");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        for (key, value, cap) in [
            ("handlerMs", MAX_HANDLER_MS + 1, MAX_HANDLER_MS),
            ("heapMb", MAX_HEAP_MB + 1, MAX_HEAP_MB),
            ("handlerMs", 0, MAX_HANDLER_MS),
            ("heapMb", 0, MAX_HEAP_MB),
        ] {
            scratch.write(
                MANIFEST_FILE,
                &format!(
                    r#"{{"name":"d","platform":{{"kind":"onebot11","ws":"ws://h"}},
                        "limits":{{"{key}":{value}}}}}"#
                ),
            );
            let error = load(&scratch.0).expect_err("超上限必须报错");
            assert_eq!(
                error.code(),
                "HBOT_MANIFEST_INVALID",
                "{key}={value}: {error}"
            );
            assert!(error.to_string().contains(key), "报错要点名字段：{error}");
            assert!(
                error.to_string().contains(&cap.to_string()),
                "报错要给出硬上限 {cap}：{error}"
            );
        }

        // Lowering is fine and takes effect.
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"ws://h"},
                "limits":{"handlerMs":250,"heapMb":8}}"#,
        );
        let limits = load(&scratch.0).expect("调低应通过").sandbox_limits();
        assert_eq!(limits.handler_timeout_ms(), 250);
        assert_eq!(limits.memory_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn the_conversion_clamps_a_hand_built_value() {
        // Defence in depth: `load` never produces an out-of-range value, but a
        // caller can build one.
        let limits = Limits::from(ManifestLimits {
            handler_ms: u64::MAX,
            heap_mb: u64::MAX,
        });
        assert_eq!(limits.handler_timeout_ms(), MAX_HANDLER_MS);
        assert_eq!(
            u64::try_from(limits.memory_bytes).unwrap_or(u64::MAX),
            MAX_HEAP_MB * 1024 * 1024
        );
    }

    /// `qq_official` has no forward WS address: its gateway is discovered
    /// through the `/gateway` API. A manifest that only says the platform must
    /// therefore parse — this is the whole point of opening `kind` up.
    #[test]
    fn qq_official_parses_without_a_websocket_url() {
        let scratch = Scratch::new("qq-official");
        scratch.write(ENTRY_MODULE, "export default {};\n");
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"qq_official"}}"#,
        );

        let manifest = load(&scratch.0).expect("QQ 官方没有 ws 也必须能解析");
        assert_eq!(manifest.platform.kind, "qq_official");
        assert_eq!(manifest.platform.ws, None, "没有 ws 就是 None");

        // A hand-written draft may still carry a `ws`; it is ignored downstream,
        // not a parse/validate error. `hbot check` is what warns about it.
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"qq_official","ws":"ws://h"}}"#,
        );
        let manifest = load(&scratch.0).expect("其它平台带 ws 不报错");
        assert_eq!(manifest.platform.ws.as_deref(), Some("ws://h"));
    }

    /// The `ws` requirement is keyed on `onebot11` only, and it is a *validation*
    /// error (not a deserialization one), so the message has to name `ws`.
    #[test]
    fn onebot11_requires_a_websocket_url() {
        let scratch = Scratch::new("onebot11-no-ws");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        for manifest_text in [
            r#"{"name":"d","platform":{"kind":"onebot11"}}"#,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":null}}"#,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":""}}"#,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"   "}}"#,
        ] {
            scratch.write(MANIFEST_FILE, manifest_text);
            let error = load(&scratch.0).expect_err("onebot11 缺 ws 必须拒绝");
            assert_eq!(error.code(), "HBOT_MANIFEST_INVALID", "{manifest_text}");
            assert!(
                error.to_string().contains("ws"),
                "报错要点名字段 ws：{error}"
            );
        }
    }

    #[test]
    fn onebot11_with_a_websocket_url_loads() {
        let scratch = Scratch::new("onebot11-with-ws");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        for ws in ["ws://x", "wss://x"] {
            scratch.write(
                MANIFEST_FILE,
                &format!(r#"{{"name":"d","platform":{{"kind":"onebot11","ws":"{ws}"}}}}"#),
            );
            let manifest = load(&scratch.0).expect("onebot11 + ws 应通过");
            assert_eq!(manifest.platform.kind, ONE_BOT_KIND);
            assert_eq!(manifest.platform.ws.as_deref(), Some(ws));
        }
    }

    /// `kind` carries no closed value set: it is metadata for `hbot check`, not
    /// something the sandbox core validates. Unknown fields are still refused —
    /// opening the *value* must not loosen the *shape*.
    #[test]
    fn unknown_platform_kinds_are_metadata_not_errors() {
        let scratch = Scratch::new("platform");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        // `telegram` is not a platform this build knows, but core must parse it.
        for manifest_text in [
            r#"{"name":"d","platform":{"kind":"telegram","ws":"ws://h"}}"#,
            r#"{"name":"d","platform":{"kind":"telegram"}}"#,
        ] {
            scratch.write(MANIFEST_FILE, manifest_text);
            let manifest = load(&scratch.0).expect("取值开放，不认识也应解析");
            assert_eq!(manifest.platform.kind, "telegram");
        }

        // The ws scheme is only checked where `ws` means something.
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"http://h"}}"#,
        );
        assert_eq!(
            load(&scratch.0).expect_err("ws 必须是 websocket").code(),
            "HBOT_MANIFEST_INVALID"
        );

        // An empty platform identifier is still nonsense, not a platform.
        for manifest_text in [
            r#"{"name":"d","platform":{"kind":""}}"#,
            r#"{"name":"d","platform":{"kind":"   "}}"#,
        ] {
            scratch.write(MANIFEST_FILE, manifest_text);
            let error = load(&scratch.0).expect_err("空 kind 要拒绝");
            assert_eq!(error.code(), "HBOT_MANIFEST_INVALID");
            assert!(error.to_string().contains("kind"), "{error}");
        }

        // Unknown *fields* inside `platform` stay refused (deny_unknown_fields).
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"qq_official","extra":1}}"#,
        );
        let error = load(&scratch.0).expect_err("未知字段仍要报错");
        assert_eq!(error.code(), "HBOT_MANIFEST_INVALID");
        assert!(error.to_string().contains("unknown field"), "{error}");

        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"  ","platform":{"kind":"onebot11","ws":"ws://h"}}"#,
        );
        assert_eq!(
            load(&scratch.0).expect_err("空名字要拒绝").code(),
            "HBOT_MANIFEST_INVALID"
        );
    }

    #[test]
    fn scaffold_writes_a_project_that_compiles() {
        let scratch = Scratch::new("scaffold");
        let report = scaffold(&scratch.0, "demo").expect("应能脚手架");

        assert_eq!(
            report.files,
            vec![
                "hbot.json".to_owned(),
                "tsconfig.json".to_owned(),
                "hbot.d.ts".to_owned(),
                "AGENTS.md".to_owned(),
                ".gitignore".to_owned(),
                "src/main.ts".to_owned(),
            ]
        );

        // The generated manifest must load under the same rules as a hand-written
        // one...
        let manifest = load(&scratch.0).expect("骨架清单应合法");
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.entry_name(), "src/main.ts");
        // Scaffold still starts a project on OneBot 11, with a placeholder ws.
        assert_eq!(manifest.platform.kind, ONE_BOT_KIND);
        assert!(
            manifest
                .platform
                .ws
                .as_deref()
                .is_some_and(|ws| ws.starts_with("ws://")),
            "骨架要带占位 ws，收到 {:?}",
            manifest.platform.ws
        );

        // ...the generated d.ts must be the current one...
        assert!(dts::is_host_dts_current(&scratch.0).expect("可读"));

        // ...and the generated code must actually compile, entry and all.
        let project =
            crate::compile::compile_project_with_entry(&scratch.0, &manifest.entry_name())
                .expect("骨架必须能编译");
        assert!(project.contains(ENTRY_MODULE));
        assert_eq!(project.entry(), "src/main.ts");
        assert!(
            project.module(ENTRY_MODULE).is_some_and(|module| {
                module.js.contains("defineBot") && module.js.contains("onMessage")
            }),
            "strip 后的产物应保留 defineBot 调用"
        );
    }

    #[test]
    fn scaffold_never_overwrites() {
        let scratch = Scratch::new("no-overwrite");
        fs::write(scratch.0.join("tsconfig.json"), "{}\n").expect("预置文件");

        let error = scaffold(&scratch.0, "demo").expect_err("必须拒绝");
        assert_eq!(error.code(), "HBOT_SCAFFOLD");
        assert!(error.to_string().contains("tsconfig.json"), "{error}");

        // Nothing was written before the refusal: no partial project.
        assert!(!scratch.0.join("hbot.json").exists(), "不应写半个项目");
        assert!(!scratch.0.join("src").exists(), "不应建 src");
        assert_eq!(
            fs::read_to_string(scratch.0.join("tsconfig.json")).expect("仍在"),
            "{}\n",
            "既有文件不能被改"
        );
    }

    #[test]
    fn declared_job_types_carry_their_budget_and_retries() {
        let scratch = Scratch::new("jobs-declared");
        scratch.write(ENTRY_MODULE, "export default {};\n");
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"ws://h"},
               "capabilities":{"jobs":[
                 {"name":"settle-war","budgetMs":120000,"retries":2},
                 {"name":"fan-out"}
               ]}}"#,
        );

        let manifest = load(&scratch.0).expect("应能加载");
        let jobs = manifest.capabilities.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name, "settle-war");
        assert_eq!(jobs[0].budget_ms, 120_000);
        assert_eq!(jobs[0].retries, 2);
        // 省略的字段走契约缺省值，不是 0 预算；否则每种作业都得把 60000 抄一遍。
        assert_eq!(jobs[1].budget_ms, hbot_core::DEFAULT_JOB_BUDGET_MS);
        assert_eq!(jobs[1].retries, hbot_core::DEFAULT_JOB_RETRIES);
    }

    #[test]
    fn a_job_type_table_that_breaks_the_contract_fails_the_load() {
        let scratch = Scratch::new("jobs-invalid");
        scratch.write(ENTRY_MODULE, "export default {};\n");

        // 预算超上限：加载就报，不等到第一次入队。
        let too_long = format!(
            r#"{{"name":"d","platform":{{"kind":"onebot11","ws":"ws://h"}},
               "capabilities":{{"jobs":[{{"name":"settle-war","budgetMs":{}}}]}}}}"#,
            hbot_core::MAX_JOB_BUDGET_MS + 1
        );
        scratch.write(MANIFEST_FILE, &too_long);
        let error = load(&scratch.0).expect_err("超上限预算必须拒绝");
        assert_eq!(error.code(), "HBOT_MANIFEST_INVALID", "{error}");
        assert!(error.to_string().contains("capabilities.jobs"), "{error}");

        // 重名。
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"ws://h"},
               "capabilities":{"jobs":[{"name":"a"},{"name":"a"}]}}"#,
        );
        let error = load(&scratch.0).expect_err("重名必须拒绝");
        assert!(error.to_string().contains("重复声明"), "{error}");

        // 名字为空。
        scratch.write(
            MANIFEST_FILE,
            r#"{"name":"d","platform":{"kind":"onebot11","ws":"ws://h"},
               "capabilities":{"jobs":[{"name":"  "}]}}"#,
        );
        let error = load(&scratch.0).expect_err("空名字必须拒绝");
        assert!(error.to_string().contains("作业名"), "{error}");
    }
}
