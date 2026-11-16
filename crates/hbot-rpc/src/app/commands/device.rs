//! 设备能力的六条命令（`device_status` / `device_grant` / `device_revoke` /
//! `device_request` / `device_settings_get` / `device_settings_set`）。
//!
//! 契约见 `docs/09-device-capability.md` §8，DTO 在 `crate::dto`（已冻结）。
//! 这一层只做四件事：**查 bot 是否存在**、**把线上字符串解析成领域枚举**、**把阻塞的
//! 登记表调用包进 `spawn_blocking`**、**把结果/HbotError 翻译成 [`Response`]**。
//! 闸门判定与会话状态机都在 [`crate::app::device`] 里 —— 命令层再抄一份判据就是两个真源。
//!
//! 三条容易踩的：
//!
//! 1. **`device_request` 绝不改状态**。它是 Agent 说"我想做这件事，用户你批不批"的通道：
//!    只发一条 `device.request` 事件，等用户点 `device_grant`。在这里顺手开会话等于让模型
//!    自己给自己授权。
//! 2. **`device_grant` 是"用户"的命令，不是 Agent 的工具**。所以它没有"审批"这一层：
//!    调用方能发这条命令就已经是用户在点按钮了（清单闸 + 设置闸仍然在登记表里生效）。
//! 3. **`device_settings_set` 的 `settings` 是"用户能配置的宿主设置"，不是"会话授权"**。
//!    两者是不同时刻的两次决定：改设置不追溯已开的会话（时长/子能力），但收紧
//!    `allowedApps` 与总开关会在下一次设备调用上就生效。

use std::collections::BTreeSet;

use hbot_core::device::DeviceCapability;
use hbot_core::{BotId, EventSink, HbotError};
use serde_json::Value;

use crate::app::AppState;
use crate::app::device::DeviceSettings;
use crate::dto::{Command, Event, Response};

/// `reason` 的字符上限（`device.request` 事件里的理由）。
///
/// 500 是"够说清一件事"与"不该让不可信文本把事件撑大"的折中；超出截断并 `warn`
/// （方向是收紧，不丢事件）。
const REASON_MAX_CHARS: usize = 500;

/// 六条设备命令的统一入口（`Dispatcher` 已经按 `Command` 变体分派到这里）。
///
/// 前四条是**某个 bot 的会话**（都要 `ensure_bot`），后两条是**宿主设置**（与 bot 无关，
/// 所以不过 `ensure_bot`：一个还没建 bot 的干净安装也得能读默认设置）。
pub async fn handle(state: &AppState, command: Command) -> Response {
    // 闸门要从"当前项目"的 hbot.json 读设备声明；`DeviceGate` 拿不到 AppState，
    // 所以每次进命令先把项目根抄给登记表（切换项目时的滞后见 note_project_root 的说明）。
    state.device().note_project_root(state.project_root());
    let result = match command {
        Command::DeviceStatus { bot_id } => device_status(state, bot_id).await,
        Command::DeviceGrant {
            bot_id,
            sub,
            allowed_apps,
            minutes,
        } => device_grant(state, bot_id, sub, allowed_apps, minutes).await,
        Command::DeviceRevoke { bot_id } => device_revoke(state, bot_id).await,
        Command::DeviceRequest {
            bot_id,
            sub,
            reason,
        } => device_request(state, bot_id, sub, reason).await,
        Command::DeviceSettingsGet => device_settings_get(state),
        Command::DeviceSettingsSet { settings } => device_settings_set(state, settings).await,
        other => return super::unimplemented(other.kind()),
    };
    match result {
        Ok(response) => response,
        Err(error) => super::failed(error),
    }
}

/// 查询某个 bot 的设备会话快照。
///
/// 从**不**返回 `data: null`：没有会话时是 `state:"none"` 的那个对象
/// （UI 靠它画"还没开"的状态条，而不是靠"没有载荷"猜）。
async fn device_status(state: &AppState, bot_id: BotId) -> Result<Response, HbotError> {
    ensure_bot(state, &bot_id).await?;
    let status = state.device().status(&bot_id);
    with_status(status)
}

