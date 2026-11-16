//! 设备能力的**进程级登记表**：宿主设置 + 内存里的 live 会话 + 闸门 + 到期结算。
//!
//! ## 为什么是进程级
//!
//! `docs/09-device-capability.md` §4.3 要求"每一次宿主调用都查这个 Arc"，而不是把授权结果
//! 快照进隔离环境：撤销必须立刻生效，快照会让"用户刚点了撤销、脚本还在动鼠标"成为可能。
//! 所以登记表挂在 `AppState` 上，`DeviceGate` 拿的是它的 `Weak`，每个动作都回来问一次。
//!
//! ## 内存与库的分工
//!
//! * **内存**（`sessions`）只放每个 bot **最近一条**会话，是 `device_status` 与闸门的唯一真源；
//!   这样查询不用碰库，闸门也不用为一次鼠标移动去查一次 SQLite。
//! * **库**（`device_sessions`）是历史：授予、撤销、到期各落一行，用来事后查"那天谁开了多久"。
//! * 两者顺序永远是**先落库、再改内存**：写库失败时内存还是旧状态，重启后库里也是旧状态，
//!   两边一致地"没发生"。反过来会出现"库里还是 active、内存说已撤销"，重启就复活了。
//!
//! ## 锁的纪律
//!
//! 只有一把会话锁，而且**同步**（`std::sync::Mutex`）：`DeviceGate::authorize` 是 `&self` 的
//! 同步方法（`hbot-core` 已冻结），注入路径上不能 `.await`。所有落库调用都来自
//! `spawn_blocking`（命令层走 `AppState::blocking`，tick 走 `tokio::task::spawn_blocking`），
//! 所以锁里做同步 SQLite 调用是这条链路上唯一能保证"内存与库同一步"的写法 —— 代价是
//! 一次会话的写库期间同 bot 的闸门要等，这是有意的（同一 bot 不该并发改同一条会话）。
//!
//! 唯一**不**在锁里做 IO 的是审计（[`grants::append_audit`]）：它是"事后能查"而不是
//! "动手的前提"，不能因为写一条审计就把鼠标动作串行化。
//!
//! ## 到期结算
//!
//! 契约（§7）没有自动续期：到期就是到期。结算有两个入口：
//!
//! 1. `AppState::open` 起的 1 秒 tick（[`DeviceRegistry::spawn_expiry_tick`]）—— 正常情况下
//!    由它把状态推到 `expired` 并落库；
//! 2. 闸门发现"已经过了 deadline 但 tick 还没扫到"—— 就地收尾，然后拒绝。
//!
//! tick 持 [`Weak`] 而不是 `Arc`：`AppState` 一析构，登记表就没了，下一轮 `upgrade()` 失败
//! 自然退出 —— 不需要 `AppState` 加 `Drop`，也不需要 `tokio_util` 之类的取消令牌。

pub mod agent_backend;
pub mod capabilities;
pub mod settings;

mod grants;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::time::Duration;

use hbot_core::device::{
    DeviceActor, DeviceCapability, DeviceError, DeviceGate, DeviceSessionState, DeviceTarget,
};
use hbot_core::{BotId, Clock, EventSink, Timestamp};
use hbot_store::NewDeviceSession;
use hbot_store::Store;
use tokio::time::MissedTickBehavior;

use crate::app::events::EventBus;
use crate::app::store_failed;
use crate::dto::{DeviceStatus, Event};

use grants::{LiveSession, append_audit, target_process};

pub use settings::DeviceSettings;

/// 到期结算的扫描间隔。
///
/// 1 秒是"用户能感知到的及时"与"SQLite 写入次数"的折中：真到期的那一刻最多晚 1 秒才落库，
/// 而闸门自己也会在发现过期时就地拒绝，所以这 1 秒只影响"状态栏什么时候变 expired"。
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// 设备能力的进程级登记表（`AppState` 上唯一的那一个）。
pub struct DeviceRegistry {
    /// 数据目录（`device.json` 在它下面）。
    data_dir: PathBuf,
    /// 当前生效的宿主设置。启动时 [`settings::DeviceSettings::load`] 读一次。
    settings: RwLock<DeviceSettings>,
    /// 每个 bot 最近一条会话（内存真源）。
    sessions: Mutex<HashMap<BotId, LiveSession>>,
    /// 落库用（`device_sessions` / `device_audit`）。
    store: Arc<Store>,
    /// 时间源。`ManualClock` 让到期测试不必真的等 30 分钟。
    clock: Arc<dyn Clock>,
    /// 发 `device.status` 事件。与 `AppState` 用的是同一个 `EventBus`。
    events: Arc<EventBus>,
    /// 当前项目根（闸门用来读 `hbot.json` 的 `permissions.device.sub`）。
    ///
    /// 为什么不是每次去问 `AppState`：`DeviceGate::authorize` 只有 `&self`，拿不到 `AppState`。
    /// 命令层每次进来先把 `AppState::project_root()` 抄进来（[`Self::note_project_root`]）。
    project_root: RwLock<Option<PathBuf>>,
    /// tick 的取消标记（`Drop` 里置位，循环里检查）。
    cancel: Arc<AtomicBool>,
    /// 唤醒 tick 用：`Drop` 之后不用等满 1 秒才发现该退出。
    wake: Arc<tokio::sync::Notify>,
}

