//! 设备能力的**宿主设置**（`<data_dir>/device.json`）。
//!
//! ## 这是什么闸
//!
//! `docs/09-device-capability.md` §4 的三道闸里，第二道是"宿主设置"：清单（`hbot.json`）
//! 声明 ≠ 被允许，用户还要在本机的 `device.json` 里点名开哪些子能力、最多开多久、一个
//! 会话最多做多少个动作。文件默认**不存在**，缺省即全拒（`enabled=false`、
//! `capabilities=[]`）——这是有意的：装完就能用的设备能力等于默认把用户的桌面交出去。
//!
//! ## 容错口径（照 `hbot-desktop` 的 `DesktopSettings::load`）
//!
//! 坏文件 / 读不出来 / 不认识的子能力一律**不阻塞启动**，只是退回默认值并 `warn`：
//! 设备能力是可选能力，读不出设置时"全拒"就是安全的默认态，没必要让整个应用起不来。
//! 但**绝不静默**：每一处偏离都留一条 `warn`，否则用户会以为"我明明开了"。
//!
//! 唯一一处**主动压缩**配置的是 `maxMinutes > 30`：契约（`dto` 的 `minutes`）硬上限是 30。
//! 这里取 30 并 `warn`，与"静默放宽安全"方向相反 —— 它把用户想要的时长**收紧**到上限，
//! 不是替用户把闸门开大（那种地方必须拒绝，见 `device_grant` 的空集判定）。
//!
//! ## 读路径容错，写路径**拒绝**（口径相反，有意的）
//!
//! [`DeviceSettings::load`] 是"文件可能被用户手改"的路径：坏值只能退回默认并 `warn`，
//! 因为宿主不能因为一个可选能力的配置文件起不来。而 [`DeviceSettings::from_wire`] 是
//! `device_settings_set` 的路径：调用方就在等一个成功/失败，**静默修正会让用户以为
//! 自己设的就是生效的那份**（比如把 `maxMinutes=90` 悄悄改成 30，UI 上还显示 90 的选项）。
//! 所以写路径一律 `-32002` + 一句说清哪条不合法，绝不"就近取一个合法值"。
//!
//! ## 为什么写盘要 tmp + rename + 重试
//!
//! `AGENTS.md` §5 第 7 条：Windows 上 `rename` 覆盖已存在的目标会撞 `EPERM`（Defender
//! 或别的句柄正拿着它）。这里沿用同一套处置：先写 `device.json.tmp`，再 rename 覆盖，
//! 撞到 `PermissionDenied`/`WouldBlock` 重试 5 次共 200ms，最后一次仍失败才报错。

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hbot_core::{DeviceCapability, HbotError};
use serde::{Deserialize, Serialize};

/// 设置文件名（`<data_dir>/device.json`）。
pub const SETTINGS_FILE: &str = "device.json";

/// 原子写的临时文件后缀：`device.json` → `device.json.tmp`。
const TEMP_SUFFIX: &str = "json.tmp";

/// 撞到"目标正被别的句柄拿着"时重试几次（`AGENTS.md` §5 第 7 条）。
const RENAME_ATTEMPTS: u32 = 5;

/// 每次重试前等多久（5 次共 200ms，与 `hbot-desktop` 同口径）。
const RENAME_BACKOFF: Duration = Duration::from_millis(40);

/// 会话时长的硬上限（分钟）。
///
/// 与 `dto::command::DeviceGrant` 的契约同值（"minutes 硬上限 30"）：设置里写更大也只生效 30，
/// 并留一条 `warn`。放在这里而不是 `dto`：这是宿主策略，契约只规定"上限是 30"这件事。
pub const MAX_SESSION_MINUTES: u32 = 30;

/// `minutes` 缺省值（用户没写时长时开多久）。
pub const DEFAULT_SESSION_MINUTES: u32 = 5;