/// 开一个限时会话（用户点了"允许操作 N 分钟"）。
async fn device_grant(
    state: &AppState,
    bot_id: BotId,
    sub: Vec<String>,
    allowed_apps: Vec<String>,
    minutes: Option<u32>,
) -> Result<Response, HbotError> {
    ensure_bot(state, &bot_id).await?;
    let requested = parse_sub(&sub)?;
    let session_id = state.ids().session_id().to_string();
    let registry = std::sync::Arc::clone(state.device());
    let status = state
        .blocking(move || registry.grant(&bot_id, &requested, allowed_apps, minutes, session_id))
        .await?;
    with_status(status)
}

/// 一键撤销（幂等：没有活跃会话也成功）。
///
/// 返回 `data: null`：撤销命令的契约是"成功/失败"，要看最新状态请再发一次
/// `device_status`（事件 `device.status` 也会把最新状态推给 UI）。
async fn device_revoke(state: &AppState, bot_id: BotId) -> Result<Response, HbotError> {
    ensure_bot(state, &bot_id).await?;
    let registry = std::sync::Arc::clone(state.device());
    state.blocking(move || registry.revoke(&bot_id)).await?;
    Ok(super::done())
}

/// Agent 请求用户授权：**只发事件，不开会话、不写设置、不改任何状态**。
async fn device_request(
    state: &AppState,
    bot_id: BotId,
    sub: Vec<String>,
    reason: String,
) -> Result<Response, HbotError> {
    ensure_bot(state, &bot_id).await?;
    // `sub` 仍然要合法：事件会直接进 UI，让一串不认识的能力名漏到前端只会得到
    // "为什么点了允许却什么都没开"的困惑。
    let requested = parse_sub(&sub)?;
    let sub = requested
        .iter()
        .map(|capability| capability.as_str().to_owned())
        .collect::<Vec<_>>();
    let reason = truncate_reason(&reason);
    let request_id = state.ids().approval_id().to_string();
    state.events().emit(Event::DeviceRequest {
        bot_id,
        sub,
        reason,
        request_id,
    });
    Ok(super::done())
}

/// 读宿主设置。读的是**内存里那份**，不是每次都读盘：磁盘只是持久化，真源是登记表
/// （启动时载入，外部改动由 1 秒 tick 盯 mtime 重载）。
fn device_settings_get(state: &AppState) -> Result<Response, HbotError> {
    let settings = state.device().settings();
    with_settings(&settings)
}

/// 全量写宿主设置。五步的顺序是契约的一部分（**内存晚于磁盘**）：
///
/// 1. 校验（[`DeviceSettings::from_wire`]）：任何一处不合法 → `-32002`，
///    **绝不静默修正** —— 静默修正会让用户以为自己设的就是生效的那份；
/// 2. 原子写 `<data_dir>/device.json`（`save()` 里 tmp + rename + Windows 重试）；
/// 3. **只有落盘成功才** `replace_settings`：先改内存再落盘的写法一旦写失败，就会出现
///    "界面显示成功、重启后还是旧策略"这种最难排查的不一致；
/// 4. 发 `device.settings`（与 `device.status` 同一个 `EventBus`），让所有窗口对齐；
/// 5. 返回写入后的设置本身（UI 照它重画，不靠猜）。
///
/// 对**已开会话**的生效范围：时长与子能力不追溯（那是用户在不同时刻点头的两次决定），
/// 但 `allowedApps` 与总开关的**收紧在下一次设备调用上就生效**（闸门每次重查设置，
/// 见 [`crate::app::device::DeviceRegistry::settings`]）；放宽不追溯，只影响之后的会话。
async fn device_settings_set(
    state: &AppState,
    wire: crate::dto::DeviceSettings,
) -> Result<Response, HbotError> {
    let settings = DeviceSettings::from_wire(&wire)?;
    let registry = std::sync::Arc::clone(state.device());
    let applied = state
        .blocking(move || {
            let data_dir = registry.data_dir().to_path_buf();
            DeviceSettings::save(&data_dir, &settings)
                .map_err(|error| HbotError::internal(format!("设备设置写盘失败：{error}")))?;
            registry.replace_settings(settings.clone());
            Ok(settings)
        })
        .await?;
    state.events().emit(Event::DeviceSettings {
        settings: applied.to_wire(),
    });
    with_settings(&applied)
}

