//! [`HandlerExecutor`](hbot_bot::HandlerExecutor) 的 QuickJS 实现。
//!
//! 一条入站消息进来时：
//!
//! 1. 取（或编译）项目里的生成代码 → [`hbot_code::Sandbox`]，每 bot 固定几个 isolate；
//! 2. 把 [`MessageCtx`] 变成一次 `dispatch` 的事件 JSON；
//! 3. 把 `reply(...)` 收集成出站文本（由 outbox 负责真正发送）。
//!
//! ## 线程约定
//!
//! [`hbot_code::Sandbox::dispatch`](hbot_code::Sandbox::dispatch) 是**阻塞**调用（它
//! 在 isolate 线程上跑并等结果），所以必须走 `spawn_blocking`，否则会占住 tokio worker
//! ——这正是旧项目里"Agent 流式输出停顿、心跳超时"的成因
//! （`docs/00-assessment.md` P0-4 的同源问题）。
//!
//! ## 当前状态（M0，**未接线**）
//!
//! 返回明确错误而不是空回复：handler 返回 `Err` 会让 supervisor 计入连续失败并进入
//! `Degraded`，UI 上能看出"这个 bot 没跑起来"；返回空回复只会让人以为 bot 在装死。

use async_trait::async_trait;
use hbot_bot::{HandlerExecutor, MessageCtx};
use hbot_core::HbotError;

/// 用 QuickJS 沙箱跑生成代码的执行器。
///
/// 现在还没有字段：接上项目路径与 isolate 池之后，这里会持有
/// `CompiledProject` 缓存与每 bot 的 `Sandbox` 池（`docs/02-architecture.md` §6.3）。
#[derive(Debug, Default)]
pub struct SandboxHandler;

impl SandboxHandler {
    /// 建一个执行器。
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl HandlerExecutor for SandboxHandler {
    async fn on_message(&self, ctx: MessageCtx) -> Result<(), HbotError> {
        let _ = ctx;
        Err(HbotError::internal(
            "sandbox handler 尚未接线：需要 isolate 池与生成代码编译（docs/02-architecture.md §6.3）",
        ))
    }
}