/// 一个会话最多允许多少次设备动作。
///
/// 10000 不是魔法数字：按"每秒 30 个鼠标事件"算约 5 分钟满负荷，够一次正常的自动化任务，
/// 又不至于让一个死循环把整段会话的时间全烧在注入上。超了回 `HostLimit`。
pub const DEFAULT_MAX_ACTIONS: u32 = 10_000;

/// 设备能力的宿主设置（`<data_dir>/device.json`）。
///
/// 字段名与线上的 JSON 一致（`camelCase`）：`enabled` / `capabilities` / `allowedApps` /
/// `defaultMinutes` / `maxMinutes` / `maxActionsPerSession`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSettings {
    /// 总开关。默认 `false`：**没有这一步，"声明了"不等于"能调"**。
    pub enabled: bool,
    /// 宿主要开的子能力。空集 = **全拒**（不是"全允许"）。
    pub capabilities: BTreeSet<DeviceCapability>,
    /// 宿主侧允许的应用（进程映像名），即**上限**。
    ///
    /// 语义与 `capabilities` 故意相反：**空集 = 没有设上限**（不是"全拒"）。
    /// 真正的白名单是用户在授权对话框里这一次点出来的那一份（`device_grant` 的
    /// `allowedApps`，必填、非空）；这里写出来的名字是"宿主不许超过的范围"。
    /// 两处语义相反的**理由**：能力是"开不开这类能力"，开了一类就是整类可用，所以空集必须
    /// 是全拒；应用是"在已经授权的那批里再收窄"，若空集也当全拒，用户就得在
    /// `device.json` 和授权对话框里各抄一遍同一份名单才能用一次输入——那只会让人把名单写宽。
    ///
    /// 求交集在 [`DeviceRegistry::grant`] 里做（生效集合落库，`device_status` 报的就是它），
    /// 闸门每次调用还会用 [`host_ceiling_allows`] 再查一遍——
    /// 因为设置可能在会话中途被改，那时旧会话必须跟着收紧。
    pub allowed_apps: Vec<String>,
    /// `device_grant` 没写 `minutes` 时开多久。
    pub default_minutes: u32,
    /// 单次会话时长上限（分钟）。配置里写更大 → 取 [`MAX_SESSION_MINUTES`] 并 `warn`。
    pub max_minutes: u32,
    /// 单次会话的动作数上限（见 [`DEFAULT_MAX_ACTIONS`]）。
    pub max_actions_per_session: u32,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            capabilities: BTreeSet::new(),
            allowed_apps: Vec::new(),
            default_minutes: DEFAULT_SESSION_MINUTES,
            max_minutes: MAX_SESSION_MINUTES,
            max_actions_per_session: DEFAULT_MAX_ACTIONS,
        }
    }
}

