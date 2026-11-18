//! 后台作业（C4.jobs）在沙箱里的宿主侧装配：`ctx.jobs.enqueue
//!
//! 契约类型、错误码与参数闸都在 `hbot_core::bot_jobs（已冻结），本模块只管三件事：
//!
//! 1. [`JobSlot] —— `Sandbox 上的可替换槽位（与 [`crate::timer::TimerSlot] 同一写法：
//!    出生为空，装配方在 isolate 起来之后按清单写进去）；
//! 2. **声明闸** —— 入队的名字必须真的是 `hbot.json 里 `capabilities.jobs 声明过的
//!    那一组；声明表为空就报 `HBOT_JOBS_NOT_DECLARED，名字不在表里也报错（不静默收下）；
//! 3. **入队端口** —— [`JobQueue]：真正把行写进库的是宿主（`hbot-rpc 调 `Store），
//!    沙箱只负责"把一次合法的入队交出去"。沙箱自己绝不攒队列：进程崩了，攒在内存里的
//!    作业就没了，而"至少一次"正是靠"入队那一刻就落库"兑现的。
//!
//! # 顺序：先解析参数，后判装配
//!
//! 与 [`crate::runtime::state_api] / [`crate::runtime::ai_api] 同一顺序，理由也相同：
//! 坏参数先报错，才不会把"名字写错了"变成"宿主没装配作业队列"，也不会让一次坏入队
//! 白占一次库写。判定顺序是：参数闸 -> 声明闸 -> 幂等键 -> 装配闸 -> 端口。
//!
//! # 缺省幂等键 = 本条消息的 messageId
//!
//! 消息事件里 `ctx.jobs.enqueue(name, payload) 不写 `idempotencyKey 时，用当前这条消息
//! 的 `messageId（与 `hbot_core::bot_jobs 模块文档同一条判据：同一条消息重复处理两次，
//! 也只能产生一条作业）。定时器事件与作业事件本身没有入站身份，它们的 `ctx.jobs 里
//! 这个缺省值被**显式清空**，所以那两种事件里不写 key 就报错而不是继承上一条消息。
//!
//! 作业事件里不挂 `ctx.jobs：作业不能无限套作业。

use std::sync::{Arc, Mutex, PoisonError};

use hbot_core::{
    JobError, JobRequest, JobSpec, MAX_JOB_TYPES, validate_idempotency_key, validate_job_name,
    validate_payload, validate_request,
};

/// 作业没有声明的回复目标时，`ctx.reply 抛出的码。
///
/// 与定时器的 `HBOT_TIMER_NO_TARGET 同一条理由：作业背后没有入站消息，宿主没有任何依据
/// 猜该发给谁；猜错就是往陌生群发消息。
pub(crate) const NO_TARGET_CODE: &str = "HBOT_JOBS_NO_TARGET";

/// 宿主没装配作业队列时的说明（`HBOT_JOBS_UNAVAILABLE 前缀由 [`unavailable] 加）。
pub(crate) const UNASSEMBLED: &str = "宿主还没有可用的作业队列（bot 未启动或存储未就绪）";

/// 把一次合法的入队交出去的端口。
///
/// 实现方是 `hbot-rpc：它按 `bot_jobs 表的唯一约束（bot_id + idempotency_key）落库，
/// 幂等命中就**静默成功**（同一个键的第二条不是错误，这正是"至少一次"的去重机制）。
///
/// 返回 `Err 时，正文已经以 `HBOT_JOBS_* 开头（由 [`JobError] 的 `Display 生成），
/// 沙箱把它原样抛给 handler，不自己再包一层。
pub trait JobQueue: Send + Sync {
    /// 落库一次入队。
    ///
    /// # Errors
    ///
    /// 存储失败（`HBOT_JOBS_FAILED）或队列已满（`HBOT_JOBS_FULL）。
    fn enqueue(&self, request: &JobRequest) -> Result<(), String>;
}

/// 槽位内部状态：一份声明表 + 一个可替换的端口 + 本事件的缺省幂等键。
#[derive(Default)]
struct JobSlotState {
    specs: Vec<JobSpec>,
    queue: Option<Arc<dyn JobQueue>>,
    message: Option<String>,
}

/// 作业槽位（一个 isolate 一个）。
pub(crate) struct JobSlot {
    state: Mutex<JobSlotState>,
}