impl DeviceRegistry {
    /// 建登记表并读一次设置（`<data_dir>/device.json`）。
    ///
    /// 构造过程**不会**失败：设置文件坏掉只退回默认值 + `warn`，不阻塞宿主启动
    /// （与 `hbot-desktop` 的 `DesktopSettings::load` 同口径）。
    pub fn new(
        data_dir: PathBuf,
        store: Arc<Store>,
        clock: Arc<dyn Clock>,
        events: Arc<EventBus>,
    ) -> Self {
        let settings = DeviceSettings::load(&data_dir);
        Self {
            data_dir,
            settings: RwLock::new(settings),
            sessions: Mutex::new(HashMap::new()),
            store,
            clock,
            events,
            project_root: RwLock::new(None),
            cancel: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 数据目录（`device.json` 所在）。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 当前设置的快照（改设置不影响已开的会话，见 [`LiveSession::max_actions`]）。
    pub fn settings(&self) -> DeviceSettings {
        self.settings_read().clone()
    }

    /// 换一份设置（内存里立即生效；落盘由调用方显式调 [`DeviceSettings::save`]）。
    pub fn replace_settings(&self, settings: DeviceSettings) {
        *self.settings_write() = settings;
    }

    /// 记下当前项目根，供闸门读清单。
    ///
    /// 命令层在每次进入时调用。**已知滞后**：`AppState::open_project` 切换项目时不会经过
    /// 这里（那条路径不在本单元的写域内），所以"刚切到新项目、还没调过任何设备命令"的窗口里，
    /// 闸门读的是上一个项目的清单。安全方向是收紧（旧项目没声明的能力会被拒），
    /// 但反过来"旧项目声明过、新项目没声明"时会多放一个能力 —— 集成阶段建议在
    /// `open_project` 里补一行 `self.device().note_project_root(self.project_root())`。
    pub fn note_project_root(&self, root: Option<PathBuf>) {
        let mut slot = self.project_root_write();
        if *slot != root {
            *slot = root;
        }
    }

    /// 当前 bot 的设备会话快照（`device_status` 的 `data`）。
    ///
    /// **只读内存**：查询命令不写库、不改状态（`device_status` 的契约就是"查询不改状态"）。
    /// 代价是重启后内存是空的 → 状态显示 `none`，直到用户重新授予；库里的历史仍在。
    pub fn status(&self, bot_id: &BotId) -> DeviceStatus {
        let settings = self.settings();
        let now = self.clock.now();
        let sessions = self.sessions_read();
        match sessions.get(bot_id) {
            Some(session) => session.status(bot_id, now),
            None => empty_status(bot_id, &settings),
        }
    }

    /// 开一个新会话：把旧会话收尾成 `revoked`，落新行，改内存，发事件。
    ///
    /// 生效子能力 = 清单声明 ∩ 宿主设置 ∩ 本次请求（空集直接拒绝，不"就近给一个"）。
    /// 时长超上限**截到上限并 `warn`** —— 这是善意超限（用户想开久一点），与"静默放宽安全"
    /// 方向相反：它把权限收紧到契约允许的范围，不是替用户把闸门开大。真正需要拒绝的是
    /// "求交集为空"那种情况，那里必须报错。
    pub fn grant(
        &self,
        bot_id: &BotId,
        requested: &BTreeSet<DeviceCapability>,
        allowed_apps: Vec<String>,
        minutes: Option<u32>,
        session_id: String,
    ) -> Result<DeviceStatus, hbot_core::HbotError> {
        let settings = self.settings();
        let sub = self.effective_sub(&settings, requested)?;
        let minutes = self.effective_minutes(&settings, minutes);
        // 只有"注入输入"或"按窗口操作"才需要应用白名单；纯截屏/剪贴板会话给空表是正常的。
        let needs_apps = sub.iter().any(|capability| {
            matches!(
                capability,
                DeviceCapability::Input | DeviceCapability::Window
            )
        });
        let apps = self.effective_apps(&settings, allowed_apps, needs_apps)?;
        let now = self.clock.now();
        let deadline = Timestamp::from_unix_millis(
            now.unix_millis()
                .saturating_add(i64::from(minutes).saturating_mul(60_000)),
        );

        let mut sessions = self.sessions_write();
        // 先收尾旧的：库里同 bot 只允许一条 active，否则重启后"哪条是真的"就说不清了。
        self.store
            .end_live_device_sessions(
                bot_id,
                DeviceSessionState::Revoked.as_str(),
                now.unix_millis(),
            )
            .map_err(store_failed)?;

        let session = LiveSession::new(
            session_id,
            sub,
            apps,
            now,
            deadline,
            settings.max_actions_per_session,
        );
        self.store
            .insert_device_session(NewDeviceSession {
                id: session.id.clone(),
                bot_id: *bot_id,
                state: session.state.as_str().to_owned(),
                sub_json: session.sub_json(),
                allowed_apps_json: session.allowed_apps_json(),
                granted_at: Some(session.granted_at.unix_millis()),
                deadline: Some(session.deadline.unix_millis()),
                created_at: now.unix_millis(),
            })
            .map_err(store_failed)?;
        let status = session.status(bot_id, now);
        sessions.insert(*bot_id, session);
        drop(sessions);

        self.events.emit(Event::DeviceStatus {
            status: status.clone(),
        });
        Ok(status)
    }

    /// 一键撤销：库与内存都置 `revoked`，发事件。**幂等**。
    ///
    /// 没有活跃会话也成功（用户点了一个"已经没有东西可撤"的按钮，不该看到错误）。
    /// 内存里保留这一条，是为了让 `device_status` 还能回答"最近一次是 revoked"而不是 `none`。
    pub fn revoke(&self, bot_id: &BotId) -> Result<DeviceStatus, hbot_core::HbotError> {
        let settings = self.settings();
        let now = self.clock.now();
        let mut sessions = self.sessions_write();
        self.store
            .end_live_device_sessions(
                bot_id,
                DeviceSessionState::Revoked.as_str(),
                now.unix_millis(),
            )
            .map_err(store_failed)?;
        let status = match sessions.get_mut(bot_id) {
            Some(session) => {
                session.state = DeviceSessionState::Revoked;
                session.status(bot_id, now)
            }
            None => empty_status(bot_id, &settings),
        };
        drop(sessions);

        self.events.emit(Event::DeviceStatus {
            status: status.clone(),
        });
        Ok(status)
    }

    /// 扫一遍内存里的 live 会话，把过期的收尾成 `expired`（落库 + 改内存 + 发事件）。
    ///
    /// 供两处调用：1 秒 tick，以及测试（用 `ManualClock` 推时间，直接调一次，
    /// 不必真的等 30 分钟）。返回这一轮收尾了几条。
    ///
    /// **同步**且会阻塞（SQLite 是同步 API）：只该从 `spawn_blocking` 或测试里调用。
    pub fn sweep_expired(&self) -> Result<usize, hbot_core::HbotError> {
        let now = self.clock.now();
        let mut expired: Vec<DeviceStatus> = Vec::new();
        let mut failure: Option<hbot_core::HbotError> = None;
        {
            let mut sessions = self.sessions_write();
            for (bot_id, session) in sessions.iter_mut() {
                if !session.state.is_live() || now < session.deadline {
                    continue;
                }
                match self.store.end_live_device_sessions(
                    bot_id,
                    DeviceSessionState::Expired.as_str(),
                    now.unix_millis(),
                ) {
                    Ok(_) => {
                        session.state = DeviceSessionState::Expired;
                        expired.push(session.status(bot_id, now));
                    }
                    Err(error) => {
                        // 写库失败就**不改内存**：宁可晚 1 秒再试，也不要出现
                        // "内存说 expired、库里还是 active"的不一致。
                        failure = Some(store_failed(error));
                        break;
                    }
                }
            }
        }
        for status in &expired {
            self.events.emit(Event::DeviceStatus {
                status: status.clone(),
            });
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(expired.len()),
        }
    }

    /// 起 1 秒 tick（`AppState::open` 调用一次）。
    ///
    /// 持 [`Weak`]：`AppState`/登记表一析构就自然退出，不需要额外的关停协议。
    /// 没有异步运行时（某些同步测试直接 `AppState::open`）时只 `warn` 不 panic ——
    /// 到期结算缺席不影响正确性，闸门自己会拒绝过期会话。
    pub fn spawn_expiry_tick(self: &Arc<Self>) {
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::warn!(
                    "没有异步运行时，设备会话到期结算 tick 未启动（闸门仍会拒绝过期会话）"
                );
                return;
            }
        };
        let registry = Arc::downgrade(self);
        let cancel = Arc::clone(&self.cancel);
        let wake = Arc::clone(&self.wake);
        handle.spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            // 落后就跳过：宁可少扫几轮，不要为了"补扫"把一轮拆成几十次立即触发。
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = wake.notified() => break,
                }
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let Some(registry) = registry.upgrade() else {
                    break;
                };
                let result = tokio::task::spawn_blocking(move || registry.sweep_expired()).await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "设备会话到期结算失败，下一秒重试");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "设备会话到期结算任务被中止");
                    }
                }
            }
        });
    }

    /// 造一个闸门（`Weak` 指回自己）。
    ///
    /// 消费方（下一单元的 `sandbox_handler` 注入）只需要 `Arc<dyn DeviceGate>`；
    /// 登记表已经没了的时候放行是绝对不行的，所以退回 [`DeviceError::Failed`]（拒绝）。
    pub fn gate(self: &Arc<Self>) -> Arc<dyn DeviceGate> {
        Arc::new(RegistryGate {
            registry: Arc::downgrade(self),
        })
    }

    /// 闸门的唯一判据（顺序即 §4 的三道闸 + 会话 + 目标）。
    ///
    /// 顺序是有意的：先"宿主总开关"（最便宜、最全局），再"清单声明"（读文件），
    /// 再"宿主点名开的子能力"，最后才是"这一会话"。这样任何一个"未启用"都回它自己的
    /// toggle 名，用户照着提示改一处就能修好，而不是笼统的 `not granted`。
    pub(crate) fn authorize(
        &self,
        bot_id: &BotId,
        capability: DeviceCapability,
        target: &DeviceTarget,
    ) -> Result<(), DeviceError> {
        let settings = self.settings();
        if !settings.enabled {
            return Err(DeviceError::NotEnabled {
                toggle: "device.enabled".to_owned(),
            });
        }
        if !self.declared_capabilities().contains(&capability) {
            return Err(DeviceError::NotDeclared { capability });
        }
        if !settings.capabilities.contains(&capability) {
            return Err(DeviceError::NotEnabled {
                toggle: "device.capabilities".to_owned(),
            });
        }

        let now = self.clock.now();
        let decision = {
            let mut sessions = self.sessions_write();
            let Some(session) = sessions.get_mut(bot_id) else {
                return Err(DeviceError::NotGranted { capability });
            };
            if !session.state.is_live() || !session.sub.contains(&capability) {
                return Err(DeviceError::NotGranted { capability });
            }
            if now >= session.deadline {
                // tick 还没扫到：就地收尾。写库失败也照样拒绝
                // （宁可拒，不能因为库写不进去就放行）。
                if let Err(error) = self.store.end_live_device_sessions(
                    bot_id,
                    DeviceSessionState::Expired.as_str(),
                    now.unix_millis(),
                ) {
                    tracing::warn!(
                        bot = %bot_id,
                        %error,
                        "设备会话到期收尾写库失败（仍然按已到期拒绝）"
                    );
                }
                session.state = DeviceSessionState::Expired;
                GateDecision::Expired(session.status(bot_id, now))
            } else {
                match target {
                    // `input`/`window` 必须带窗口目标：没有进程名的输入注入等于"往当前焦点里打"，
                    // 那是全局控制，不是"操作某个应用"。宁可拒。
                    DeviceTarget::None
                        if !matches!(
                            capability,
                            DeviceCapability::Screen | DeviceCapability::Clipboard
                        ) =>
                    {
                        return Err(DeviceError::AppNotAllowed {
                            process: "<none>".to_owned(),
                        });
                    }
                    DeviceTarget::None => {}
                    DeviceTarget::Window { process }
                        if !session.allows_process(process)
                            || !host_ceiling_allows(&settings.allowed_apps, process) =>
                    {
                        return Err(DeviceError::AppNotAllowed {
                            process: process.clone(),
                        });
                    }
                    DeviceTarget::Window { .. } => {}
                }
                session.take_action()?;
                GateDecision::Allowed(target_process(target).map(str::to_owned))
            }
        };

        match decision {
            GateDecision::Expired(status) => {
                self.events.emit(Event::DeviceStatus { status });
                Err(DeviceError::Expired)
            }
            GateDecision::Allowed(process) => {
                // 审计在锁外写：一条审计写不进去不该把注入路径串行化。
                append_audit(
                    &self.store,
                    bot_id,
                    DeviceActor::BotCode,
                    capability.as_str(),
                    process.as_deref(),
                    "ok",
                    now,
                );
                Ok(())
            }
        }
    }

    /// Agent 工具的授权判据：**"这一刻有没有一条仍然生效、且包含这个子能力的会话"**。
    ///
    /// 为什么需要第四层：`ToolGateway` 的 `Capability` 步只判"项目清单声明了没有"，
    /// 那是**代码有没有资格要**；会话是**用户此刻批没批**。少了这一层，撤销设备会话
    /// 停得住 bot 代码、停不住 IDE 里的 Agent（`read_screen` 会一直能读屏），
    /// 那"限时会话 + 一键撤销"就只对一半消费者有效。
    ///
    /// 与 [`Self::authorize`] 的差别只有一处：Agent 会话没有 `BotId`，所以"哪一条会话"
    /// 的答案是"当前项目里**任意**一条"。这不构成放宽 —— 撤销、到期、改窄 `device.json`
    /// 都会立刻让这个答案变成拒绝，而每个工具调用都会重问一遍（不缓存、不快照）。
    pub(crate) fn authorize_tool(&self, capability: DeviceCapability) -> Result<(), DeviceError> {
        let settings = self.settings();
        if !settings.enabled {
            return Err(DeviceError::NotEnabled {
                toggle: "device.enabled".to_owned(),
            });
        }
        if !self.declared_capabilities().contains(&capability) {
            return Err(DeviceError::NotDeclared { capability });
        }
        if !settings.capabilities.contains(&capability) {
            return Err(DeviceError::NotEnabled {
                toggle: "device.capabilities".to_owned(),
            });
        }
        // 没有目标的能力才允许 Agent 直接要：`input`/`window` 必须先指定窗口，
        // 那是"操作某个应用"，不是"看一眼屏幕"。与 `authorize` 里 `DeviceTarget::None`
        // 那条判据同一个理由（BotCode 路径的分类，这里按工具路径再判一次）。
        if !matches!(
            capability,
            DeviceCapability::Screen | DeviceCapability::Ocr | DeviceCapability::Clipboard
        ) {
            return Err(DeviceError::AppNotAllowed {
                process: "<none>".to_owned(),
            });
        }

        let now = self.clock.now();
        // 取**一条**可用会话并记住它是谁的：审计表的 `bot_id` 不可空
        // （`device_audit` 外键指 `bots.id`），而"这条 Agent 调用是按谁的授权放行的"
        // 恰好是有用的记录 —— 比编一个空值更诚实。按 `BotId` 排序取第一条，
        // 让同一次调用的审计在多次运行间稳定。
        let granted_by = {
            let sessions = self.sessions_read();
            let mut candidates: Vec<_> = sessions
                .iter()
                .filter(|(_, session)| {
                    session.state.is_live()
                        && session.sub.contains(&capability)
                        && now < session.deadline
                })
                .map(|(bot_id, _)| *bot_id)
                .collect();
            candidates.sort();
            candidates.first().copied()
        };
        let Some(bot_id) = granted_by else {
            return Err(DeviceError::NotGranted { capability });
        };

        // 审计记 actor=`agent_tool`、目标为空：这条路径没有应用白名单可比。
        append_audit(
            &self.store,
            &bot_id,
            DeviceActor::AgentTool,
            capability.as_str(),
            None,
            "ok",
            now,
        );
        Ok(())
    }

    /// 读当前项目的 `hbot.json`，取 `permissions.device.sub`。
    ///
    /// 读不到（没开项目 / 没清单 / 清单坏了 / 没写 `permissions.device`）一律当**没声明**：
    /// 闸门的方向只能是"证明声明过才放行"，不能因为文件读不出来就放行。
    ///
    /// 读取本身在 [`capabilities::declared_at`]：能力声明层（`CapabilityGate`）与本登记表
    /// 问的是同一个问题，必须同一份实现（两份会打架，见
    /// `docs/09-device-capability.md` §4.1 与 §4.4 的判定表）。
    pub(crate) fn declared_capabilities(&self) -> BTreeSet<DeviceCapability> {
        let root = self.project_root_read().clone();
        let Some(root) = root else {
            return BTreeSet::new();
        };
        capabilities::declared_at(&root)
    }

    /// 生效子能力 = 清单声明 ∩ 宿主设置 ∩ 本次请求；空集 → `-32002`。
    fn effective_sub(
        &self,
        settings: &DeviceSettings,
        requested: &BTreeSet<DeviceCapability>,
    ) -> Result<BTreeSet<DeviceCapability>, hbot_core::HbotError> {
        if !settings.enabled {
            return Err(invalid(DeviceError::NotEnabled {
                toggle: "device.enabled".to_owned(),
            }));
        }
        let declared = self.declared_capabilities();
        let effective: BTreeSet<DeviceCapability> = requested
            .intersection(&declared)
            .filter(|capability| settings.capabilities.contains(capability))
            .copied()
            .collect();
        if !effective.is_empty() {
            return Ok(effective);
        }
        // 三个集合里至少一个为空/不相交。把三边都写出来，用户才知道该改哪儿。
        let detail = match requested.iter().next().copied() {
            None => "请求里没有子能力".to_owned(),
            Some(capability) if declared.is_empty() => {
                DeviceError::NotDeclared { capability }.to_string()
            }
            Some(_) => format!(
                "请求 [{}] ∩ 清单声明 [{}] ∩ 宿主设置 [{}] 为空集，请检查 hbot.json 的 \
                 permissions.device.sub 与 device.json 的 device.capabilities",
                names(requested.iter().copied()),
                names(declared.iter().copied()),
                names(settings.capabilities.iter().copied()),
            ),
        };
        Err(hbot_core::HbotError::invalid(format!(
            "没有可生效的设备子能力：{detail}"
        )))
    }

    /// 生效应用白名单 = 请求 ∩ 宿主上限；**宿主上限为空集时视为"没有设上限"**。
    ///
    /// 为什么这里与 [`Self::effective_sub`] 的空集语义相反：
    ///
    /// - `device.capabilities` 是"这台机器开不开这类能力"，空集是**安全的默认**（全拒），
    ///   因为能力一旦开了一类就是整类可用。
    /// - `device.allowedApps` 是**上限**而不是开关：真正的白名单是用户在授权对话框里
    ///   这一次点出来的那一份（`device_grant` 的 `allowedApps`，必填且非空）。
    ///   若把"宿主没写上限"也解释成全拒，则用户必须在两个文件里各抄一遍同一份应用名
    ///   才能用一次输入——那不是安全，是让人放弃配置，最后把整份名单写宽。
    ///
    /// 所以：宿主写了上限就当硬上限（求交集，交集为空直接 `-32002`）；没写就只受会话约束。
    /// 闸门侧（[`Self::authorize`]）用的是同一个 [`host_ceiling_allows`]，两处不会打架。
    fn effective_apps(
        &self,
        settings: &DeviceSettings,
        requested: Vec<String>,
        needs_apps: bool,
    ) -> Result<Vec<String>, hbot_core::HbotError> {
        let requested = normalize_apps(requested);
        if !needs_apps {
            // 只截屏 / 只用剪贴板：没有"操作哪个应用"这回事，空表是正常状态而不是缺口。
            // 若这里也要求非空，则一个只截图的 bot 必须凭空编一个进程名才能开会话。
            return Ok(requested);
        }
        let declared = self.declared_apps();
        let ceiling = normalize_apps(settings.allowed_apps.clone());
        // 三边求交：清单**提议**（这个 bot 想控制哪些应用）、宿主设置**允许的**上限、
        // 本次请求（用户此刻点头的那几个）。任一边是空的，结论就是"没有可生效的应用"。
        let effective: Vec<String> = requested
            .iter()
            .filter(|app| declared.contains(app))
            .filter(|app| ceiling.is_empty() || ceiling.contains(app))
            .cloned()
            .collect();
        if !effective.is_empty() {
            return Ok(effective);
        }
        Err(hbot_core::HbotError::invalid(format!(
            "没有可生效的应用白名单：请求 [{}] ∩ 清单声明 permissions.device.apps [{}] ∩ \
             宿主设置 device.allowedApps [{}] 为空集",
            requested.join(", "),
            declared.join(", "),
            if ceiling.is_empty() {
                "（未设上限）".to_owned()
            } else {
                ceiling.join(", ")
            },
        )))
    }

    /// 清单里的应用白名单（`permissions.device.apps`），已归一化。
    ///
    /// 与 [`Self::declared_capabilities`] 同源同口径：读不到清单就是空表（= 什么都没提议）。
    /// 这份名单还会通过 `Sandbox::set_device_apps` 交给设备后端做第二道检查，
    /// 因为后端的 `input` 也有一个 `allowed_apps` 参数（见 `hbot_core::DeviceBackend`）。
    pub fn declared_apps(&self) -> Vec<String> {
        let root = self.project_root_read().clone();
        let Some(root) = root else {
            return Vec::new();
        };
        match hbot_code::manifest::load(&root) {
            Ok(manifest) => manifest
                .permissions
                .device
                .map(|device| normalize_apps(device.apps))
                .unwrap_or_default(),
            Err(error) => {
                tracing::debug!(
                    root = %root.display(),
                    %error,
                    "读不到项目清单，设备应用白名单按未声明处理"
                );
                Vec::new()
            }
        }
    }

    /// 生效时长：缺省取设置的 `defaultMinutes`，超上限截到 `maxMinutes` 并 `warn`。
    fn effective_minutes(&self, settings: &DeviceSettings, minutes: Option<u32>) -> u32 {
        let requested = minutes.unwrap_or(settings.default_minutes);
        if requested == 0 {
            tracing::warn!("device_grant 的 minutes=0 会让会话立刻到期，按 1 分钟开");
            return 1;
        }
        if requested > settings.max_minutes {
            // 善意超限：用户想开久一点。这里**收紧**到上限（不是放宽、也不是拒绝），
            // 与"静默放宽安全"是相反方向；真正该拒绝的是空交集（见 Self::effective_sub）。
            tracing::warn!(
                requested,
                limit = settings.max_minutes,
                "device_grant 的 minutes 超过宿主上限，按上限生效"
            );
            return settings.max_minutes;
        }
        requested
    }

    fn settings_read(&self) -> RwLockReadGuard<'_, DeviceSettings> {
        self.settings.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn settings_write(&self) -> RwLockWriteGuard<'_, DeviceSettings> {
        self.settings
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions_read(&self) -> MutexGuard<'_, HashMap<BotId, LiveSession>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions_write(&self) -> MutexGuard<'_, HashMap<BotId, LiveSession>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn project_root_read(&self) -> RwLockReadGuard<'_, Option<PathBuf>> {
        self.project_root
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn project_root_write(&self) -> RwLockWriteGuard<'_, Option<PathBuf>> {
        self.project_root
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for DeviceRegistry {
    fn drop(&mut self) {
        // 让 tick 立刻醒来退出，而不是等下一次 interval 到期才发现 Weak 已经死了。
        self.cancel.store(true, Ordering::Relaxed);
        self.wake.notify_waiters();
    }
}