impl DeviceSettings {
    /// `device.json` 的绝对路径。
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// 读设置；文件不存在 / 读不出来 / 不是合法 JSON 都退回 [`Self::default`] 并 `warn`。
    ///
    /// 不返回 `Result`：调用方（`DeviceRegistry::new`）在 `AppState::open` 里，**不能**
    /// 因为一个可选能力的设置文件坏掉就让整个应用起不来。返回值永远是一份可用的设置。
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // 首次启动的正常情形（这个文件只在用户开过设备能力后才存在）。
                // 仍然 warn：不然"我明明开了却没生效"会变成一次无声的排查。
                tracing::warn!(
                    path = %path.display(),
                    "设备设置文件不存在，按默认值启动（设备能力默认关闭）"
                );
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "读不了设备设置文件，改用默认值（文件保持原样，不覆盖）"
                );
                return Self::default();
            }
        };
        match serde_json::from_str::<RawSettings>(&text) {
            Ok(raw) => Self::from_raw(raw, &path),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "设备设置文件不是合法 JSON，改用默认值（文件保持原样，不覆盖）"
                );
                Self::default()
            }
        }
    }

    /// 原子写回 `<data_dir>/device.json`（tmp + rename，Windows `EPERM` 重试 5 次/40ms）。
    ///
    /// 写盘失败要返回 `Err` 让调用方知道：设置没落盘就等于"下次启动还是旧策略"，
    /// 那和用户以为的"已经开了"是两件事。
    pub fn save(data_dir: &Path, settings: &Self) -> io::Result<()> {
        fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let raw = RawSettings {
            enabled: settings.enabled,
            capabilities: settings
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            allowed_apps: settings.allowed_apps.clone(),
            default_minutes: settings.default_minutes,
            max_minutes: settings.max_minutes,
            max_actions_per_session: settings.max_actions_per_session,
        };
        let mut text = serde_json::to_string_pretty(&raw).map_err(io::Error::other)?;
        text.push('\n');

        let temp = path.with_extension(TEMP_SUFFIX);
        fs::write(&temp, text.as_bytes())?;
        match rename_with_retry(&temp, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // 临时文件不能留：下次启动时会看到两个形状相同的文件，没人知道哪个是真的。
                let _ = fs::remove_file(&temp);
                Err(error)
            }
        }
    }

    /// 从线上 DTO 转成宿主设置：**校验，不修正**（见模块文档的"写路径拒绝"）。
    ///
    /// 与 [`Self::from_raw`] 的分工：那个读的是"用户可能手改过的磁盘文件"，能容错就容错；
    /// 这个读的是 `device_settings_set` 的入参，调用方就在等一个明确的成败，任何一处不合法
    /// 都当场 `-32002`。两条路径的共同底线：**没有任何一处修正会把闸门放宽**。
    ///
    /// 子能力名走 [`DeviceCapability`] 的 `FromStr`（与 `device_grant` 同一个解析器）：
    /// 在这里手抄一份 `["screen", "ocr", …]` 就等于给能力名单造第二真源，将来
    /// `hbot-core` 加一种能力时两处一定不一致。
    pub(crate) fn from_wire(wire: &crate::dto::DeviceSettings) -> Result<Self, HbotError> {
        let mut capabilities = BTreeSet::new();
        let mut unknown = Vec::new();
        for name in &wire.capabilities {
            match name.parse::<DeviceCapability>() {
                Ok(capability) => {
                    capabilities.insert(capability);
                }
                Err(_) => unknown.push(name.clone()),
            }
        }
        if !unknown.is_empty() {
            return Err(HbotError::invalid(format!(
                "不认识的设备子能力：{}（允许：{}）",
                unknown.join(", "),
                allowed_names()
            )));
        }

        if let Some(index) = wire
            .allowed_apps
            .iter()
            .position(|app| app.trim().is_empty())
        {
            return Err(HbotError::invalid(format!(
                "allowedApps 第 {} 项是空应用名（trim 后为空）：要么写映像名，要么整项删掉",
                index + 1
            )));
        }
        // 归一化只允许有一份实现（`normalize_apps`：trim → 小写 → 去重，保序）。
        // 传克隆：DTO 是调用方的数据，别就地改。
        let allowed_apps = super::normalize_apps(wire.allowed_apps.clone());

        if wire.max_minutes == 0 || wire.max_minutes > MAX_SESSION_MINUTES {
            return Err(HbotError::invalid(format!(
                "maxMinutes 必须在 1..={MAX_SESSION_MINUTES} 之间（{MAX_SESSION_MINUTES} 是硬上限，\
                 与 device_grant 的实现一致），实际 {}",
                wire.max_minutes
            )));
        }
        if wire.default_minutes == 0 || wire.default_minutes > wire.max_minutes {
            return Err(HbotError::invalid(format!(
                "defaultMinutes 必须在 1..=maxMinutes（{}）之间，实际 {}",
                wire.max_minutes, wire.default_minutes
            )));
        }
        if wire.max_actions_per_session == 0 {
            return Err(HbotError::invalid(
                "maxActionsPerSession 必须 >= 1（0 会让第一次设备动作就被 HostLimit 拒掉）",
            ));
        }

        Ok(Self {
            enabled: wire.enabled,
            capabilities,
            allowed_apps,
            default_minutes: wire.default_minutes,
            max_minutes: wire.max_minutes,
            max_actions_per_session: wire.max_actions_per_session,
        })
    }

    /// 转成线上 DTO（`device_settings_get` 的 `data`、`device.settings` 事件的载荷）。
    ///
    /// 子能力按 [`DeviceCapability`] 的声明顺序输出（`BTreeSet` 的迭代序就是枚举序），
    /// 所以同一份设置每次序列化出来的数组顺序都稳定 —— UI 与测试都不必自己排序。
    pub(crate) fn to_wire(&self) -> crate::dto::DeviceSettings {
        crate::dto::DeviceSettings {
            enabled: self.enabled,
            capabilities: self
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            allowed_apps: self.allowed_apps.clone(),
            default_minutes: self.default_minutes,
            max_minutes: self.max_minutes,
            max_actions_per_session: self.max_actions_per_session,
        }
    }

    /// 把"线上形状"归一化成生效值，并对每一处偏离留 `warn`。
    fn from_raw(raw: RawSettings, path: &Path) -> Self {
        let mut capabilities = BTreeSet::new();
        for name in &raw.capabilities {
            match name.parse::<DeviceCapability>() {
                Ok(capability) => {
                    capabilities.insert(capability);
                }
                // 丢掉不认识的项只会**收紧**权限（不会放宽），所以可以继续；但要留痕，
                // 否则用户以为那一项已经开了，实际什么都没开。
                Err(_) => tracing::warn!(
                    path = %path.display(),
                    %name,
                    "设备设置里有不认识的子能力名，已忽略这一项（允许：{}）",
                    allowed_names()
                ),
            }
        }

        let max_minutes = match raw.max_minutes {
            0 => {
                tracing::warn!(
                    path = %path.display(),
                    "maxMinutes=0 会让每个会话立刻到期，按 1 分钟生效"
                );
                1
            }
            value if value > MAX_SESSION_MINUTES => {
                // 善意超限：用户想开久一点，收紧到契约上限（不是放宽，也不是拒绝）。
                tracing::warn!(
                    path = %path.display(),
                    configured = value,
                    limit = MAX_SESSION_MINUTES,
                    "maxMinutes 超过硬上限，按 {MAX_SESSION_MINUTES} 分钟生效"
                );
                MAX_SESSION_MINUTES
            }
            value => value,
        };

        let default_minutes = match raw.default_minutes {
            0 => {
                tracing::warn!(
                    path = %path.display(),
                    "defaultMinutes=0 会让缺省会话立刻到期，按 1 分钟生效"
                );
                1
            }
            value => value,
        };
        let default_minutes = if default_minutes > max_minutes {
            tracing::warn!(
                path = %path.display(),
                default_minutes,
                max_minutes,
                "defaultMinutes 超过 maxMinutes，按 maxMinutes 生效"
            );
            max_minutes
        } else {
            default_minutes
        };

        let max_actions_per_session = match raw.max_actions_per_session {
            0 => {
                tracing::warn!(
                    path = %path.display(),
                    "maxActionsPerSession=0 会让第一次设备动作就被 HostLimit 拒掉，按 1 次生效"
                );
                1
            }
            value => value,
        };

        Self {
            enabled: raw.enabled,
            capabilities,
            allowed_apps: raw.allowed_apps,
            default_minutes,
            max_minutes,
            max_actions_per_session,
        }
    }
}

