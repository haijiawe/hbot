//! 应用状态：一个进程里所有命令共享的那一份东西。
//!
//! ## 为什么需要它
//!
//! M0 的 [`crate::Dispatcher`] 只持有可能为空的时钟 / ID / 事件出口，所以除
//! `AppInfo` 外全部返回 `-32006`。接上真实能力后，命令需要同一份 store、同一个
//! 打开的项目、同一个事件出口——把它们塞进 `Dispatcher` 的字段会让每个命令都能
//! 随手 new 一个数据库连接。这里是**唯一**装配点（`docs/02-architecture.md`
//! §2：`hbot-rpc` 是唯一编排层）。
//!
//! ## 持有的东西
//!
//! | 字段 | 谁用 | 生命周期 |
//! |---|---|---|
//! | `store` | 所有持久化命令 | 进程级（单 writer 线程在 `Store` 内部） |
//! | `workspace` | 文件 / 搜索 / 编译 / 诊断 | 随 `project_open` 更换 |
//! | `events` | 所有要推事件的能力 | 进程级（广播，无订阅者时丢弃） |
//! | `searches` | `search_start` / `search_cancel` | 逐次搜索（登记表里只有正在跑的） |
//! | `agent` | `agent_send` / `agent_cancel` / `agent_approve` | 进程级（会话表在它内部） |
//! | `adapter_factory` | `bot_start` | 进程级；测试换成假适配器，生产是 `PlatformRouter`（按平台选 OneBot / QQ 官方） |
//! | `clock` / `ids` | 需要"现在"和关联键的命令 | 进程级，测试可注入固定值 |
//!
//! ## 不持有的东西
//!
//! * 正在跑的 bot supervisor（在 [`crate::app::bots`]，按 `BotId` 索引，可启停）；
//! * Agent 会话表 / 待审编辑 / 审批通道（在 [`crate::app::agent`]，按会话索引）；
//! * UI 连接（在 [`crate::ws`]，它从 `events` 订阅）。
//!
//! ## 阻塞约定
//!
//! `Store` 与 `Workspace` 都是**同步** API（`rusqlite` / `std::fs` 没有 async 版本），
//! 在 `async fn` 里必须走 [`spawn_blocking`]，否则会占住 tokio worker
//! （`crates/hbot-store/src/lib.rs` 的调用约定）。本模块提供
//! [`AppState::blocking`] 把这个包装成一句，避免每个命令各写一遍。

pub mod agent;
pub mod bots;
pub mod commands;
pub mod debug;
pub mod device;
pub mod events;
pub mod files;
pub mod ids;
pub mod runtime;
pub mod search;

#[cfg(test)]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hbot_core::{BotId, Clock, HbotError, IdGenerator};
use hbot_store::Store;
use hbot_workspace::Workspace;

pub use events::EventBus;
pub use files::ActiveFiles;