impl JobSlot {
    /// 空槽位：没有声明、没有端口、没有身份。
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(JobSlotState::default()),
        }
    }

    /// 装入声明表（`hbot.json 的 `capabilities.jobs）。
    ///
    /// 整份替换是刻意的：清单是唯一真源，热重载换清单后下一次入队立刻按新表判。
    pub(crate) fn set_specs(&self, specs: &[JobSpec]) {
        let mut state = self.lock();
        state.specs = specs.to_vec();
    }

    /// 装入（或替换）入队端口。
    pub(crate) fn set_queue(&self, queue: Arc<dyn JobQueue>) {
        let mut state = self.lock();
        state.queue = Some(queue);
    }

    /// 是否已经装配了端口。
    #[must_use]
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().queue.is_some()
    }

    /// 已声明几种作业。
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.lock().specs.len()
    }

    /// 声明表里的一条。
    ///
    /// 只有测试读它：真实入队路径在 [`JobSlot::enqueue`] 内部就查表了，
    /// 宿主也不需要这一层（作业的预算是随行进库的，不随清单变）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn spec(&self, name: &str) -> Option<JobSpec> {
        self.lock()
            .specs
            .iter()
            .find(|spec| spec.name == name)
            .cloned()
    }

    /// 记下本事件的缺省幂等键。
    ///
    /// **每次整体覆盖**，包括覆盖成 `None：定时器与作业事件没有入站身份，
    /// 清空它才不会让一次"没写 key"的入队悄悄用上一条消息的 messageId。
    pub(crate) fn set_current_message(&self, message: Option<String>) {
        self.lock().message = message;
    }

    /// 一次入队：参数闸 -> 声明闸 -> 幂等键 -> 装配闸 -> 端口。
    ///
    /// `key 为 `None 时用本事件的缺省幂等键（见模块文档）。
    pub(crate) fn enqueue(
        &self,
        name: &str,
        payload: serde_json::Value,
        key: Option<String>,
        conversation: Option<String>,
    ) -> Result<(), JobError> {
        validate_job_name(name)?;
        validate_payload(&payload)?;
        if let Some(key) = &key {
            validate_idempotency_key(key)?;
        }

        let (declared, queue, fallback) = {
            let state = self.lock();
            if state.specs.is_empty() {
                return Err(JobError::NotDeclared { capability: "jobs" });
            }
            (
                state.specs.iter().find(|spec| spec.name == name).cloned(),
                state.queue.clone(),
                state.message.clone(),
            )
        };

        let Some(spec) = declared else {
            return Err(JobError::Invalid {
                message: format!(
                    "作业名 {name} 没有在 hbot.json 的 capabilities.jobs 里声明；先声明一条 \
                     {{ \"name\": \"{name}\", \"budgetMs\": 60000, \"retries\": 0 }}"
                ),
            });
        };
        let key = key.or(fallback).ok_or_else(|| JobError::Invalid {
            message: format!(
                "入队作业 {name} 时必须给 options.idempotencyKey：这条事件没有入站 \
                 messageId 可以当缺省幂等键，而没有幂等键就没有去重依据"
            ),
        })?;

        let request = JobRequest {
            name: spec.name.clone(),
            payload,
            idempotency_key: key,
            conversation,
        };
        validate_request(&request)?;

        let Some(queue) = queue else {
            return Err(JobError::Unavailable {
                reason: unavailable(UNASSEMBLED),
            });
        };
        queue.enqueue(&request).map_err(|message| {
            // 端口已经把 `JobError 的 Display 写进了正文；再包一层会让错误码
            // 重复出现两次（`HBOT_JOBS_FAILED: HBOT_JOBS_FAILED: …）。
            JobError::Failed { message }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JobSlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for JobSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobSlot")
            .field("specs", &self.len())
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 没装配时的错误正文（以 `HBOT_JOBS_UNAVAILABLE 开头）。
#[must_use]
pub fn unavailable(detail: &str) -> String {
    JobError::Unavailable {
        reason: detail.to_owned(),
    }
    .to_string()
}

/// 声明表的上限（与 `hbot_core::MAX_JOB_TYPES 同值，导出给宿主读一遍）。
///
/// 这里不放第二份判定：清单加载时的 [`hbot_core::validate_specs] 才是唯一判据；
/// 这个常量只是让"表装得下多少"在沙箱侧也读得到。
pub const MAX_DECLARED_JOBS: usize = MAX_JOB_TYPES;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hbot_core::{JobError, JobSpec, MAX_JOB_PAYLOAD_BYTES, validate_idempotency_key};

    use super::{JobQueue, JobSlot};

    /// 记下端口收到的请求；需要时可以改成一律失败。
    #[derive(Default)]
    struct Recording {
        requests: Mutex<Vec<hbot_core::JobRequest>>,
        failure: Mutex<Option<String>>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn taken(&self) -> Vec<hbot_core::JobRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl JobQueue for Recording {
        fn enqueue(&self, request: &hbot_core::JobRequest) -> Result<(), String> {
            if let Some(message) = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return Err(message);
            }
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            Ok(())
        }
    }

    fn settle_war() -> JobSpec {
        JobSpec::new("settle-war")
    }

    #[test]
    fn an_empty_slot_reports_not_declared() {
        let slot = JobSlot::new();
        let error = slot
            .enqueue("settle-war", serde_json::json!({}), None, None)
            .expect_err("空声明表必须报 NOT_DECLARED");
        assert_eq!(error.code(), "HBOT_JOBS_NOT_DECLARED");
        assert_eq!(slot.len(), 0);
    }

    #[test]
    fn a_name_outside_the_declaration_table_is_refused() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        slot.set_queue(Recording::new());
        let error = slot
            .enqueue("fan-out", serde_json::json!({}), Some("k-1".into()), None)
            .expect_err("未声明的名字必须报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        let text = error.to_string();
        assert!(text.contains("fan-out"), "{text}");
        assert!(text.contains("capabilities.jobs"), "{text}");
    }

    #[test]
    fn a_valid_enqueue_reaches_the_port_verbatim() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        let queue = Recording::new();
        slot.set_queue(queue.clone());

        slot.enqueue(
            "settle-war",
            serde_json::json!({ "warId": "w-1" }),
            Some("w-1".into()),
            Some("group-42".into()),
        )
        .expect("合法入队必须成功");

        let taken = queue.taken();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].name, "settle-war");
        assert_eq!(taken[0].payload, serde_json::json!({ "warId": "w-1" }));
        assert_eq!(taken[0].idempotency_key, "w-1");
        assert_eq!(taken[0].conversation.as_deref(), Some("group-42"));
    }

    #[test]
    fn the_message_id_stands_in_for_a_missing_idempotency_key() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        let queue = Recording::new();
        slot.set_queue(queue.clone());
        slot.set_current_message(Some("msg-7".into()));

        slot.enqueue("settle-war", serde_json::json!({}), None, None)
            .expect("有 messageId 时缺省幂等键成立");
        assert_eq!(queue.taken()[0].idempotency_key, "msg-7");

        // 定时器 / 作业事件没有入站身份：清空之后再缺省就必须失败。
        slot.set_current_message(None);
        let error = slot
            .enqueue("settle-war", serde_json::json!({}), None, None)
            .expect_err("没有幂等键必须报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        assert!(error.to_string().contains("idempotencyKey"), "{error}");
    }

    #[test]
    fn a_declared_job_without_a_port_is_unavailable() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        assert!(!slot.is_assembled());
        let error = slot
            .enqueue(
                "settle-war",
                serde_json::json!({}),
                Some("k-1".into()),
                None,
            )
            .expect_err("没有端口必须报 UNAVAILABLE");
        assert_eq!(error.code(), "HBOT_JOBS_UNAVAILABLE");
    }

    #[test]
    fn a_payload_over_the_cap_never_reaches_the_port() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        let queue = Recording::new();
        slot.set_queue(queue.clone());
        let big = "x".repeat(MAX_JOB_PAYLOAD_BYTES + 1);
        let error = slot
            .enqueue(
                "settle-war",
                serde_json::json!({ "blob": big }),
                Some("k-1".into()),
                None,
            )
            .expect_err("超限载荷必须报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        assert!(queue.taken().is_empty(), "坏参数不许占一次库写");
    }

    #[test]
    fn the_port_failure_is_reported_without_doubling_the_code() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        let queue = Recording::new();
        *queue
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            JobError::Full {
                message: "这个 bot 已有 256 条未完成作业".into(),
            }
            .to_string(),
        );
        slot.set_queue(queue);

        let error = slot
            .enqueue(
                "settle-war",
                serde_json::json!({}),
                Some("k-1".into()),
                None,
            )
            .expect_err("端口失败必须冒出来");
        assert_eq!(error.code(), "HBOT_JOBS_FAILED");
        let text = error.to_string();
        assert_eq!(text.matches("HBOT_JOBS_FULL").count(), 1, "{text}");
    }

    #[test]
    fn a_bad_idempotency_key_is_refused_before_the_declaration_gate() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war()]);
        slot.set_queue(Recording::new());
        let error = slot
            .enqueue(
                "settle-war",
                serde_json::json!({}),
                Some(" pad ".into()),
                None,
            )
            .expect_err("带首尾空白的幂等键必须报错");
        assert_eq!(error.code(), "HBOT_JOBS_INVALID");
        assert!(validate_idempotency_key(" pad ").is_err());
    }

    #[test]
    fn the_declaration_table_is_replaced_as_a_whole() {
        let slot = JobSlot::new();
        slot.set_specs(&[settle_war(), JobSpec::new("fan-out")]);
        assert_eq!(slot.len(), 2);
        assert!(slot.spec("fan-out").is_some());

        slot.set_specs(&[JobSpec::new("fan-out")]);
        assert_eq!(slot.len(), 1);
        assert!(slot.spec("settle-war").is_none());
    }
}
