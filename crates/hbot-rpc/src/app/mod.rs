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
//! | `clock` / `ids` | 需要"现在"和关联键的命令 | 进程级，测试可注入固定值 |
//!
//! ## 不持有的东西
//!
//! * 正在跑的 bot supervisor（在 [`crate::app::bots`]，按 `BotId` 索引，可启停）；
//! * Agent 会话（M1，随 `agent_send` 建立）；
//! * UI 连接（在 [`crate::ws`]，它从 `events` 订阅）。
//!
//! ## 阻塞约定
//!
//! `Store` 与 `Workspace` 都是**同步** API（`rusqlite` / `std::fs` 没有 async 版本），
//! 在 `async fn` 里必须走 [`spawn_blocking`]，否则会占住 tokio worker
//! （`crates/hbot-store/src/lib.rs` 的调用约定）。本模块提供
//! [`AppState::blocking`] 把这个包装成一句，避免每个命令各写一遍。

pub mod bots;
pub mod commands;
pub mod events;
pub mod files;
pub mod ids;
pub mod runtime;

#[cfg(test)]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hbot_core::{Clock, HbotError, IdGenerator};
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
    bots: bots::BotRegistry,
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
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir).map_err(|err| {
            HbotError::internal(format!(
                "cannot create data dir `{}`: {err}",
                data_dir.display()
            ))
        })?;
        let store = Store::open(data_dir.join("hbot.db")).map_err(store_failed)?;

        Ok(Self {
            data_dir,
            store: Arc::new(store),
            workspace: RwLock::new(None),
            files: RwLock::new(ActiveFiles::default()),
            events: Arc::new(EventBus::new()),
            bots: bots::BotRegistry::new(),
            clock,
            ids,
        })
    }

    /// 应用数据目录（`hbot.db` 所在）。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 存储层。
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// 事件出口（`EventSink<Event>` 的实现，见 [`EventBus`]）。
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
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
        *self
            .files
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ActiveFiles::default();
        *self
            .workspace
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(workspace));
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