impl std::fmt::Debug for DeviceRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceRegistry")
            .field("data_dir", &self.data_dir)
            .field("settings", &self.settings_read())
            .field("sessions", &self.sessions_read().len())
            .finish_non_exhaustive()
    }
}

/// 闸门在某次 `authorize` 里得出的结论（锁内算，锁外执行副作用）。
enum GateDecision {
    /// 已就地收尾成 `expired`，要把这个状态广播出去。
    Expired(DeviceStatus),
    /// 放行；带目标进程名（审计用）。
    Allowed(Option<String>),
}

/// 注入给沙箱的闸门实现：只持 [`Weak`]，绝不延长登记表的寿命。
struct RegistryGate {
    registry: Weak<DeviceRegistry>,
}

impl DeviceGate for RegistryGate {
    fn authorize(
        &self,
        bot_id: &BotId,
        capability: DeviceCapability,
        target: &DeviceTarget,
    ) -> Result<(), DeviceError> {
        let Some(registry) = self.registry.upgrade() else {
            // 宿主已经开始关停了：任何设备动作都不该再动。
            return Err(DeviceError::Failed {
                message: "设备登记表已随宿主关闭而释放".to_owned(),
            });
        };
        registry.authorize(bot_id, capability, target)
    }
}

/// 没有会话时的状态：`state: "none"`、`sub: []`、白名单用**设置里的值**（其余 null）。
///
/// 为什么白名单不是空数组：UI 要在"还没开会话"时就告诉用户"开了之后能操作哪些应用"，
/// 这个值来自设置而不是会话 —— 会话还没建立，但它将要受什么约束是已知的。
fn empty_status(bot_id: &BotId, settings: &DeviceSettings) -> DeviceStatus {
    DeviceStatus {
        bot_id: *bot_id,
        session_id: None,
        state: DeviceSessionState::None.as_str().to_owned(),
        sub: Vec::new(),
        allowed_apps: settings.allowed_apps.clone(),
        granted_at: None,
        deadline: None,
        remaining_ms: None,
    }
}

