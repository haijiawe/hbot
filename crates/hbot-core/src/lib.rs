//! hbot-core：领域模型、端口 trait、错误与 ID。
//!
//! 边界（见 `docs/03-directory.md` §2）：
//!
//! - 只有实体、值类型、端口 trait、错误，以及一个**默认实现**：[`SystemClock`]；
//! - **不做 IO**（[`SystemClock`] 只读 `std::time::SystemTime`，不碰文件/网络/SDK），
//!   **不依赖 workspace 内其他 crate**（只依赖 serde / thiserror / uuid）；
//! - 时间与 UUID 经 [`ports::Clock`] / [`ports::IdGenerator`] 注入，便于测试与重放。
//!
//! 模块划分：
//!
//! - [`ids`]：`ProjectId` / `BotId` / `ConversationId` / `SessionId` / `MessageId` /
//!   `InboundId` / `OutboundId` / `ApprovalId` / `EditId` / `SearchId` newtype；
//! - [`errors`]：`HbotError` 与 RPC 错误码映射（`code()`）；
//! - [`domain`]：实体与枚举，只有数据，没有业务规则；
//! - [`command`]：受控命令执行的契约类型（`CommandSpec` / `CommandOutput` /
//!   `CommandDenied`），实现放在 `hbot-command`，见 `docs/adr/0003-command-isolation.md`；
//! - [`ports`]：同步端口 trait（时钟、ID 生成、事件出口、命令执行）+ [`SystemClock`]。
//!   `SystemClock` 放这里是因为读系统时间不是平台适配；放适配器层会逼着组合根
//!   （如 `hbot-desktop`）为了一个时钟依赖整个 `hbot-adapters`，那是错误的依赖方向。
//! - [`paths`]：应用数据目录的唯一算法（桌面端与后台端必须落到同一个库）。
//! - [`device`]：设备能力（C6）的契约类型与两个端口（[`device::DeviceGate`] 判据、
//!   [`device::DeviceBackend`] 执行）。实现分别在 `hbot-rpc` 与 `hbot-device`，
//!   设计见 `docs/09-device-capability.md`。

#![warn(missing_docs)]

pub mod bot_ai;
pub mod bot_egress;
pub mod bot_jobs;
pub mod bot_lifecycle;
pub mod bot_session;
pub mod bot_state;
pub mod bot_timer;
pub mod bot_webhook;
pub mod command;
pub mod debug;
pub mod device;
pub mod domain;
pub mod errors;
pub mod ids;
pub mod paths;
pub mod ports;
pub mod redact;