/// bot 不存在就回 `-32001`。
///
/// 四条会话类命令（`device_status`/`grant`/`revoke`/`request`）都先过这一关：
/// 一个不存在的 bot 的设备会话本来就不该存在，
/// 用 `-32001` 让调用方知道"ID 给错了"，而不是让它拿到一份空状态以为"这个 bot 还没开"。
///
/// 两条设置类命令**不过这一关**：宿主设置与 bot 无关（干净安装也要能读默认值）。
async fn ensure_bot(state: &AppState, bot_id: &BotId) -> Result<(), HbotError> {
    let store = std::sync::Arc::clone(state.store());
    let id = *bot_id;
    let present = state
        .blocking(move || {
            store
                .get_bot(&id)
                .map(|bot| bot.is_some())
                .map_err(crate::app::store_failed)
        })
        .await?;
    if present {
        Ok(())
    } else {
        Err(HbotError::not_found("bot", bot_id.to_string()))
    }
}

/// 把线上字符串解析成子能力集合；有不认识的就回 `-32002` 并列出允许值。
fn parse_sub(sub: &[String]) -> Result<BTreeSet<DeviceCapability>, HbotError> {
    let mut requested = BTreeSet::new();
    let mut unknown = Vec::new();
    for name in sub {
        match name.parse::<DeviceCapability>() {
            Ok(capability) => {
                requested.insert(capability);
            }
            Err(_) => unknown.push(name.clone()),
        }
    }
    if !unknown.is_empty() {
        return Err(HbotError::invalid(format!(
            "不认识的设备子能力：{}（允许：{}）",
            unknown.join(", "),
            DeviceCapability::all()
                .iter()
                .map(|capability| capability.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        )));
    }
    Ok(requested)
}

/// `reason` 截到 [`REASON_MAX_CHARS`] 个字符。
///
/// 按**字符**而不是字节切：按字节切会把一个汉字劈成两半，前端拿到的是乱码而不是一个短理由。
fn truncate_reason(reason: &str) -> String {
    let mut truncated = String::with_capacity(reason.len().min(REASON_MAX_CHARS * 4));
    for (index, character) in reason.chars().enumerate() {
        if index >= REASON_MAX_CHARS {
            tracing::warn!(
                limit = REASON_MAX_CHARS,
                "device_request 的 reason 超长，已截断（它是不可信文本，UI 只做展示）"
            );
            break;
        }
        truncated.push(character);
    }
    truncated
}

/// `DeviceStatus` → `Response::with_data`（序列化失败算内部错误，但不可能是常态）。
fn with_status(status: crate::dto::DeviceStatus) -> Result<Response, HbotError> {
    let data: Value = serde_json::to_value(&status)
        .map_err(|error| HbotError::internal(format!("device status 序列化失败：{error}")))?;
    Ok(Response::with_data(data))
}

/// `DeviceSettings`（宿主形状 → 线上形状）→ `Response::with_data`。
fn with_settings(settings: &DeviceSettings) -> Result<Response, HbotError> {
    let data: Value = serde_json::to_value(settings.to_wire())
        .map_err(|error| HbotError::internal(format!("device settings 序列化失败：{error}")))?;
    Ok(Response::with_data(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sub_accepts_known_names_and_normalizes_case() {
        let parsed = parse_sub(&["screen".to_owned(), " INPUT ".to_owned()]).expect("都能解析");
        assert_eq!(
            parsed,
            [DeviceCapability::Screen, DeviceCapability::Input]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn parse_sub_rejects_unknown_names_and_lists_the_allowed_ones() {
        let error =
            parse_sub(&["screen".to_owned(), "scren".to_owned()]).expect_err("拼错就得报错");
        assert_eq!(error.code(), -32002);
        let HbotError::Invalid { message } = &error else {
            panic!("期望 -32002，实际：{error:?}");
        };
        assert!(
            message.contains("scren"),
            "要点出是哪个名字不认识：{message}"
        );
        assert!(
            message.contains("clipboard"),
            "要列出允许值，用户才知道该填什么：{message}"
        );
    }

    #[test]
    fn truncate_reason_counts_characters_not_bytes() {
        let long = "设".repeat(REASON_MAX_CHARS + 10);
        let truncated = truncate_reason(&long);
        assert_eq!(truncated.chars().count(), REASON_MAX_CHARS);
        assert!(truncated.chars().all(|character| character == '设'));
        // 短的（含非 ASCII）原样返回。
        assert_eq!(truncate_reason("帮我截个图"), "帮我截个图");
    }
}
