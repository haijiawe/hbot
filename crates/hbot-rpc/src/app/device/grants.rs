//! **会话与审计**：内存里一条 live 会话的形状，以及写审计行的唯一入口。
//!
//! 这个文件只管数据与纯逻辑（序列化成 DB 列、剩余时长、动作计数、白名单匹配），
//! 状态迁移与落库由 `mod.rs` 的 `DeviceRegistry` 负责 —— 分开的原因是：
//! 会话字段的"形状"会被 `device_status`、事件、DB 行三处同时读，逻辑一旦混在一起，
//! 三处就会各自漂移。
//!
//! 为什么 `actions` 是 `AtomicU32` 而不是 `u32`：`DeviceGate::authorize` 是 `&self` 的同步
//! 方法（`hbot-core` 已冻结），注入路径上会被并发调用；用 `Mutex<u32>` 就得让每次鼠标
//! 动作都去抢会话表的锁。计数用原子量就够，反正它只是"这一会话还能做多少动作"的粗限。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};

use hbot_core::device::{
    DeviceActor, DeviceCapability, DeviceError, DeviceSessionState, DeviceTarget,
};
use hbot_core::{BotId, Timestamp};
use hbot_store::NewDeviceAudit;
use hbot_store::Store;

use crate::dto::DeviceStatus;

/// 内存里的一条设备会话。
///
/// 与 `device_sessions` 表一一对应（字段而不是元组：审计与事件都要按名字取值），
/// 但**只保留"当前这一条"**的主机侧视图 —— 历史行在库里，内存只留最近一条，
/// 这样 `device_status` 才不用为了"上次会话是不是 revoked"去查库。
pub(crate) struct LiveSession {
    /// 会话 ID（UUIDv4 字符串，与 `device_sessions.id` 一致）。
    pub(crate) id: String,
    /// 当前状态。只有 `Active` 算 live（见 `DeviceSessionState::is_live`）。
    pub(crate) state: DeviceSessionState,
    /// 生效的子能力（清单声明 ∩ 宿主设置 ∩ 本次请求）。
    pub(crate) sub: BTreeSet<DeviceCapability>,
    /// 这个会话允许的应用（进程映像名）。
    pub(crate) allowed_apps: Vec<String>,
    /// 授予时刻。
    pub(crate) granted_at: Timestamp,
    /// 到期时刻（`granted_at + minutes`）。
    pub(crate) deadline: Timestamp,
    /// 已经批准过多少个设备动作。
    pub(crate) actions: AtomicU32,
    /// 这一会话的动作数上限（授予时从设置里抄一份，之后改设置不影响已开的会话）。
    pub(crate) max_actions: u32,
}

impl LiveSession {
    /// 新开一条 `Active` 会话。
    pub(crate) fn new(
        id: String,
        sub: BTreeSet<DeviceCapability>,
        allowed_apps: Vec<String>,
        granted_at: Timestamp,
        deadline: Timestamp,
        max_actions: u32,
    ) -> Self {
        Self {
            id,
            state: DeviceSessionState::Active,
            sub,
            allowed_apps,
            granted_at,
            deadline,
            actions: AtomicU32::new(0),
            max_actions,
        }
    }

    /// `sub` 列：JSON 数组字符串（`[]` = 全拒）。
    ///
    /// 用 `serde_json` 而不是逗号拼接：`docs/09` §3.1 的旧稿写的是逗号分隔，但 JSON 数组
    /// 才能让"空 = 全拒"和"有一个空的项"区分开，也不必为分隔符里的逗号定转义规矩。
    pub(crate) fn sub_json(&self) -> String {
        serde_json::to_string(
            &self
                .sub
                .iter()
                .map(|capability| capability.as_str())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_owned())
    }

    /// `allowed_apps` 列：JSON 数组字符串。
    pub(crate) fn allowed_apps_json(&self) -> String {
        serde_json::to_string(&self.allowed_apps).unwrap_or_else(|_| "[]".to_owned())
    }

    /// 这个会话现在还活着吗（`state == active`，且没到 deadline）。
    ///
    /// 到期但 tick 还没扫到也算"不活"：判据只有一处，别让调用方各自比 deadline。
    pub(crate) fn is_live_at(&self, now: Timestamp) -> bool {
        self.state.is_live() && now < self.deadline
    }

    /// 剩余毫秒；已结束或已到期为 `None`（契约要求 `remainingMs` 在那种情况下是 `null`）。
    pub(crate) fn remaining_ms(&self, now: Timestamp) -> Option<i64> {
        if !self.is_live_at(now) {
            return None;
        }
        Some(self.deadline.unix_millis() - now.unix_millis())
    }