/// 线上形状：为了容错而**故意宽松**的中间结构。
///
/// * 没有 `deny_unknown_fields` —— 未知字段忽略（前后版本可以各自加字段）；
/// * 每个字段都有默认值（`#[serde(default)]` + 手写 `Default`），少写就是默认；
/// * `capabilities` 收 `Vec<String>` 而不是 `Vec<DeviceCapability>`：后者以 `DeviceCapability`
///   的 serde 实现为准，一个拼错的 `"scren"` 会让**整个文件**反序列化失败 → 全家退回默认值
///   （连 `enabled=true` 也丢）。收字符串再逐项解析，才有机会只丢那一项 + `warn`。
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct RawSettings {
    enabled: bool,
    capabilities: Vec<String>,
    allowed_apps: Vec<String>,
    default_minutes: u32,
    max_minutes: u32,
    max_actions_per_session: u32,
}

impl Default for RawSettings {
    fn default() -> Self {
        let defaults = DeviceSettings::default();
        Self {
            enabled: defaults.enabled,
            capabilities: Vec::new(),
            allowed_apps: Vec::new(),
            default_minutes: defaults.default_minutes,
            max_minutes: defaults.max_minutes,
            max_actions_per_session: defaults.max_actions_per_session,
        }
    }
}

/// `allowedNames` 类错误文案里列的全部子能力名（拼一次，别在各处手抄）。
fn allowed_names() -> String {
    DeviceCapability::all()
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// rename 覆盖：撞到"目标被占用"时等一下再试。
///
/// 只对 `PermissionDenied` / `WouldBlock` 重试 —— 那是 Windows 上 `MoveFileEx` 的瞬时失败；
/// 其余错误（跨卷、路径不存在）重试也不会变好，直接返回。
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last: Option<io::Error> = None;
    for attempt in 1..=RENAME_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if is_transient(&error) && attempt < RENAME_ATTEMPTS => {
                tracing::debug!(
                    attempt,
                    from = %from.display(),
                    to = %to.display(),
                    %error,
                    "设备设置落盘 rename 被占用，重试"
                );
                last = Some(error);
                std::thread::sleep(RENAME_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("device settings rename failed")))
}