/// 一个进程的唯一共享状态。
///
/// 构造在组合根（`hbot-cli serve` / `hbot-desktop`），之后只以 `Arc<AppState>` 传递。
pub struct AppState {
    data_dir: PathBuf,
    store: Arc<Store>,
    workspace: RwLock<Option<Arc<Workspace>>>,
    files: RwLock<ActiveFiles>,
    events: Arc<EventBus>,
    searches: search::SearchRegistry,
    bots: bots::BotRegistry,
    agent: Arc<agent::AgentManager>,
    device: Arc<device::DeviceRegistry>,
    adapter_factory: Arc<dyn runtime::AdapterFactory>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

/// 存储层错误 → 领域错误。
///
/// `hbot-store` 不认识 `hbot-core`（依赖方向只允许 store → core 的单向），所以
/// 映射放在使用方。只有"调用方给错了值"这一类保留可纠正语义（`invalid` /
/// `conflict`）；其余（SQLite 失败、迁移失败、writer 线程退出）一律 `internal`
/// ——它们是 bug 或环境故障，不该让 UI 提示用户"改个参数再试"。
pub fn store_failed(error: hbot_store::StoreError) -> HbotError {
    use hbot_store::StoreError as E;

    match error {
        E::Invalid {
            field,
            value,
            allowed,
        } => HbotError::invalid(format!("{field}={value:?} 不接受（允许：{allowed:?}）")),
        E::IdCollision { table, id } => {
            HbotError::conflict(format!("{table} 里 id={id} 已属于另一行"))
        }
        E::SchemaTooNew { found, supported } => HbotError::internal(format!(
            "数据库 schema {found} 比本版本支持的 {supported} 新：请升级 hbot，勿降级运行"
        )),
        other => HbotError::internal(other.to_string()),
    }
}

impl AppState {
    /// 在 `data_dir` 下打开（必要时创建）数据库，装配出一个还没有项目的状态。
    ///
    /// `data_dir` 只放应用数据（`hbot.db`、日志）；项目代码在项目自己的目录里
    /// （`docs/02-architecture.md` §6.1），两者不混。
    pub fn open(
        data_dir: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self, HbotError> {
        let total = hbot_store::startup_trace::Phase::start("appstate_open_total");
        let data_dir = data_dir.as_ref().to_path_buf();
        let phase = hbot_store::startup_trace::Phase::start("app_create_data_dir");
        std::fs::create_dir_all(&data_dir).map_err(|err| {
            HbotError::internal(format!(
                "cannot create data dir `{}`: {err}",
                data_dir.display()
            ))
        })?;
        phase.done();
        let phase = hbot_store::startup_trace::Phase::start("app_store_open");
        let store = Arc::new(Store::open(data_dir.join("hbot.db")).map_err(store_failed)?);
        phase.done();

        // 事件出口与 ID/时钟先建好再共享：`agent` 也要用同一份（否则 UI 收不到
        // agent.delta，且 EditId 会与别处撞号）。
        let phase = hbot_store::startup_trace::Phase::start("app_agent_manager");
        let events = Arc::new(EventBus::new());

        // 设备能力：设置 + live 会话 + 闸门 + 1 秒到期结算（见 [`device::DeviceRegistry`]）。
        //
        // 建在 `agent` **之前**：Agent 侧设备后端要用它做第四层判据
        // （`device::agent_backend::SessionScopedBackend`，撤销/到期立刻停住 `read_screen`）。
        let device = Arc::new(device::DeviceRegistry::new(
            data_dir.clone(),
            Arc::clone(&store),
            Arc::clone(&clock),
            Arc::clone(&events),
        ));
        device.spawn_expiry_tick();

        // Agent 日志不属于任何 bot：`StoreRunLogSink` 写 `bot_id = NULL`、`source = 'agent'`。
        let run_log = Arc::new(agent::StoreRunLogSink::new(
            Arc::clone(&store),
            Arc::clone(&clock),
        ));
        let agent = Arc::new(
            agent::AgentManager::new(
                data_dir.clone(),
                Arc::clone(&events),
                Arc::clone(&ids),
                Arc::clone(&clock),
                run_log,
            )
            .with_device_registry(Arc::clone(&device)),
        );
        phase.done();

        let state = Self {
            data_dir,
            store,
            workspace: RwLock::new(None),
            files: RwLock::new(ActiveFiles::default()),
            events,
            searches: search::SearchRegistry::new(),
            bots: bots::BotRegistry::new(),
            agent,
            device,
            // 生产工厂是路由：按 bots.platform 选 OneBot 或 QQ 官方（见 runtime::PlatformRouter）。
            adapter_factory: Arc::new(runtime::PlatformRouter),
            clock,
            ids,
        };
        total.done();
        Ok(state)
    }

    /// 换掉适配器工厂。
    ///
    /// 生产装配不调用它（缺省就是 [`runtime::PlatformRouter`]，按平台选 OneBot / QQ 官方）；
    /// 测试用它注入假适配器，从而在不连真 socket
    /// 的前提下跑通 store → 队列 → 沙箱 → outbox 的**同一条**链路。
    pub fn with_adapter_factory(mut self, factory: Arc<dyn runtime::AdapterFactory>) -> Self {
        self.adapter_factory = factory;
        self
    }

    /// 换掉 Agent 装配层。
    ///
    /// 生产装配不调用它（缺省读环境变量 / `agent.json`）；测试用它注入固定配置与假
    /// provider，从而在无网络的前提下把 `agent_send` → `run_turn` 的整条链跑通。
    pub fn with_agent(mut self, agent: Arc<agent::AgentManager>) -> Self {
        self.agent = agent;
        self
    }

    /// 适配器工厂（`bot_start` 用它按平台造 `BotAdapter`）。
    pub fn adapter_factory(&self) -> &Arc<dyn runtime::AdapterFactory> {
        &self.adapter_factory
    }

    /// 应用数据目录（`hbot.db` 所在）。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 存储层。
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// 往 `logs` 表追加一行。
    ///
    /// ## 为什么要有这个入口
    ///
    /// 桌面壳（`hbot-desktop`）要在下次启动时把「上次异常退出」写进日志表，
    /// 否则那句话只在文件里躺着、用户永远看不到。它本来可以直接调
    /// `store.append_log(hbot_store::NewLog { … })` —— 但那样它就得依赖
    /// `hbot-store` 来构造 `NewLog`，而依赖规则是**壳只许依赖 `hbot-core` + `hbot-rpc`**
    /// （见 `docs/03-directory.md` 的依赖硬规则与 `xtask deps` 的闸门）。
    ///
    /// 把「构造 `NewLog`」这件小事留在本层，壳就只需要说「给这个 bot 记一条 error」。
    /// 时间是这里取的，调用方不用自己编。
    ///
    /// **这是阻塞调用**（`Store::append_log` 走单写线程并等回执），
    /// 在 async 上下文里必须包 `spawn_blocking`。
    pub fn append_log(
        &self,
        bot_id: BotId,
        level: &str,
        line: impl Into<String>,
    ) -> Result<(), HbotError> {
        let entry = hbot_store::NewLog {
            bot_id,
            level: level.to_string(),
            line: line.into(),
            at: self.clock.now().unix_millis(),
        };
        self.store.append_log(entry).map_err(store_failed)
    }

    /// 事件出口（`EventSink<Event>` 的实现，见 [`EventBus`]）。
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// 正在跑的搜索登记表（`search_cancel` 用它把取消落到搜索线程上）。
    pub fn searches(&self) -> &search::SearchRegistry {
        &self.searches
    }

    /// 时钟。
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// ID 生成器。
    pub fn ids(&self) -> &Arc<dyn IdGenerator> {
        &self.ids
    }

    /// 正在跑的 bot（启停入口）。
    pub fn bots(&self) -> &bots::BotRegistry {
        &self.bots
    }

    /// Agent 装配层（会话表、待审编辑、审批通道、provider 工厂）。
    pub fn agent(&self) -> &Arc<agent::AgentManager> {
        &self.agent
    }

    /// 调试登记表（断点 + 暂停中的等待者，见 [`debug::DebugRegistry`]）。
    ///
    /// 返回 `Arc` 而不是 `&`：它现在是进程级单例（[`debug::registry`] 里有原因说明——
    /// `SandboxHandlerConfig` 那条装配缝由别的单元持有，本单元塞不进字段）。
    /// 等那条缝能改之后，这里改成 `&Arc<debug::DebugRegistry>`（`AppState` 自己持有），
    /// 调用方一行都不用动。
    pub fn debug(&self) -> Arc<debug::DebugRegistry> {
        debug::registry()
    }

    /// 设备能力登记表（宿主设置 + live 会话 + 闸门 + 到期结算）。
    ///
    /// 设备闸门的消费方（`runtime::sandbox_handler`）用 [`device::DeviceRegistry::gate`] 拿
    /// `Arc<dyn DeviceGate>` 注入沙箱 —— 它持 `Weak`，不会延长登记表的寿命。
    pub fn device(&self) -> &Arc<device::DeviceRegistry> {
        &self.device
    }

    /// 已打开的项目根；没打开项目时 `None`。
    pub fn project_root(&self) -> Option<PathBuf> {
        self.workspace
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf())
    }