/// 宿主级应用上限的判断：**空表 = 没有设上限**（见 [`DeviceRegistry::effective_apps`]）。
///
/// 与 [`LiveSession::allows_process`] 的区别：那个空表 = 全拒（会话白名单是用户点出来的，
/// 没点就是没授）；这里空表 = 不设限（宿主没写上限不该把整条链路掐死）。
fn host_ceiling_allows(ceiling: &[String], process: &str) -> bool {
    if ceiling.is_empty() {
        return true;
    }
    let process = process.trim().to_ascii_lowercase();
    ceiling
        .iter()
        .any(|app| app.trim().to_ascii_lowercase() == process)
}

/// 白名单归一化：去空白、转小写、去重，保持首次出现的顺序。
///
/// `DeviceStatus::allowed_apps` 的契约是"映像名，小写"，而闸门比较是大小写不敏感的：
/// 这里统一成小写，等于让"库里存的东西"和"文档写的形状"一致，省掉后来人的一次困惑。
fn normalize_apps(apps: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(apps.len());
    for app in apps {
        let app = app.trim().to_ascii_lowercase();
        if app.is_empty() || out.contains(&app) {
            continue;
        }
        out.push(app);
    }
    out
}

/// `-32002`：参数/前置条件不满足（未启用、清单没声明、交集为空）。
fn invalid(error: DeviceError) -> hbot_core::HbotError {
    hbot_core::HbotError::invalid(error.to_string())
}