/// 这个 IO 错误值不值得再试一次。
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试自己的临时目录（`crates/hbot-rpc/tests` 里那些也是这样，不用 dev-dependency）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hbot-device-settings-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write(dir: &Path, text: &str) {
        fs::create_dir_all(dir).expect("建 data_dir");
        fs::write(DeviceSettings::path(dir), text).expect("写 device.json");
    }

    /// 没有文件 = 全拒的默认值（这是"装完就安全"的那条基线）。
    #[test]
    fn missing_file_falls_back_to_deny_all_defaults() {
        let dir = TempDir::new("missing");
        let settings = DeviceSettings::load(&dir.path);
        assert_eq!(settings, DeviceSettings::default());
        assert!(!settings.enabled, "缺省必须是关的");
        assert!(settings.capabilities.is_empty(), "缺省是空集 = 全拒");
        assert_eq!(settings.default_minutes, 5);
        assert_eq!(settings.max_minutes, 30);
        assert_eq!(settings.max_actions_per_session, 10_000);
    }

    /// 坏 JSON 退回默认值，**不覆盖**文件（用户手改坏了还能自己修回来）。
    #[test]
    fn corrupt_file_falls_back_to_defaults_without_rewriting_it() {
        let dir = TempDir::new("corrupt");
        write(&dir.path, "{ this is not json");
        let settings = DeviceSettings::load(&dir.path);
        assert_eq!(settings, DeviceSettings::default());
        assert_eq!(
            fs::read_to_string(DeviceSettings::path(&dir.path)).expect("文件还在"),
            "{ this is not json"
        );
    }

    /// 正常文件：snake_case 子能力名、未知字段忽略、数值原样生效。
    #[test]
    fn valid_file_is_parsed_and_unknown_fields_are_ignored() {
        let dir = TempDir::new("valid");
        write(
            &dir.path,
            r#"{
                "enabled": true,
                "capabilities": ["screen", "input", "clipboard"],
                "allowedApps": ["game.exe"],
                "defaultMinutes": 7,
                "maxMinutes": 20,
                "maxActionsPerSession": 42,
                "futureField": {"whatever": 1}
            }"#,
        );
        let settings = DeviceSettings::load(&dir.path);
        assert!(settings.enabled);
        assert_eq!(
            settings.capabilities,
            [
                DeviceCapability::Screen,
                DeviceCapability::Input,
                DeviceCapability::Clipboard
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
        assert_eq!(settings.allowed_apps, vec!["game.exe".to_owned()]);
        assert_eq!(settings.default_minutes, 7);
        assert_eq!(settings.max_minutes, 20);
        assert_eq!(settings.max_actions_per_session, 42);
    }

    /// 空 `capabilities` 保持空集（**不是**"空 = 全允许"）。
    #[test]
    fn empty_capabilities_stay_empty() {
        let dir = TempDir::new("empty-caps");
        write(&dir.path, r#"{"enabled": true, "capabilities": []}"#);
        let settings = DeviceSettings::load(&dir.path);
        assert!(settings.enabled);
        assert!(settings.capabilities.is_empty());
    }

    /// `maxMinutes` 写 999：取 30（不是 999，也不是"拒绝启动"）。
    #[test]
    fn max_minutes_over_the_hard_cap_is_clamped_to_thirty() {
        let dir = TempDir::new("clamp-max");
        write(
            &dir.path,
            r#"{"enabled": true, "maxMinutes": 999, "defaultMinutes": 90}"#,
        );
        let settings = DeviceSettings::load(&dir.path);
        assert_eq!(settings.max_minutes, 30);
        assert_eq!(
            settings.default_minutes, 30,
            "defaultMinutes 90 超过上限，也要收到 maxMinutes"
        );
    }

    /// 子能力名字拼错：只丢那一项，别的照旧（不会因为一个 typo 把 `enabled` 一起丢掉）。
    #[test]
    fn unknown_capability_name_is_dropped_but_the_rest_survives() {
        let dir = TempDir::new("typo");
        write(
            &dir.path,
            r#"{"enabled": true, "capabilities": ["screen", "scren", "INPUT"]}"#,
        );
        let settings = DeviceSettings::load(&dir.path);
        assert!(settings.enabled, "enabled 不能被一个拼错的子能力带走");
        assert_eq!(
            settings.capabilities,
            [DeviceCapability::Screen, DeviceCapability::Input]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "大小写按 FromStr 归一化，拼错的丢掉"
        );
    }

    /// 存 → 读 往返一致，且落盘后临时文件不残留。
    #[test]
    fn save_then_load_round_trips_and_leaves_no_temp_file() {
        let dir = TempDir::new("roundtrip");
        let settings = DeviceSettings {
            enabled: true,
            capabilities: [DeviceCapability::Screen, DeviceCapability::Window]
                .into_iter()
                .collect(),
            allowed_apps: vec!["game.exe".to_owned()],
            default_minutes: 3,
            max_minutes: 10,
            max_actions_per_session: 500,
        };
        DeviceSettings::save(&dir.path, &settings).expect("写设置");
        assert_eq!(DeviceSettings::load(&dir.path), settings);
        assert!(
            !dir.path.join("device.json.tmp").exists(),
            "rename 成功后临时文件必须消失"
        );
    }

    /// 写路径的样例：一份全部字段都合法、且值都不是默认值的设置。
    fn wire() -> crate::dto::DeviceSettings {
        crate::dto::DeviceSettings {
            enabled: true,
            capabilities: vec!["window".to_owned(), "screen".to_owned()],
            allowed_apps: vec!["notepad.exe".to_owned()],
            default_minutes: 5,
            max_minutes: 20,
            max_actions_per_session: 100,
        }
    }

    fn invalid_message(error: &HbotError) -> &str {
        match error {
            HbotError::Invalid { message } => message,
            other => panic!("期望 -32002 Invalid，实际：{other:?}"),
        }
    }

    /// 合法载荷原样生效（子能力按枚举顺序、应用名归一化），`to_wire` 能转回去。
    #[test]
    fn from_wire_accepts_a_valid_payload_and_round_trips() {
        let settings = DeviceSettings::from_wire(&wire()).expect("合法载荷必须通过");
        assert!(settings.enabled);
        assert_eq!(
            settings.capabilities,
            [DeviceCapability::Screen, DeviceCapability::Window]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(settings.allowed_apps, vec!["notepad.exe".to_owned()]);
        assert_eq!(settings.default_minutes, 5);
        assert_eq!(settings.max_minutes, 20);
        assert_eq!(settings.max_actions_per_session, 100);

        let back = settings.to_wire();
        assert_eq!(
            back.capabilities,
            vec!["screen".to_owned(), "window".to_owned()],
            "线上顺序按枚举声明序（screen 在 window 前），稳定可复现"
        );
        assert_eq!(
            DeviceSettings::from_wire(&back).expect("转回来还是合法"),
            settings
        );
    }

    /// 大小写与空白在写路径上也归一化（`"Notepad.EXE"` → `notepad.exe`，重复项去掉）。
    #[test]
    fn from_wire_trims_lowercases_and_dedupes_app_names() {
        let mut payload = wire();
        payload.allowed_apps = vec![
            "  Notepad.EXE ".to_owned(),
            "notepad.exe".to_owned(),
            "NOTEPAD.EXE".to_owned(),
        ];
        let settings = DeviceSettings::from_wire(&payload).expect("归一化后合法");
        assert_eq!(settings.allowed_apps, vec!["notepad.exe".to_owned()]);
    }

    /// 不认识的子能力名：`-32002`，且把允许值列出来（用户照提示就能改对）。
    #[test]
    fn from_wire_rejects_unknown_capability_names() {
        let mut payload = wire();
        payload.capabilities = vec!["screen".to_owned(), "scren".to_owned()];
        let error = DeviceSettings::from_wire(&payload).expect_err("拼错必须拒绝");
        assert_eq!(error.code(), -32002);
        let message = invalid_message(&error);
        assert!(message.contains("scren"), "要点出是哪个名字：{message}");
        assert!(message.contains("clipboard"), "要列出允许值：{message}");
    }

    /// `maxMinutes` 硬上限 30：31 与 0 都拒绝（读路径会截断，写路径不行）。
    #[test]
    fn from_wire_rejects_max_minutes_outside_the_hard_range() {
        let mut payload = wire();
        payload.max_minutes = 31;
        let error = DeviceSettings::from_wire(&payload).expect_err("超上限必须拒绝");
        assert_eq!(error.code(), -32002);
        assert!(invalid_message(&error).contains("maxMinutes"));

        let mut zero = wire();
        zero.max_minutes = 0;
        assert_eq!(
            DeviceSettings::from_wire(&zero)
                .expect_err("0 分钟必须拒绝")
                .code(),
            -32002
        );
    }

    /// `defaultMinutes` 必须在 `1..=maxMinutes`：大于上限与 0 都拒绝。
    #[test]
    fn from_wire_rejects_default_minutes_outside_the_max() {
        let mut payload = wire();
        payload.max_minutes = 10;
        payload.default_minutes = 11;
        let error = DeviceSettings::from_wire(&payload).expect_err("超过 maxMinutes 必须拒绝");
        assert_eq!(error.code(), -32002);
        assert!(invalid_message(&error).contains("defaultMinutes"));

        let mut zero = wire();
        zero.default_minutes = 0;
        assert_eq!(
            DeviceSettings::from_wire(&zero)
                .expect_err("0 分钟必须拒绝")
                .code(),
            -32002
        );
    }

    /// 空应用名：拒绝并指出第几项（不是静默丢掉 —— 丢掉会让用户以为设上了）。
    #[test]
    fn from_wire_rejects_empty_app_names() {
        let mut payload = wire();
        payload.allowed_apps = vec!["notepad.exe".to_owned(), "   ".to_owned()];
        let error = DeviceSettings::from_wire(&payload).expect_err("空应用名必须拒绝");
        assert_eq!(error.code(), -32002);
        let message = invalid_message(&error);
        assert!(message.contains("第 2 项"), "要指出是哪一项：{message}");
    }

    /// `maxActionsPerSession=0` 会让第一次设备动作就被拒，写路径直接报错。
    #[test]
    fn from_wire_rejects_zero_max_actions() {
        let mut payload = wire();
        payload.max_actions_per_session = 0;
        let error = DeviceSettings::from_wire(&payload).expect_err("0 次动作必须拒绝");
        assert_eq!(error.code(), -32002);
        assert!(invalid_message(&error).contains("maxActionsPerSession"));
    }
}