    /// 打开（或切换）项目根。
    ///
    /// 先停掉旧项目在跑的 bot，再打开新的 workspace：两个项目的 bot 同时对着
    /// 各自的 `hbot.db` 读写会让"哪个项目在跑"变得说不清（`docs/06-ide.md` §6）。
    pub async fn open_project(&self, path: impl AsRef<Path>) -> Result<PathBuf, HbotError> {
        let path = path.as_ref().to_path_buf();
        let workspace = Workspace::open(path)?;
        let root = workspace.root().to_path_buf();

        self.bots.stop_all().await;
        // 上一轮的 Agent 回合还在跑（或在等审批）时不能让新项目接手：它会拿着旧项目
        // 的路径继续写。待审编辑同理——旧项目的 Diff 不该落到新项目里。
        self.agent.cancel_all();
        self.agent.clear_edits();
        // 旧搜索扫的是上一个项目的目录，结果推给 UI 只会造成"打开 B 却看到 A 的命中"。
        self.searches.cancel_all();
        *self
            .files
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ActiveFiles::default();
        *self
            .workspace
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(workspace));
        // 设备闸读的是**当前项目**的 `permissions.device`。不在这里同步，就会出现
        // "刚切到新项目、还没调过任何 device_* 命令"的窗口里，闸还在按上一个项目的清单放行
        // （旧项目声明过、新项目没声明的能力会被多放一次）。
        self.device.note_project_root(Some(root.clone()));
        Ok(root)
    }

    /// 取已打开的项目；没打开时报 `-32002`（不是 `-32001`：是"状态不对"，不是"东西不存在"）。
    pub fn workspace(&self) -> Result<Arc<Workspace>, HbotError> {
        self.workspace
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| HbotError::invalid("没有打开的项目：先发 project_open"))
    }

    /// 当前编辑器打开过的文件（用于变更检测与关闭时的落盘检查）。
    pub fn files(&self) -> std::sync::RwLockReadGuard<'_, ActiveFiles> {
        self.files
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 改当前打开文件集合。
    pub fn files_mut(&self) -> std::sync::RwLockWriteGuard<'_, ActiveFiles> {
        self.files
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 把一个同步阻塞调用挪到阻塞线程池。
    ///
    /// 只用于 `Store` / `Workspace` 这类真阻塞的调用。`JoinError` 说明阻塞任务
    /// panic 了——那是 bug，映射成 `internal` 而不是静默当成"没数据"。
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, HbotError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, HbotError> + Send + 'static,
    {
        match tokio::task::spawn_blocking(f).await {
            Ok(result) => result,
            Err(error) => Err(HbotError::internal(format!(
                "blocking task failed: {error}"
            ))),
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("data_dir", &self.data_dir)
            .field("project", &self.project_root())
            .finish_non_exhaustive()
    }
}