    /// 目标进程是否在这个会话的白名单里。
    ///
    /// 大小写不敏感：Windows 的映像名在用户手里千奇百怪（`Game.exe` / `game.exe`），
    /// 而白名单不该因为大小写变成一次静默的拒绝。空名单 = 谁都不放。
    pub(crate) fn allows_process(&self, process: &str) -> bool {
        self.allowed_apps
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(process))
    }

    /// 记一次动作；超过上限回 [`DeviceError::HostLimit`]。
    ///
    /// 先加再比：`fetch_add` 返回旧值，`old >= max` 就是"这一发已经超了"，把加的还回去。
    /// 并发的两发最多让计数多跳一格（无害），不会让上限失效。
    pub(crate) fn take_action(&self) -> Result<(), DeviceError> {
        let previous = self.actions.fetch_add(1, Ordering::Relaxed);
        if previous >= self.max_actions {
            self.actions.fetch_sub(1, Ordering::Relaxed);
            return Err(DeviceError::HostLimit {
                detail: "每会话动作数上限".to_owned(),
            });
        }
        Ok(())
    }

    /// 已经用掉多少动作（给测试与将来的诊断面用）。
    #[cfg(test)]
    pub(crate) fn action_count(&self) -> u32 {
        self.actions.load(Ordering::Relaxed)
    }

    /// 快照成 DTO（`device_status` 的 `data`，也是 `device.status` 事件的载荷）。
    pub(crate) fn status(&self, bot_id: &BotId, now: Timestamp) -> DeviceStatus {
        DeviceStatus {
            bot_id: *bot_id,
            session_id: Some(self.id.clone()),
            state: self.state.as_str().to_owned(),
            sub: self
                .sub
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            allowed_apps: self.allowed_apps.clone(),
            granted_at: Some(self.granted_at),
            deadline: Some(self.deadline),
            remaining_ms: self.remaining_ms(now),
        }
    }
}

/// 目标里的进程名（`DeviceTarget::Window{process}`）；`None` 目标回 `None`。
///
/// 审计的 `target` 列只写这个：**不写窗口标题、不写截图内容、不写剪贴板内容**（§3.2）。
pub(crate) fn target_process(target: &DeviceTarget) -> Option<&str> {
    match target {
        DeviceTarget::None => None,
        DeviceTarget::Window { process } => Some(process.as_str()),
    }
}

/// 写一条审计。
///
/// 失败**不让设备动作失败**：审计是"事后能查"，不是"动手的前提"；一条写不进去的审计
/// 不该把用户正在跑的任务卡死。但必须 `warn` —— 静默丢审计等于事后查不到，是最难查的那种。
pub(crate) fn append_audit(
    store: &Store,
    bot_id: &BotId,
    actor: DeviceActor,
    action: &str,
    target: Option<&str>,
    outcome: &str,
    at: Timestamp,
) {
    let entry = NewDeviceAudit {
        at: at.unix_millis(),
        bot_id: *bot_id,
        actor: actor.as_str().to_owned(),
        action: action.to_owned(),
        target: target.map(str::to_owned),
        outcome: outcome.to_owned(),
    };
    if let Err(error) = store.append_device_audit(entry) {
        tracing::warn!(
            bot = %bot_id,
            %action,
            %error,
            "设备审计写库失败（动作照常放行，但事后查不到这一条）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(max_actions: u32) -> LiveSession {
        LiveSession::new(
            "s1".to_owned(),
            [DeviceCapability::Screen, DeviceCapability::Input]
                .into_iter()
                .collect(),
            vec!["Game.exe".to_owned()],
            Timestamp::from_unix_millis(1_000),
            Timestamp::from_unix_millis(2_000),
            max_actions,
        )
    }

    #[test]
    fn sub_and_apps_serialize_as_json_arrays() {
        let session = session(10);
        assert_eq!(session.sub_json(), r#"["screen","input"]"#);
        assert_eq!(session.allowed_apps_json(), r#"["Game.exe"]"#);
        let empty = LiveSession::new(
            "s2".to_owned(),
            BTreeSet::new(),
            Vec::new(),
            Timestamp::from_unix_millis(0),
            Timestamp::from_unix_millis(1),
            1,
        );
        assert_eq!(empty.sub_json(), "[]", "空集落库是 [] 而不是空串");
    }

    #[test]
    fn remaining_ms_is_none_once_the_deadline_passes() {
        let session = session(10);
        assert_eq!(
            session.remaining_ms(Timestamp::from_unix_millis(1_500)),
            Some(500)
        );
        assert_eq!(
            session.remaining_ms(Timestamp::from_unix_millis(2_000)),
            None
        );
        assert!(!session.is_live_at(Timestamp::from_unix_millis(2_000)));
    }

    #[test]
    fn process_whitelist_is_case_insensitive_and_empty_denies_all() {
        let session = session(10);
        assert!(session.allows_process("game.exe"));
        assert!(session.allows_process("GAME.EXE"));
        assert!(!session.allows_process("other.exe"));
        let empty = LiveSession::new(
            "s3".to_owned(),
            BTreeSet::new(),
            Vec::new(),
            Timestamp::from_unix_millis(0),
            Timestamp::from_unix_millis(1),
            1,
        );
        assert!(!empty.allows_process("anything.exe"), "空名单 = 全拒");
    }

    #[test]
    fn action_counter_stops_at_the_limit() {
        let session = session(2);
        assert_eq!(session.action_count(), 0);
        session.take_action().expect("第 1 次");
        session.take_action().expect("第 2 次");
        let error = session.take_action().expect_err("第 3 次必须被拒");
        assert!(
            matches!(&error, DeviceError::HostLimit { detail } if detail == "每会话动作数上限"),
            "超限必须是 HostLimit 且 detail 对得上，实际：{error:?}"
        );
        assert_eq!(session.action_count(), 2, "被拒的那一发不该计数");
    }
}