pub use bot_ai::{
    AiClient, AiError, AiRequest, AiText, AiUsage, DEFAULT_OUTPUT_TOKENS, MAX_CALLS_PER_EVENT,
    MAX_OUTPUT_TOKENS, MAX_PROMPT_BYTES, MAX_SYSTEM_BYTES, MAX_TEMPERATURE, MAX_TIME_PER_EVENT,
    validate_max_tokens, validate_prompt, validate_system, validate_temperature,
};
pub use bot_egress::{
    MAX_CONVERSATION_ID_BYTES, MAX_NOTIFY_TEXT_BYTES, MAX_SEND_TEXT_BYTES, NotifyError,
    NotifyRequest, NotifyTarget, SendError, SendRequest, validate_conversation, validate_notify,
    validate_notify_target, validate_send,
};
pub use bot_jobs::{
    DEFAULT_JOB_BUDGET_MS, DEFAULT_JOB_RETRIES, JobError, JobRequest, JobSpec, JobState,
    MAX_JOB_BUDGET_MS, MAX_JOB_IDEMPOTENCY_KEY_BYTES, MAX_JOB_NAME_BYTES, MAX_JOB_PAYLOAD_BYTES,
    MAX_JOB_RETRIES, MAX_JOB_TYPES, MAX_JOBS_PER_BOT, MAX_RETRY_DELAY_MS, MIN_JOB_BUDGET_MS,
    MIN_RETRY_DELAY_MS, retry_delay_ms, validate_idempotency_key, validate_job_name,
    validate_payload, validate_request, validate_specs,
};
pub use bot_lifecycle::{LifecyclePhase, STOP_BUDGET, StopReason};
pub use bot_session::{
    DEFAULT_WAIT_TIMEOUT_MS, DueWait, MAX_WAIT_KEY_BYTES, MAX_WAIT_PROMPT_BYTES,
    MAX_WAIT_TIMEOUT_MS, MAX_WAITS_PER_BOT, MAX_WAITS_PER_CONVERSATION, MIN_WAIT_TIMEOUT_MS,
    SessionError, WaitHit, WaitScope, WaitSpec, WaitState, WaitStore,
    validate_key as validate_wait_key, validate_prompt as validate_wait_prompt,
    validate_spec as validate_wait_spec, validate_timeout as validate_wait_timeout,
};
pub use bot_state::{
    BotState, BotStateError, HistoryDirection, HistoryEntry, HistoryReader, KvEntry,
    KvWriteOptions, MAX_HISTORY, MAX_KV_BATCH, MAX_KV_KEY_BYTES, MAX_KV_LIST, MAX_KV_TTL,
    MAX_KV_VALUE_BYTES, history_limit, validate_batch, validate_key, validate_ttl, validate_value,
};
pub use bot_timer::{
    DAY_MS, DueSchedule, MAX_INTERVAL_MS, MAX_SCHEDULE_NAME_BYTES, MAX_SCHEDULES,
    MAX_UTC_OFFSET_MINUTES, MIN_INTERVAL_MS, MIN_UTC_OFFSET_MINUTES, ScheduleSpec, ScheduleWhen,
    TimerError, validate_name, validate_schedules, validate_when,
};
pub use bot_webhook::{
    DEFAULT_WEBHOOK_CONTENT_TYPE, MAX_WEBHOOK_BODY_BYTES, MAX_WEBHOOK_PATH_BYTES,
    MAX_WEBHOOK_RESPONSE_BYTES, MAX_WEBHOOK_ROUTES, MAX_WEBHOOK_SECRET_ENV_BYTES,
    WEBHOOK_SECRET_HEADER, WebhookError, WebhookRequest, WebhookResponse, WebhookRoute,
    response_from_handler, route_for, secrets_match, validate_request as validate_webhook_request,
    validate_response as validate_webhook_response, validate_route_path, validate_routes,
    validate_secret_env,
};
pub use command::{
    CommandDenied, CommandOutput, CommandSpec, CommandStatus, IoDirection, OutputStream, Platform,
    ResourceLimits, ResourceUsage,
};
pub use debug::{Breakpoints, DebugAction, DebugFrame, DebugScope, DebugSink, LoggedAt, PausedAt};
pub use device::{
    CapabilityGate, ClipboardOp, ClipboardValue, DeviceActor, DeviceAuditEntry, DeviceBackend,
    DeviceCapability, DeviceCapabilityReport, DeviceError, DeviceGate, DeviceRateLimits,
    DeviceRateWindow, DeviceSessionState, DeviceTarget, InputAction, MouseButton, OcrRequest,
    OcrText, Screenshot, ScreenshotRequest, WindowInfo,
};
pub use domain::{
    ApprovalDecision, Bot, BotStatus, Conversation, ConversationKind, InboundMessage,
    OutboundMessage, OutboundState, Project, SandboxMode, Timestamp, ToolCall, ToolCallStatus,
};
pub use errors::HbotError;
pub use ids::{
    ApprovalId, BotId, ConversationId, EditId, InboundId, JobId, MessageId, OutboundId, ProjectId,
    SearchId, SessionId, WaitId,
};
pub use paths::{APP_DIR, data_dir, data_dir_for};
pub use ports::{Clock, CommandRunner, EventSink, IdGenerator, SystemClock};
pub use redact::{
    REDACTED, RedactedArgs, RedactedEnv, RedactedJson, is_secret_key, redact_arg, redact_args,
    redact_text, redact_value,
};