/// 把一组子能力拼成 `screen, input` 这样的可读列表（错误文案用）。
fn names(capabilities: impl Iterator<Item = DeviceCapability>) -> String {
    capabilities
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use hbot_core::{
        ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapabilityReport, DeviceError,
        IdGenerator, InputAction, OcrRequest, OcrText, ProjectId, Screenshot, ScreenshotRequest,
        SystemClock, WindowInfo,
    };
    use hbot_store::{Bot, Project};

    use super::*;
    use crate::app::device::agent_backend::SessionScopedBackend;
    use crate::app::ids::{ManualClock, UuidIds};

    /// 每个测试自己的临时目录（与 `crates/hbot-rpc/tests` 的做法一致，不引 dev-dependency）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hbot-device-registry-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// 一组测具：临时目录（含已 seed 的 bot 与清单）+ 登记表 + 手动时钟。
    struct Fixture {
        dir: TempDir,
        registry: Arc<DeviceRegistry>,
        clock: Arc<ManualClock>,
        events: Arc<EventBus>,
        /// 与登记表里同一个 `Arc<Store>`：审计断言要直接读表（登记表不暴露 store）。
        store: Arc<Store>,
        bot_id: BotId,
    }

    impl Fixture {
        /// `enabled` 与子能力由调用方指定（每个测试要的闸门状态不同）。
        fn new(label: &str, enabled: bool, capabilities: &[DeviceCapability]) -> Self {
            let dir = TempDir::new(label);
            let data_dir = dir.path.join("data");
            std::fs::create_dir_all(&data_dir).expect("建 data_dir");
            let store = Arc::new(Store::open(data_dir.join("hbot.db")).expect("开库"));
            let clock = Arc::new(ManualClock::new(Timestamp::from_unix_millis(1_000_000)));
            let events = Arc::new(EventBus::new());
            let registry = Arc::new(DeviceRegistry::new(
                data_dir.clone(),
                Arc::clone(&store),
                Arc::clone(&clock) as Arc<dyn Clock>,
                Arc::clone(&events),
            ));

            let settings = DeviceSettings {
                enabled,
                capabilities: capabilities.iter().copied().collect(),
                ..DeviceSettings::default()
            };
            registry.replace_settings(settings.clone());
            DeviceSettings::save(&data_dir, &settings).expect("写设置");

            let project_dir = dir.path.join("project");
            std::fs::create_dir_all(project_dir.join("src")).expect("建项目目录");
            // 清单校验要求 entry 指向的文件真的存在（`src/main.ts` 是默认入口）。
            std::fs::write(project_dir.join("src").join("main.ts"), "export {};\n")
                .expect("写入口文件");
            std::fs::write(
                project_dir.join("hbot.json"),
                r#"{"name":"device-test","platform":{"kind":"local"},"permissions":{"http":[],"device":{"sub":["screen","ocr","input","window","clipboard"],"apps":["game.exe","notepad.exe","anything.exe","other.exe"]}}}"#,
            )
            .expect("写清单");
            registry.note_project_root(Some(project_dir.clone()));

            let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0x1234));
            store
                .insert_project(&Project {
                    id: project_id,
                    name: "device".to_owned(),
                    path: project_dir.display().to_string(),
                    created_at: 0,
                })
                .expect("insert_project");
            let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0x5678));
            store
                .insert_bot(&Bot {
                    id: bot_id,
                    project_id,
                    name: "device-bot".to_owned(),
                    platform: "local".to_owned(),
                    config_json: "{}".to_owned(),
                    enabled: true,
                    created_at: 0,
                })
                .expect("insert_bot");

            Self {
                dir,
                registry,
                clock,
                events,
                store,
                bot_id,
            }
        }
    }

    fn caps(capabilities: &[DeviceCapability]) -> BTreeSet<DeviceCapability> {
        capabilities.iter().copied().collect()
    }

    fn invalid_message(error: &hbot_core::HbotError) -> &str {
        match error {
            hbot_core::HbotError::Invalid { message } => message,
            other => panic!("期望 -32002 Invalid，实际：{other:?}"),
        }
    }

    #[test]
    fn status_without_a_session_is_none_and_uses_the_configured_apps() {
        let fixture = Fixture::new("status-none", true, &[DeviceCapability::Screen]);
        let status = fixture.registry.status(&fixture.bot_id);
        assert_eq!(status.state, "none");
        assert!(status.sub.is_empty());
        assert_eq!(status.allowed_apps, Vec::<String>::new());
        assert!(status.session_id.is_none());
        assert!(status.remaining_ms.is_none());
    }

    #[test]
    fn grant_then_status_is_active_with_the_intersection() {
        let fixture = Fixture::new(
            "grant",
            true,
            &[DeviceCapability::Screen, DeviceCapability::Input],
        );
        let session_id = UuidIds.session_id().to_string();
        let status = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[
                    DeviceCapability::Screen,
                    DeviceCapability::Input,
                    // ocr 没在清单里也没在设置里：必须被求交集丢掉。
                    DeviceCapability::Ocr,
                ]),
                vec!["Game.exe".to_owned()],
                Some(2),
                session_id.clone(),
            )
            .expect("grant");
        assert_eq!(status.state, "active");
        assert_eq!(status.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(status.sub, vec!["screen".to_owned(), "input".to_owned()]);
        assert_eq!(status.allowed_apps, vec!["game.exe".to_owned()]);
        assert_eq!(status.remaining_ms, Some(120_000));
    }

    #[test]
    fn grant_is_refused_when_the_host_switch_is_off() {
        let fixture = Fixture::new("grant-off", false, &[DeviceCapability::Screen]);
        let error = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                None,
                "s1".to_owned(),
            )
            .expect_err("关着的时候不能开");
        assert_eq!(error.code(), -32002);
        assert!(
            invalid_message(&error).contains("device.enabled"),
            "文案要点出该改哪个开关，实际：{}",
            invalid_message(&error)
        );
    }

    #[test]
    fn grant_is_refused_when_the_intersection_is_empty() {
        // 清单声明了 screen，但宿主设置一个都没开 → 交集为空。
        let fixture = Fixture::new("grant-empty", true, &[]);
        let error = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                None,
                "s1".to_owned(),
            )
            .expect_err("空交集必须拒绝");
        assert_eq!(error.code(), -32002);
        assert!(
            invalid_message(&error).contains("device.capabilities"),
            "文案要点出是宿主设置那边空，实际：{}",
            invalid_message(&error)
        );
    }

    #[test]
    fn grant_is_refused_when_the_manifest_declares_nothing() {
        let fixture = Fixture::new("grant-undeclared", true, &[DeviceCapability::Screen]);
        // 把项目根指到一个没有 hbot.json 的目录 → 当没声明。
        fixture
            .registry
            .note_project_root(Some(fixture.dir.path.join("no-manifest")));
        let error = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                None,
                "s1".to_owned(),
            )
            .expect_err("没声明就不能开");
        assert_eq!(error.code(), -32002);
        assert!(
            invalid_message(&error).contains("HBOT_DEVICE_NOT_DECLARED"),
            "要说是「清单没声明」，实际：{}",
            invalid_message(&error)
        );
    }

    #[test]
    fn minutes_over_the_cap_are_clamped_to_the_settings_maximum() {
        let fixture = Fixture::new("clamp", true, &[DeviceCapability::Screen]);
        let status = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(999),
                "s1".to_owned(),
            )
            .expect("grant");
        assert_eq!(status.remaining_ms, Some(30 * 60_000));
    }

    #[test]
    fn authorize_walks_the_gates_in_order() {
        // 设置里只点名 `screen` + `input`；`window` 在清单里声明了但宿主没开。
        let fixture = Fixture::new(
            "authorize",
            true,
            &[DeviceCapability::Screen, DeviceCapability::Input],
        );

        // 没有会话 → NotGranted。
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect_err("没会话");
        assert!(matches!(error, DeviceError::NotGranted { .. }), "{error:?}");

        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");

        // 成功：screen + None 目标合法，计数 +1。
        fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect("screen + None 合法");

        // 清单声明了、但宿主设置没点名 → NotEnabled{device.capabilities}。
        // 这一闸在会话闸之前：没被宿主打开的能力，连"这次会话里有没有"都不必问。
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Window,
                &DeviceTarget::Window {
                    process: "game.exe".to_owned(),
                },
            )
            .expect_err("window 没在设置里打开");
        assert_eq!(error.toggle(), Some("device.capabilities"), "{error:?}");

        // 会话里没这个子能力 → NotGranted。
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Input,
                &DeviceTarget::Window {
                    process: "game.exe".to_owned(),
                },
            )
            .expect_err("input 没授予");
        assert!(matches!(error, DeviceError::NotGranted { .. }), "{error:?}");
    }

    #[test]
    fn authorize_denies_window_targets_outside_the_whitelist() {
        let fixture = Fixture::new("authorize-app", true, &[DeviceCapability::Input]);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["game.exe".to_owned()],
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Input,
                &DeviceTarget::Window {
                    process: "other.exe".to_owned(),
                },
            )
            .expect_err("不在白名单");
        assert!(
            matches!(&error, DeviceError::AppNotAllowed { process } if process == "other.exe"),
            "{error:?}"
        );

        // input 不带窗口目标同样拒（宁可拒）。
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Input,
                &DeviceTarget::None,
            )
            .expect_err("input 必须有窗口目标");
        assert!(
            matches!(error, DeviceError::AppNotAllowed { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn authorize_stops_at_the_action_limit() {
        let fixture = Fixture::new("authorize-limit", true, &[DeviceCapability::Screen]);
        let mut settings = fixture.registry.settings();
        settings.max_actions_per_session = 1;
        fixture.registry.replace_settings(settings);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");
        fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect("第 1 次");
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect_err("第 2 次超限");
        assert!(
            matches!(&error, DeviceError::HostLimit { detail } if detail == "每会话动作数上限"),
            "{error:?}"
        );
    }

    #[test]
    fn authorize_settles_an_overdue_session_as_expired_first() {
        let fixture = Fixture::new("authorize-expire", true, &[DeviceCapability::Screen]);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");
        let mut receiver = fixture.events.subscribe();
        fixture.clock.advance(61_000);
        let error = fixture
            .registry
            .authorize(
                &fixture.bot_id,
                DeviceCapability::Screen,
                &DeviceTarget::None,
            )
            .expect_err("过期了");
        assert!(matches!(error, DeviceError::Expired), "{error:?}");
        assert_eq!(fixture.registry.status(&fixture.bot_id).state, "expired");
        let event = receiver.try_recv().expect("应该有 device.status 事件");
        assert!(matches!(event, Event::DeviceStatus { .. }), "{event:?}");
    }

    #[test]
    fn sweep_expired_reports_the_number_it_settled() {
        let fixture = Fixture::new("sweep", true, &[DeviceCapability::Screen]);
        assert_eq!(fixture.registry.sweep_expired().expect("空扫"), 0);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");
        assert_eq!(fixture.registry.sweep_expired().expect("还没到期"), 0);
        fixture.clock.advance(60_000);
        assert_eq!(fixture.registry.sweep_expired().expect("到期"), 1);
        assert_eq!(fixture.registry.sweep_expired().expect("幂等"), 0);
        assert_eq!(fixture.registry.status(&fixture.bot_id).state, "expired");
    }

    #[test]
    fn revoke_keeps_the_latest_session_queryable() {
        let fixture = Fixture::new("revoke", true, &[DeviceCapability::Screen]);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Screen]),
                Vec::new(),
                Some(1),
                "s1".to_owned(),
            )
            .expect("grant");
        let status = fixture.registry.revoke(&fixture.bot_id).expect("撤销");
        assert_eq!(status.state, "revoked");
        assert!(status.session_id.is_some(), "要保留一条可查询的最近会话");
        assert!(status.remaining_ms.is_none(), "结束的会话没有剩余时长");
        // 幂等。
        let again = fixture.registry.revoke(&fixture.bot_id).expect("再撤一次");
        assert_eq!(again.state, "revoked");
    }

    #[test]
    fn gate_refuses_after_the_registry_is_gone() {
        let fixture = Fixture::new("gate", true, &[DeviceCapability::Screen]);
        let gate = fixture.registry.gate();
        let bot_id = fixture.bot_id;
        let error = gate
            .authorize(&bot_id, DeviceCapability::Screen, &DeviceTarget::None)
            .expect_err("没会话");
        assert!(matches!(error, DeviceError::NotGranted { .. }), "{error:?}");
        drop(fixture);
        let error = gate
            .authorize(&bot_id, DeviceCapability::Screen, &DeviceTarget::None)
            .expect_err("登记表没了必须拒");
        assert!(matches!(error, DeviceError::Failed { .. }), "{error:?}");
    }

    #[test]
    fn expiry_tick_shuts_down_when_the_registry_is_dropped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("起运行时");
        let _guard = runtime.enter();
        let (registry, gate) = {
            let fixture = Fixture::new("tick", true, &[DeviceCapability::Screen]);
            fixture.registry.spawn_expiry_tick();
            (Arc::clone(&fixture.registry), fixture.registry.gate())
        };
        // 登记表被丢下后，闸门必须开始拒绝（tick 那边也是同一判据：Weak 失效即退出）。
        drop(registry);
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0x9999));
        let error = gate
            .authorize(&bot_id, DeviceCapability::Screen, &DeviceTarget::None)
            .expect_err("登记表没了");
        assert!(matches!(error, DeviceError::Failed { .. }), "{error:?}");
    }

    #[test]
    fn a_registry_built_with_the_system_clock_defaults_to_deny_all() {
        let dir = TempDir::new("system-clock");
        let store = Arc::new(Store::open(dir.path.join("hbot.db")).expect("开库"));
        let registry = DeviceRegistry::new(
            dir.path.clone(),
            store,
            Arc::new(SystemClock),
            Arc::new(EventBus::new()),
        );
        assert!(!registry.settings().enabled, "没有设置文件 → 默认关");
        assert!(
            registry.declared_capabilities().is_empty(),
            "没有项目 → 没声明"
        );
        assert_eq!(registry.sweep_expired().expect("空扫"), 0);
    }

    #[test]
    fn names_joins_capability_names_for_error_messages() {
        assert_eq!(
            names([DeviceCapability::Screen, DeviceCapability::Input].into_iter()),
            "screen, input"
        );
        assert_eq!(names(std::iter::empty()), "");
    }

    #[test]
    fn normalize_apps_trims_lowercases_and_dedupes() {
        assert_eq!(
            normalize_apps(vec![
                " Game.exe ".to_owned(),
                "game.exe".to_owned(),
                String::new(),
                "Tool.EXE".to_owned()
            ]),
            vec!["game.exe".to_owned(), "tool.exe".to_owned()]
        );
    }

    // ---- 宿主级应用上限（`device.allowedApps`）----
    //
    // 三条测试钉住的是"设置里那列 allowedApps 到底有没有用"。在它们之前，那一列只被
    // `device_status` 拿去显示，闸门完全不看——于是"宿主说只许这两个应用"是一句空话。
    // 空集的语义与 `capabilities` 相反（见 `DeviceSettings::allowed_apps` 的文档），
    // 所以第四条钉住"空表不设限"。

    #[test]
    fn grant_intersects_the_request_with_the_host_ceiling() {
        let fixture = Fixture::new("ceiling-grant", true, &[DeviceCapability::Input]);
        let mut settings = fixture.registry.settings();
        settings.allowed_apps = vec!["game.exe".to_owned(), "notepad.exe".to_owned()];
        fixture.registry.replace_settings(settings);

        let status = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["Game.exe".to_owned(), "other.exe".to_owned()],
                Some(1),
                "s-ceiling".to_owned(),
            )
            .expect("grant");
        assert_eq!(
            status.allowed_apps,
            vec!["game.exe".to_owned()],
            "只在请求里、不在上限里的 other.exe 必须被丢掉"
        );
    }

    #[test]
    fn grant_is_refused_when_the_host_ceiling_excludes_everything() {
        let fixture = Fixture::new("ceiling-empty", true, &[DeviceCapability::Input]);
        let mut settings = fixture.registry.settings();
        settings.allowed_apps = vec!["game.exe".to_owned()];
        fixture.registry.replace_settings(settings);

        let error = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["other.exe".to_owned()],
                Some(1),
                "s-none".to_owned(),
            )
            .expect_err("交集为空必须拒绝");
        let message = invalid_message(&error);
        assert!(message.contains("allowedApps"), "{message}");
        // 拒绝之后不能留下一份"看起来生效"的会话。
        assert_eq!(fixture.registry.status(&fixture.bot_id).state, "none");
    }

    #[test]
    fn the_host_ceiling_is_rechecked_on_every_call() {
        let fixture = Fixture::new("ceiling-live", true, &[DeviceCapability::Input]);
        // 先不含上限地授权（空表 = 不设限），会话里允许 game.exe。
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["game.exe".to_owned()],
                Some(5),
                "s-live".to_owned(),
            )
            .expect("grant");
        let target = DeviceTarget::Window {
            process: "game.exe".to_owned(),
        };
        fixture
            .registry
            .authorize(&fixture.bot_id, DeviceCapability::Input, &target)
            .expect("没有上限时放行");

        // 会话中途把上限改窄：旧会话必须立刻跟着收紧（这正是"每次调用都再查一遍"的意义）。
        let mut settings = fixture.registry.settings();
        settings.allowed_apps = vec!["notepad.exe".to_owned()];
        fixture.registry.replace_settings(settings);
        let error = fixture
            .registry
            .authorize(&fixture.bot_id, DeviceCapability::Input, &target)
            .expect_err("上限收紧后必须拒");
        assert!(
            matches!(&error, DeviceError::AppNotAllowed { process } if process == "game.exe"),
            "{error:?}"
        );
    }

    #[test]
    fn grant_is_refused_when_the_manifest_does_not_declare_the_app() {
        let fixture = Fixture::new("apps-undeclared", true, &[DeviceCapability::Input]);
        let error = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["never-declared.exe".to_owned()],
                Some(1),
                "s-undeclared".to_owned(),
            )
            .expect_err("清单没提议的应用不能生效");
        let message = invalid_message(&error);
        assert!(message.contains("清单声明"), "{message}");
        assert_eq!(fixture.registry.status(&fixture.bot_id).state, "none");
    }

    #[test]
    fn an_empty_host_ceiling_means_no_ceiling_not_deny_all() {
        let fixture = Fixture::new("ceiling-absent", true, &[DeviceCapability::Input]);
        assert!(
            fixture.registry.settings().allowed_apps.is_empty(),
            "夹具默认不写上限"
        );
        let status = fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input]),
                vec!["anything.exe".to_owned()],
                Some(1),
                "s-absent".to_owned(),
            )
            .expect("没有上限时不该拒绝");
        assert_eq!(status.allowed_apps, vec!["anything.exe".to_owned()]);
        assert!(host_ceiling_allows(&[], "anything.exe"));
        assert!(!host_ceiling_allows(
            &["game.exe".to_owned()],
            "anything.exe"
        ));
        assert!(host_ceiling_allows(&["GAME.exe".to_owned()], "game.exe"));
    }
    // ---- Agent 工具路径的会话判据（`agent_backend::SessionScopedBackend`） ----
    //
    // 这组测试存在的理由：`ToolGateway` 的 `Capability` 步只判"清单声明了没有"，
    // 那是"代码有没有资格要"。撤销/到期必须也能停住 IDE 里的 Agent，
    // 否则"限时会话 + 一键撤销"只对 bot 代码那一半消费者有效。

    /// 数调用次数的假后端（只关心"判据之前/之后有没有被碰到"）。
    #[derive(Default)]
    struct CountingBackend {
        calls: AtomicUsize,
    }

    impl CountingBackend {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl DeviceBackend for CountingBackend {
        fn capabilities(&self) -> DeviceCapabilityReport {
            DeviceCapabilityReport {
                ocr: true,
                ..DeviceCapabilityReport::default()
            }
        }

        fn screenshot(&self, _request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(DeviceError::Failed {
                message: "fake".to_owned(),
            })
        }

        fn ocr(&self, _request: &OcrRequest) -> Result<OcrText, DeviceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(OcrText {
                text: "fake".to_owned(),
                truncated: false,
            })
        }

        fn input(&self, _action: &InputAction, _apps: &[String]) -> Result<(), DeviceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        fn clipboard(&self, _op: &ClipboardOp) -> Result<ClipboardValue, DeviceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(DeviceError::Failed {
                message: "fake".to_owned(),
            })
        }
    }

    fn ocr_request() -> OcrRequest {
        OcrRequest {
            target: DeviceTarget::None,
            max_chars: 100,
        }
    }

    #[test]
    fn an_agent_tool_call_without_a_live_session_never_reaches_the_backend() {
        let fixture = Fixture::new("agent-no-session", true, &[DeviceCapability::Ocr]);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);

        let error = backend
            .ocr(&ocr_request())
            .expect_err("没有生效会话必须拒绝");
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_GRANTED");
        assert_eq!(counting.calls(), 0, "判据在装饰器里，后端不能被碰到");
    }

    #[test]
    fn an_agent_tool_call_uses_a_live_session_and_revocation_stops_the_next_one() {
        let fixture = Fixture::new("agent-live", true, &[DeviceCapability::Ocr]);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Ocr]),
                Vec::new(),
                Some(5),
                "s-agent".to_owned(),
            )
            .expect("grant");

        let text = backend.ocr(&ocr_request()).expect("有会话应放行");
        assert_eq!(text.text, "fake");
        assert_eq!(counting.calls(), 1);

        // 撤销之后**同一个装饰器实例**的下一次调用就必须拒绝（不快照授权）。
        fixture.registry.revoke(&fixture.bot_id).expect("撤销");
        let error = backend.ocr(&ocr_request()).expect_err("撤销后必须拒绝");
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_GRANTED");
        assert_eq!(counting.calls(), 1, "撤销后后端一次都不能多调");
    }

    #[test]
    fn an_agent_tool_call_is_refused_after_the_session_expires() {
        let fixture = Fixture::new("agent-expired", true, &[DeviceCapability::Ocr]);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Ocr]),
                Vec::new(),
                Some(1),
                "s-exp".to_owned(),
            )
            .expect("grant");
        assert!(backend.ocr(&ocr_request()).is_ok());

        // 手动时钟往前推过 1 分钟：到期结算还没被 tick 扫到，判据也必须已经收紧。
        fixture.clock.advance(61_000);
        let error = backend.ocr(&ocr_request()).expect_err("到期后必须拒绝");
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_GRANTED");
        assert_eq!(counting.calls(), 1);
    }

    #[test]
    fn an_agent_tool_call_needs_the_capability_switched_on_in_the_host_settings() {
        // 会话里有 ocr，但宿主设置没开这个子能力：`grant` 都批不下来，
        // 这里直接断言判据本身也拒（顺序：总开关 → 清单 → 设置 → 会话）。
        let fixture = Fixture::new("agent-settings", true, &[DeviceCapability::Screen]);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        let error = backend.ocr(&ocr_request()).expect_err("设置没开必须拒绝");
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_ENABLED", "{}", error);
        assert_eq!(counting.calls(), 0);
    }

    #[test]
    fn an_agent_tool_call_is_refused_when_the_manifest_declares_nothing() {
        let fixture = Fixture::new("agent-manifest", true, &[DeviceCapability::Ocr]);
        // 清空项目根：读不到清单 = 没声明（fail-closed，不 panic）。
        fixture.registry.note_project_root(None);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        let error = backend.ocr(&ocr_request()).expect_err("没声明必须拒绝");
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_DECLARED");
        assert_eq!(counting.calls(), 0);
    }

    #[test]
    fn agent_tools_never_get_input_or_window_even_with_a_session() {
        // `input`/`window` 必须有窗口目标；Agent 侧没有 target，所以一律拒。
        let fixture = Fixture::new(
            "agent-input",
            true,
            &[DeviceCapability::Input, DeviceCapability::Window],
        );
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Input, DeviceCapability::Window]),
                vec!["game.exe".to_owned()],
                Some(5),
                "s-input".to_owned(),
            )
            .expect("grant");

        let error = backend
            .input(
                &InputAction::Text {
                    text: "hi".to_owned(),
                },
                &[],
            )
            .expect_err("Agent 侧不得注入输入");
        assert_eq!(error.code(), "HBOT_DEVICE_APP_NOT_ALLOWED");
        let error = backend.windows().expect_err("Agent 侧不得枚举窗口");
        assert_eq!(error.code(), "HBOT_DEVICE_APP_NOT_ALLOWED");
        assert_eq!(counting.calls(), 0);
    }

    #[test]
    fn an_agent_tool_call_audits_which_bot_granted_it() {
        let fixture = Fixture::new("agent-audit", true, &[DeviceCapability::Ocr]);
        let counting = Arc::new(CountingBackend::default());
        let backend = SessionScopedBackend::new(counting.clone(), &fixture.registry);
        fixture
            .registry
            .grant(
                &fixture.bot_id,
                &caps(&[DeviceCapability::Ocr]),
                Vec::new(),
                Some(5),
                "s-audit".to_owned(),
            )
            .expect("grant");
        backend.ocr(&ocr_request()).expect("应放行");

        let rows = fixture
            .store
            .list_device_audit(&fixture.bot_id, 10)
            .expect("读审计");
        let last = rows.last().expect("应有一条审计");
        assert_eq!(last.actor, "agent_tool");
        assert_eq!(last.action, "ocr");
        assert_eq!(last.outcome, "ok");
    }
}
