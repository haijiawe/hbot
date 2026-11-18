//! 作业队列的宿主实现：把 `ctx.jobs.enqueue` 落进 `bot_jobs` 表。
//!
//! `hbot-code` 的 [`hbot_code::jobs::JobQueue`] 只声明"把一次合法入队交出去"这个动作；
//! 真正写库的是这里。为什么不在沙箱里攒队列：进程崩了攒在内存里的作业就没了，而
//! "至少一次"正是靠"入队那一刻就落库"兑现的（见 `hbot_core::bot_jobs` 模块文档）。
//!
//! ## 三道判据的顺序
//!
//! 沙箱侧已经过了参数闸与声明闸（名字必在 `hbot.json` 的 `capabilities.jobs` 里），
//! 所以这里只做落库前还剩的三件事：
//!
//! 1. **容量闸**：这个 bot 的未完成作业到 `MAX_JOBS_PER_BOT` 就报 `HBOT_JOBS_FULL`，
//!    而不是让行无限长下去（有上界才叫背压，没上界就是"跑到内存满才开始丢"）；
//! 2. **预算冻结**：从清单里现读这条作业类型的 `budgetMs`，写进**行**里。之后改清单
//!    不会让已经排队的那条作业换预算 —— 换语义是排障时最费时间的一类问题；
//! 3. **落库**：`Store::enqueue_job` 用 `(bot_id, idempotency_key)` 的唯一约束去重，
//!    命中幂等键返回 `Ok(None)` —— 那是**成功**（重复入队不该报错），只记一条 debug。
//!
//! ## 为什么这个端口是阻塞的
//!
//! 底层 `Store` 是同步 API（`rusqlite`）。`hbot-code` 的 `jobs_api` 已经把端口调用
//! 放进 `tokio::task::spawn_blocking`，所以这里只管同步写完并返回，不自己再 spawn。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbot_code::jobs::JobQueue;
use hbot_core::{
    BotId, Clock, DEFAULT_JOB_BUDGET_MS, IdGenerator, JobError, JobRequest, JobSpec,
    MAX_JOBS_PER_BOT,
};
use hbot_store::{NewJob, Store};

use crate::app::store_failed;

/// 作业队列的 store 实现（一个 bot 一个）。
pub struct StoreJobQueue {
    bot_id: BotId,
    /// 清单所在目录：`budgetMs` 从它现读（与声明闸同一个真源）。
    project_root: PathBuf,
    store: Arc<Store>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl StoreJobQueue {
    /// 装配一个队列端口。
    #[must_use]
    pub fn new(
        bot_id: BotId,
        project_root: PathBuf,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bot_id,
            project_root,
            store,
            ids,
            clock,
        }
    }
}

impl JobQueue for StoreJobQueue {
    fn enqueue(&self, request: &JobRequest) -> Result<(), String> {
        let active = self
            .store
            .count_active_jobs_for_bot(&self.bot_id)
            .map_err(|error| store_failed(error).to_string())?;
        if active >= u32::try_from(MAX_JOBS_PER_BOT).unwrap_or(u32::MAX) {
            return Err(JobError::Full {
                message: format!(
                    "这个 bot 已有 {active} 条未完成作业（上限 {MAX_JOBS_PER_BOT}）：\
                     等它们跑完再入队，或把批量操作拆开"
                ),
            }
            .to_string());
        }

        let budget_ms = declared_jobs(&self.project_root)
            .iter()
            .find(|spec| spec.name == request.name)
            .map_or(DEFAULT_JOB_BUDGET_MS, |spec| spec.budget_ms);
        let payload_json = serde_json::to_string(&request.payload).map_err(|error| {
            JobError::Invalid {
                message: format!("作业载荷无法序列化成 JSON：{error}"),
            }
            .to_string()
        })?;
        let now = self.clock.now().unix_millis();
        let job = NewJob {
            id: self.ids.job_id(),
            name: request.name.clone(),
            payload_json,
            idempotency_key: request.idempotency_key.clone(),
            budget_ms,
            conversation: request.conversation.clone(),
            next_at: now,
            created_at: now,
        };
        match self.store.enqueue_job(&self.bot_id, &job) {
            Ok(Some(_id)) => Ok(()),
            Ok(None) => {
                // 幂等命中：同一个键已经排过队了。这是**成功**，不是失败 ——
                // 用户连点两次、平台重投、重启后补跑都会走到这里。
                tracing::debug!(
                    bot = %self.bot_id,
                    job = %request.name,
                    key = %request.idempotency_key,
                    "作业已存在（幂等键命中），本次入队不新增行"
                );
                Ok(())
            }
            Err(error) => Err(JobError::Failed {
                message: store_failed(error).to_string(),
            }
            .to_string()),
        }
    }
}

impl std::fmt::Debug for StoreJobQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreJobQueue")
            .field("bot_id", &self.bot_id)
            .finish_non_exhaustive()
    }
}

/// 清单里声明的作业类型（`capabilities.jobs`），读不到就是空表。
///
/// 与 `sandbox_handler` 的 `declared_schedules` 同一口径：清单是项目文件，改它除了
/// 指纹没有别的信号，所以每次装配时现读一遍。
#[must_use]
pub fn declared_jobs(root: &Path) -> Vec<JobSpec> {
    let Ok(manifest) = hbot_code::manifest::load(root) else {
        return Vec::new();
    };
    manifest.capabilities.jobs().to_vec()
}
