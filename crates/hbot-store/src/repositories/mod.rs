//! Repository：每条 SQL 只出现在这里，`db.rs` 只负责调度。
//!
//! 所有函数都是同步的，接收 `&Connection`——既被单 writer 线程调用（写），也被只读连接
//! 调用（读）。不做业务判断：去重、重试策略等由 `hbot-bot` 决定。

pub(crate) mod bot;
pub(crate) mod conversation;
pub(crate) mod device;
pub(crate) mod history;
pub(crate) mod inbound;
pub(crate) mod jobs;
pub(crate) mod kv;
pub(crate) mod log;
pub(crate) mod outbound;
pub(crate) mod project;
