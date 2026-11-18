//! 连接、PRAGMA、单 db 线程与只读连接。
//!
//! ## 为什么是“一个写线程 + 只读连接”
//!
//! `rusqlite` 是同步 API，SQLite 的写事务本身也只能串行。`docs/02-architecture.md` §7、§10
//! 要求：写走**专用 `std::thread` + 有界 mpsc**，读用独立连接。这样做的收益：
//!
//! * 写永不占用 tokio worker（`AGENTS.md` §5：阻塞操作不放 reactor）；
//! * 写队列有界（[`WRITE_QUEUE_CAPACITY`]），满时对上游施加背压而不是无限增长；
//! * 读路径不排队等写，`busy_timeout` 只影响真正争锁的那一条语句。
//!
//! ## 两条读路径（有意不同，改动前先读这段）
//!
//! 1. [`Store::get_bot`] / [`Store::list_bots`] 走**只读连接池**（WAL 下读不阻塞写），
//!    适合 UI/状态面板这种“随时可读、不必看到刚入队写”的查询；
//! 2. 续发/排空类读（[`Store::list_pending_outbound`]、[`Store::list_unprocessed_inbound`]、
//!    [`Store::count_pending_outbound`] 等）走**db 线程**，与写排队同序——调用方读到的一定
//!    包含自己在这之前入队的写，避免“刚 enqueue 完立刻 list 却是空的”这种续发漏写。
//!
//! 代价：第 2 类读会排在已入队的写后面。单 bot 每秒几十条消息时这是亚毫秒级；若将来出现
//! 长事务导致读饥饿，再把这几个查询迁到只读连接池（必须同时接受“可能看不到刚入队写”）。
//!
//! ## 调用约定（重要）
//!
//! 本 crate 的公开方法**全是同步阻塞**的。异步调用方必须把它们放进
//! `tokio::task::spawn_blocking`，不要在 async 任务里直接调用——否则会把 tokio worker
//! 卡住，表现为 Agent 流式输出停顿（`docs/02-architecture.md` §7 复核补充）。

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Result, StoreError};
use crate::ids::{BotId, ConversationId, InboundId, JobId, OutboundId, WaitId};
use crate::migrations;
use crate::models::{
    BOT_STATUSES, Bot, BotStatusRow, Conversation, DeviceAuditRow, DeviceSessionRow, HistoryRow,
    InboundReplayRow, InboundRow, JobRow, LogRow, NewAgentLog, NewDeviceAudit, NewDeviceSession,
    NewInbound, NewJob, NewKvEntry, NewLog, NewOutbound, NewWait, OutboundRow, OutboundTarget,
    Project, WaitRow, is_valid_bot_status,
};
use crate::repositories;
use crate::startup_trace;

/// 写/读连接统一的忙等窗口（毫秒）：遇到锁时最多重试 5s，再报 `SQLITE_BUSY`。
pub const BUSY_TIMEOUT_MS: u32 = 5_000;

/// 只读连接数量（`docs/02-architecture.md` §7 建议 2–4）。
///
/// `pub(crate)`：启动路径的回归闸门要按它断言"只开 1 条写连接 + 这么多条只读连接"
/// （`crate::tests::startup_opens_only_the_writer_and_read_connections`）。
pub(crate) const READ_CONNECTIONS: usize = 2;

/// 写队列容量。满载时 `send` 阻塞，形成背压（`AGENTS.md` §5：通道一律有界）。
const WRITE_QUEUE_CAPACITY: usize = 1_024;

/// WAL 模式的规范化名字，用于校验 `PRAGMA journal_mode` 的返回值。
const JOURNAL_MODE_WAL: &str = "wal";

/// 单次请求的回复通道（容量 1：一对一应答）。
type Reply<T> = SyncSender<Result<T>>;

/// 交给 db 线程执行的操作（写 + 需要与写同序的读）。
///
/// 每个变体携带自己的回复类型，避免用“万能 Result”丢失类型信息。
enum StoreOp {
    Shutdown,
    InsertProject {
        project: Project,
        reply: Reply<()>,
    },
    /// 按 `projects.path` 查项目：`bot_start` 需要判断"这个项目根是否已登记过"。
    ///
    /// 为什么不能"先 list 再在内存里筛"：项目行数没有上限，而 `bot_start` 每次都会调它。
    GetProjectByPath {
        path: String,
        reply: Reply<Option<Project>>,
    },
    InsertBot {
        bot: Bot,
        reply: Reply<()>,
    },
    InsertConversation {
        conversation: Conversation,
        reply: Reply<()>,
    },
    InsertInbound {
        message: NewInbound,
        sender_id: String,
        reply: Reply<Option<InboundId>>,
    },
    MarkInboundProcessed {
        id: InboundId,
        processed_at: i64,
        reply: Reply<bool>,
    },
    EnqueueOutbound {
        message: NewOutbound,
        target: OutboundTarget,
        reply: Reply<OutboundId>,
    },
    MarkOutboundSent {
        id: OutboundId,
        sent_at: i64,
        reply: Reply<bool>,
    },
    MarkOutboundAttempt {
        id: OutboundId,
        reply: Reply<Option<u32>>,
    },
    MarkOutboundFailed {
        id: OutboundId,
        attempts: u32,
        reply: Reply<bool>,
    },
    AppendLog {
        entry: NewLog,
        reply: Reply<()>,
    },
    AppendAgentLog {
        entry: NewAgentLog,
        reply: Reply<()>,
    },
    UpdateBotStatus {
        id: BotId,
        status: String,
        updated_at: i64,
        reply: Reply<bool>,
    },
    ListPendingOutbound {
        limit: u32,
        reply: Reply<Vec<OutboundRow>>,
    },
    ListPendingOutboundForBot {
        bot_id: BotId,
        limit: u32,
        reply: Reply<Vec<OutboundRow>>,
    },
    CountPendingOutbound {
        reply: Reply<u32>,
    },
    GetOutbound {
        id: OutboundId,
        reply: Reply<Option<OutboundRow>>,
    },
    ListUnprocessedInbound {
        limit: u32,
        reply: Reply<Vec<InboundRow>>,
    },
    /// 启动补偿：按 `(received_at, rowid)` 分批取某个 bot 的未处理入站消息。
    ///
    /// `after` 是上一批最后一行的游标（`None` = 从头开始）。必须走 db 线程（与写同序），
    /// 否则刚插入的 inbound 行可能看不到，补偿会漏行。
    ListInboundReplay {
        bot_id: BotId,
        after: Option<(i64, i64)>,
        limit: u32,
        reply: Reply<Vec<InboundReplayRow>>,
    },
    GetConversation {
        id: ConversationId,
        reply: Reply<Option<Conversation>>,
    },
    GetConversationByExternal {
        bot_id: BotId,
        external_id: String,
        reply: Reply<Option<Conversation>>,
    },
    /// 按 bot 列出全部会话（`bot_conversations` 的数据源）。
    ///
    /// 与同组的两条会话读一样走 db 线程（不是只读连接池）：它排在已入队的写后面，
    /// 所以 `bot_start` 刚刚幂等建好的 `local:default` 行不会被这条读漏掉。
    ListConversationsForBot {
        bot_id: BotId,
        reply: Reply<Vec<Conversation>>,
    },
    GetBotStatus {
        id: BotId,
        reply: Reply<Option<BotStatusRow>>,
    },
    ListLogs {
        bot_id: Option<BotId>,
        limit: u32,
        reply: Reply<Vec<LogRow>>,
    },
    /// 插入一条设备会话，返回落库后的行（读回，不回显入参）。
    InsertDeviceSession {
        session: NewDeviceSession,
        reply: Reply<DeviceSessionRow>,
    },
    /// 取某个 bot 最近一次设备会话。**必须与写同序**（走 db 线程而不是只读池）：
    /// 撤销返回后紧接着的这条读不能还看到 `active`（`docs/09` §4.3 要求撤销立即生效）。
    LatestDeviceSession {
        bot_id: BotId,
        reply: Reply<Option<DeviceSessionRow>>,
    },
    UpdateDeviceSessionState {
        id: String,
        state: String,
        ended_at: Option<i64>,
        reply: Reply<bool>,
    },
    /// 撤销 / 到期的统一落点：把该 bot 所有 `active` 行一次 UPDATE 置为新状态。
    EndLiveDeviceSessions {
        bot_id: BotId,
        state: String,
        ended_at: i64,
        reply: Reply<usize>,
    },
    AppendDeviceAudit {
        entry: NewDeviceAudit,
        reply: Reply<()>,
    },
    ListDeviceAudit {
        bot_id: BotId,
        limit: usize,
        reply: Reply<Vec<DeviceAuditRow>>,
    },
    /// 读一个 KV 键；过期行不算存在。**必须与写同序**（走 db 线程而不是只读池）：
    /// `set_kv` 返回后紧接着的这条读不能还看到旧值（与 [`StoreOp::LatestDeviceSession`]
    /// 同一条理由）。
    GetKv {
        bot_id: BotId,
        key: String,
        now: i64,
        reply: Reply<Option<String>>,
    },
    /// 写一个 KV 键（存在则覆盖，`expires_at` 可把旧 TTL 清成 NULL）。
    SetKv {
        bot_id: BotId,
        key: String,
        value_json: String,
        expires_at: Option<i64>,
        now: i64,
        reply: Reply<()>,
    },
    /// 删一个 KV 键；返回是否真的删到行。
    DeleteKv {
        bot_id: BotId,
        key: String,
        reply: Reply<bool>,
    },
    /// 列出未过期的 KV 键名（按 key 升序，可选前缀）。与写同序的理由同 [`StoreOp::GetKv`]。
    ListKv {
        bot_id: BotId,
        prefix: Option<String>,
        limit: u32,
        now: i64,
        reply: Reply<Vec<String>>,
    },
    /// 一次事务里批量 upsert（全成或全不成）。
    SetKvMany {
        bot_id: BotId,
        entries: Vec<NewKvEntry>,
        now: i64,
        reply: Reply<()>,
    },
    /// 合成 `inbound` / `outbound` 的会话历史，最近 `limit` 条、升序。
    ///
    /// **生产读路径不走这个变体**：历史只追加、不回读刚写的行，所以
    /// [`Store::list_history`] 走只读连接池（见那边的注释）。这里保留 db 线程的孪生分支是因为
    /// 两条路径必须给出同一答案，而 SQL 只能出现在 `repositories/`（本 crate 的硬规则）——
    /// 所以孪生分支调的是同一个 `repositories::history::history`，
    /// `crate::tests::history_reads_agree_on_both_paths` 对拍两条路径的结果。
    /// 非测试构建里它不被构造，因此显式 `allow(dead_code)`：留着它是为了"读写同序"这条路
    /// 随时可用，而不是忘了删。
    #[allow(dead_code)]
    ListHistory {
        bot_id: BotId,
        conversation_id: ConversationId,
        limit: u32,
        reply: Reply<Vec<HistoryRow>>,
    },
    /// 入队一条后台作业。命中 `(bot_id, idempotency_key)` 时返回 `None` —— 这不是错误。
    EnqueueJob {
        bot_id: BotId,
        job: NewJob,
        reply: Reply<Option<JobId>>,
    },
    /// 取到期作业、就地改成 `running` 并把 `attempts` 加一，返回改完的行（见
    /// `repositories::jobs` 的模块文档：这是唯一关得掉"先查后改"窗口的形状）。
    ClaimDueJobs {
        now: i64,
        limit: u32,
        reply: Reply<Vec<JobRow>>,
    },
    /// 取**某个 bot** 的到期作业（`update_jobs` / `bot_stop` 之外的第三种用法：per-bot tick）。
    ///
    /// 与 `ClaimDueJobs` 同一份 SQL 判据，只多一个 `bot_id` 条件 —— 没有它，per-bot 的
    /// tick 会互相抢作业，然后在错的 bot 的 isolate 里跑。
    ClaimDueJobsForBot {
        bot_id: BotId,
        now: i64,
        limit: u32,
        reply: Reply<Vec<JobRow>>,
    },
    /// 按主键读一条作业（排障 / 面板）。
    GetJob {
        id: JobId,
        reply: Reply<Option<JobRow>>,
    },
    /// 标成功：落 `done` 终态并清掉 `last_error`。
    MarkJobDone {
        id: JobId,
        now: i64,
        reply: Reply<bool>,
    },
    /// 排下一次重试：回 `pending`、改 `next_at`、记 `last_error`（`attempts` 不动）。
    RescheduleJob {
        id: JobId,
        next_at: i64,
        error: String,
        now: i64,
        reply: Reply<bool>,
    },
    /// 落 `failed` 终态并写错误正文（行保留，不作删除）。
    MarkJobFailed {
        id: JobId,
        error: String,
        now: i64,
        reply: Reply<bool>,
    },
    /// `bot_stop`：把这个 bot 的未完成作业全改成 `cancelled`，返回取消了几条。
    CancelJobsForBot {
        bot_id: BotId,
        now: i64,
        reply: Reply<u32>,
    },
    /// 进程启动时把残留的 `running` 退回 `pending`，返回恢复了几条。
    ResumeInterruptedJobs {
        now: i64,
        reply: Reply<u32>,
    },
    /// 注册一条会话等待；撞「同会话已有 pending」时返回 `StoreError::WaitPending`。
    RegisterWait {
        bot_id: BotId,
        wait: NewWait,
        reply: Reply<WaitId>,
    },
    /// 一条消息唤醒唯一那个等待（挑 + 改 `fulfilled` + 返回，一条 SQL）。
    ClaimWait {
        bot_id: BotId,
        conversation_id: ConversationId,
        sender_id: String,
        message_id: String,
        now: i64,
        reply: Reply<Option<WaitRow>>,
    },
    /// 超时结算：把到期的 `pending` 改成 `expired` 并返回。
    ExpireDueWaits {
        bot_id: BotId,
        now: i64,
        limit: u32,
        reply: Reply<Vec<WaitRow>>,
    },
    /// 读这个会话当前未结的那条等待。
    CurrentWait {
        bot_id: BotId,
        conversation_id: ConversationId,
        reply: Reply<Option<WaitRow>>,
    },
    /// 取消这个会话未结的那条等待（返回是否命中）。
    CancelWait {
        bot_id: BotId,
        conversation_id: ConversationId,
        now: i64,
        reply: Reply<bool>,
    },
    /// 数一个 bot 的未结等待（容量闸的判据）。
    CountPendingWaitsForBot {
        bot_id: BotId,
        reply: Reply<u32>,
    },
    /// `bot_stop`：取消这个 bot 的全部未结等待，返回取消了几条。
    CancelWaitsForBot {
        bot_id: BotId,
        now: i64,
        reply: Reply<u32>,
    },
    /// 数一个 bot 的未完成作业（作业池上限的判据）。
    CountActiveJobsForBot {
        bot_id: BotId,
        reply: Reply<u32>,
    },
    /// 列一个 bot 的作业行（按入队时刻升序）。
    ListJobsForBot {
        bot_id: BotId,
        limit: u32,
        reply: Reply<Vec<JobRow>>,
    },
}

/// SQLite 存储句柄：一个 db 线程 + 一个小只读连接池。
///
/// 克隆无意义（`Store` 内含通道与线程句柄），共享请用 `Arc<Store>`。
pub struct Store {
    // 字段顺序即析构顺序：先停 db 线程并 join，再关只读连接。
    ops: Mutex<Option<SyncSender<StoreOp>>>,
    ops_thread: Mutex<Option<JoinHandle<()>>>,
    reads: ReadPool,
}

impl Store {
    /// 打开（必要时创建）数据库，执行迁移并启动 db 线程。
    ///
    /// 硬前提：`journal_mode=wal`。文件系统不支持 WAL（例如某些网络盘）时返回
    /// [`StoreError::JournalMode`]，**不**静默退回 delete 模式。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let open_total = startup_trace::Phase::start("store_open_total");

        let mut writer = open_connection(path, "role=writer")?;

        let phase = startup_trace::Phase::start("store_migrations");
        let schema_version = migrations::apply(&mut writer, now_unix_ms())?;
        phase.done();

        let mut read_connections = Vec::with_capacity(READ_CONNECTIONS);
        for index in 0..READ_CONNECTIONS {
            let role = if index == 0 {
                "role=read0"
            } else {
                "role=read1"
            };
            read_connections.push(Mutex::new(open_connection(path, role)?));
        }

        let phase = startup_trace::Phase::start("store_writer_thread");
        let (sender, receiver) = mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let ops_thread = std::thread::Builder::new()
            .name("hbot-store-writer".to_owned())
            .spawn(move || store_loop(writer, receiver))?;
        phase.done();

        tracing::info!(
            path = %path.display(),
            schema_version,
            queue_capacity = WRITE_QUEUE_CAPACITY,
            read_connections = READ_CONNECTIONS,
            "hbot-store opened"
        );

        open_total.done();

        Ok(Self {
            ops: Mutex::new(Some(sender)),
            ops_thread: Mutex::new(Some(ops_thread)),
            reads: ReadPool::new(read_connections),
        })
    }

    /// 插入项目（`projects`）。
    pub fn insert_project(&self, project: &Project) -> Result<()> {
        let project = project.clone();
        self.submit(|reply| StoreOp::InsertProject { project, reply })
    }

    /// 按 `projects.path` 查项目；不存在返回 `Ok(None)`。
    pub fn get_project_by_path(&self, path: &str) -> Result<Option<Project>> {
        let path = path.to_owned();
        self.submit(|reply| StoreOp::GetProjectByPath { path, reply })
    }

    /// 插入 bot（`bots`）。
    pub fn insert_bot(&self, bot: &Bot) -> Result<()> {
        let bot = bot.clone();
        self.submit(|reply| StoreOp::InsertBot { bot, reply })
    }

    /// 插入会话（`conversations`）。
    pub fn insert_conversation(&self, conversation: &Conversation) -> Result<()> {
        let conversation = conversation.clone();
        self.submit(|reply| StoreOp::InsertConversation {
            conversation,
            reply,
        })
    }

    /// 幂等插入入站消息：`(bot_id, external_id)` 已存在时返回 `None`。
    ///
    /// 返回 `None` 表示“这条外部消息已经落库过”，调用方应直接丢弃，不再入队。
    ///
    /// **落地顺序硬约束**：`bots` 行与 `conversations` 行必须先存在，否则本方法返回
    /// `FOREIGN KEY constraint failed`（`foreign_keys=ON` 且不外键降级）。适配器顺序：
    /// 写 bot →（首次见到会话时）写 conversation → 写 inbound → 入队处理。
    ///
    /// 本方法落库的 `sender_id` 是空串（未知发送者）。知道发送者时请用
    /// [`Store::insert_inbound_with_sender`]，否则 UI 侧会显示“未知发送者”。
    pub fn insert_inbound_if_new(&self, message: NewInbound) -> Result<Option<InboundId>> {
        self.insert_inbound_with_sender(message, String::new())
    }

    /// 同 [`Store::insert_inbound_if_new`]，但把平台发送者 ID 一起落库。
    ///
    /// `sender_id` 是平台侧的原样字符串（OneBot 的 `user_id`、Telegram 的 `from.id`…），
    /// 不是 UUID；平台未提供时传空串，不要伪造占位值。
    pub fn insert_inbound_with_sender(
        &self,
        message: NewInbound,
        sender_id: String,
    ) -> Result<Option<InboundId>> {
        self.submit(|reply| StoreOp::InsertInbound {
            message,
            sender_id,
            reply,
        })
    }

    /// 标记入站消息已处理；返回是否命中一行。
    pub fn mark_inbound_processed(&self, id: &InboundId, processed_at: i64) -> Result<bool> {
        let id = *id;
        self.submit(|reply| StoreOp::MarkInboundProcessed {
            id,
            processed_at,
            reply,
        })
    }

    /// 入队一条出站消息（初始 `pending`），不携带发送目标/回复引用。
    ///
    /// 目标未知时落库为 `target_json = '{}'`、`reply_to = NULL`。被动回复必须用
    /// [`Store::enqueue_outbound_with_target`] 带上 `reply_to`（`AGENTS.md` §4.1：被动回复
    /// 必须绑定 `reply_to`，主动消息不得冒用旧事件）。
    pub fn enqueue_outbound(&self, message: NewOutbound) -> Result<OutboundId> {
        self.enqueue_outbound_with_target(message, OutboundTarget::unknown())
    }

    /// 入队一条出站消息，并落库发送目标与被动回复引用。
    pub fn enqueue_outbound_with_target(
        &self,
        message: NewOutbound,
        target: OutboundTarget,
    ) -> Result<OutboundId> {
        let id = message.id;
        self.submit(|reply| StoreOp::EnqueueOutbound {
            message,
            target,
            reply,
        })?;
        Ok(id)
    }

    /// 标记出站消息已发送；返回是否命中一行。
    pub fn mark_outbound_sent(&self, id: &OutboundId, sent_at: i64) -> Result<bool> {
        let id = *id;
        self.submit(|reply| StoreOp::MarkOutboundSent { id, sent_at, reply })
    }

    /// 记一次发送尝试，返回累加后的 attempts；行不存在返回 `None`。
    pub fn mark_outbound_attempt(&self, id: &OutboundId) -> Result<Option<u32>> {
        let id = *id;
        self.submit(|reply| StoreOp::MarkOutboundAttempt { id, reply })
    }

    /// 把出站消息转为死信（`state='failed'`），超过重试上限时调用。
    ///
    /// `attempts` 是最终尝试次数，一起落库，便于 UI 显示“重试 3 次后放弃”。
    /// 返回 `false` = ID 不存在。行保留不删：排障需要原始 target/text。
    pub fn mark_outbound_failed(&self, id: &OutboundId, attempts: u32) -> Result<bool> {
        let id = *id;
        self.submit(|reply| StoreOp::MarkOutboundFailed {
            id,
            attempts,
            reply,
        })
    }

    /// 追加一条运行日志（`logs`）。
    pub fn append_log(&self, entry: NewLog) -> Result<()> {
        self.submit(|reply| StoreOp::AppendLog { entry, reply })
    }

    /// 追加一条 Agent 日志（`bot_id = NULL`、`source = 'agent'`）。
    pub fn append_agent_log(&self, entry: NewAgentLog) -> Result<()> {
        self.submit(|reply| StoreOp::AppendAgentLog { entry, reply })
    }

    /// 读一个 bot；不存在返回 `Ok(None)`。走只读连接池。
    pub fn get_bot(&self, id: &BotId) -> Result<Option<Bot>> {
        self.read(|conn| repositories::bot::get(conn, id))
    }

    /// 列出全部 bot。走只读连接池。
    pub fn list_bots(&self) -> Result<Vec<Bot>> {
        self.read(repositories::bot::list)
    }

    /// 更新 bot 运行状态，返回是否命中一行。
    ///
    /// `status` 必须是 [`BOT_STATUSES`] 之一（`stopped`/`running`/`degraded`/`error`）；
    /// 其它值返回 [`StoreError::Invalid`]，**不**静默落库（否则 UI 会拿到一个没有语义的
    /// 状态字符串）。
    pub fn update_bot_status(&self, id: &BotId, status: &str, updated_at: i64) -> Result<bool> {
        if !is_valid_bot_status(status) {
            return Err(StoreError::Invalid {
                field: "bots.status",
                value: status.to_owned(),
                allowed: BOT_STATUSES,
            });
        }
        let id = *id;
        let status = status.to_owned();
        self.submit(|reply| StoreOp::UpdateBotStatus {
            id,
            status,
            updated_at,
            reply,
        })
    }

    /// 读 bot 的运行状态（`status` + `updated_at`）；不存在返回 `Ok(None)`。
    pub fn get_bot_status(&self, id: &BotId) -> Result<Option<BotStatusRow>> {
        let id = *id;
        self.submit(|reply| StoreOp::GetBotStatus { id, reply })
    }

    /// 当前 schema 版本（`PRAGMA user_version`）。
    pub fn schema_version(&self) -> Result<u32> {
        self.read(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            Ok(u32::try_from(version).unwrap_or(u32::MAX))
        })
    }

    /// 当前 journal_mode；正常应为 `"wal"`。
    pub fn journal_mode(&self) -> Result<String> {
        self.read(|conn| Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?))
    }

    /// 只读连接上的 `busy_timeout`（毫秒），用于自检与测试。
    pub fn busy_timeout_ms(&self) -> Result<u32> {
        self.read(|conn| {
            let timeout: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            Ok(u32::try_from(timeout).unwrap_or(0))
        })
    }

    /// 列出待发送的出站消息（`state='pending'`），按**产生顺序**取前 `limit` 条。
    ///
    /// 排序是 `(created_at, rowid)`：先产生时间，再插入顺序。进程重启后用它续发
    /// （`docs/02-architecture.md` §4.1 `outbound row → sender retry`），同一会话的回复
    /// 必须按产生顺序发出，所以不能按 `id`（UUID 字典序无时间含义）。
    /// 与写同序执行：本方法返回时，调用方此前 `enqueue_*` 的行一定已在结果里。
    pub fn list_pending_outbound(&self, limit: u32) -> Result<Vec<OutboundRow>> {
        self.submit(|reply| StoreOp::ListPendingOutbound { limit, reply })
    }

    /// 列出某个 bot 的待发送出站消息（`state='pending'`），按 `(created_at, rowid)` 取前
    /// `limit` 条。
    ///
    /// 与 [`Store::list_pending_outbound`] 只差 `bot_id` 过滤：`bot_start` 恢复未发消息时
    /// 只知道自己的 `BotId`，不能先取全局窗口再过滤（别的 bot 的积压会把本 bot 的行挤出去，
    /// 等于静默丢消息）。
    pub fn list_pending_outbound_for_bot(
        &self,
        bot_id: &BotId,
        limit: u32,
    ) -> Result<Vec<OutboundRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ListPendingOutboundForBot {
            bot_id,
            limit,
            reply,
        })
    }

    /// 待发送条数（指标与背压用）。条件与 [`Store::list_pending_outbound`] 完全相同。
    pub fn count_pending_outbound(&self) -> Result<u32> {
        self.submit(|reply| StoreOp::CountPendingOutbound { reply })
    }

    /// 按主键读一条出站消息；不存在返回 `Ok(None)`。
    pub fn get_outbound(&self, id: &OutboundId) -> Result<Option<OutboundRow>> {
        let id = *id;
        self.submit(|reply| StoreOp::GetOutbound { id, reply })
    }

    /// 列出未处理的入站消息（`processed_at IS NULL`），按 `(received_at, id)` 升序取前
    /// `limit` 条。
    ///
    /// 用于“先落库再处理”的补偿：进程在处理前崩溃时，行仍是未处理态，重启后由这里捞回
    /// 重新入队（不丢消息，`docs/02-architecture.md` §4.1）。
    pub fn list_unprocessed_inbound(&self, limit: u32) -> Result<Vec<InboundRow>> {
        self.submit(|reply| StoreOp::ListUnprocessedInbound { limit, reply })
    }

    /// 启动补偿：分批取某个 bot 的未处理入站消息，按 `(received_at, rowid)` 升序。
    ///
    /// `after` 是上一批最后一行的 `(received_at, rowid)`；`None` 表示从最早一条开始。
    /// **这是 `bot_start` 唯一合法的补偿入口**：`list_unprocessed_inbound` 没有 bot 过滤，
    /// 拿它补偿会把别的 bot 的消息塞进本 bot 的队列（`conversation_id` 对不上，等于串号）。
    ///
    /// 语义是 at-least-once（`docs/02-architecture.md` §5.1）：调用方处理成功后必须
    /// `mark_inbound_processed`，否则下次启动还会再补偿一次。
    pub fn list_inbound_replay_for_bot(
        &self,
        bot_id: &BotId,
        after: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<InboundReplayRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ListInboundReplay {
            bot_id,
            after,
            limit,
            reply,
        })
    }

    /// 回放（FR-27）：取某个 bot 的**全部历史入站**，按 `(received_at, rowid)` 升序。
    ///
    /// 与 [`Store::list_inbound_replay_for_bot`] 只差一处：那个方法带 `processed_at IS NULL`，
    /// 是**启动补偿**用的；回放要的是"当时真的被处理过的那批"，已处理的行才是历史证据，
    /// 所以这里刻意不过滤 `processed_at`。
    ///
    /// `from_received_at` 是 `--from` 的下界（毫秒，含）；`None` 表示不限。`limit` 是条数上限。
    /// 走只读连接池：回放不该参与写序，WAL 下读也不会挡住正在写新消息的 db 线程。
    pub fn list_inbound_history_for_bot(
        &self,
        bot_id: &BotId,
        from_received_at: Option<i64>,
        limit: u32,
    ) -> Result<Vec<InboundReplayRow>> {
        self.read(|connection| {
            // JOIN 之后 `id` / `external_id` 两张表都有，所以每一列都写表名限定。
            // 前 7 列的顺序与 `mappers::INBOUND_COLUMNS` 一致，`row_to_inbound` 不用改。
            let sql = "SELECT inbound.id, inbound.bot_id, inbound.external_id,
                              inbound.conversation_id, inbound.sender_id,
                              inbound.payload_json, inbound.received_at,
                              inbound.rowid, c.external_id, c.kind
                       FROM inbound
                       JOIN conversations AS c ON c.id = inbound.conversation_id
                       WHERE inbound.bot_id = ?1
                         AND (?2 IS NULL OR inbound.received_at >= ?2)
                       ORDER BY inbound.received_at, inbound.rowid
                       LIMIT ?3";
            let mut statement = connection.prepare(sql)?;
            let rows = statement.query_map(
                rusqlite::params![bot_id.to_string(), from_received_at, i64::from(limit)],
                |row| {
                    Ok(InboundReplayRow {
                        message: crate::mappers::row_to_inbound(row)?,
                        rowid: row.get(7)?,
                        conversation_external_id: row.get(8)?,
                        conversation_kind: row.get(9)?,
                    })
                },
            )?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(messages)
        })
    }

    /// 回放（FR-27）：取某个 bot 里 `reply_to` 落在给定集合中的历史出站，按
    /// `(created_at, rowid)` 升序。
    ///
    /// **为什么按 `reply_to` 而不是时间窗口**：一条出站行只靠时间无法可靠归属到某条入站 ——
    /// 同一毫秒可能有多条入站、handler 也可能跨事件延迟回复（`ctx.sleep`）。而被动回复的
    /// `outbound.reply_to` 就是入站行的 `external_id`（见
    /// `hbot_rpc::app::runtime::sandbox_handler` 的 `passive_reply_id`），这是库里唯一可靠的
    /// "这条回复来自哪条消息"的外键。
    ///
    /// `reply_to` 为空时直接返回空表（`IN ()` 是 SQL 语法错）。
    pub fn list_outbound_by_reply_to(
        &self,
        bot_id: &BotId,
        reply_to: &[String],
    ) -> Result<Vec<OutboundRow>> {
        if reply_to.is_empty() {
            return Ok(Vec::new());
        }
        self.read(|connection| {
            // 占位符按集合大小动态生成（没开 rusqlite 的 `rarray` 特性，不能用
            // `?1 IN rarray(...)`）。`?1` 固定是 bot_id，所以 reply_to 从 `?2` 起。
            let placeholders = (0..reply_to.len())
                .map(|index| format!("?{}", index + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {} FROM outbound
                 WHERE bot_id = ?1 AND reply_to IN ({placeholders})
                 ORDER BY created_at, rowid",
                crate::mappers::OUTBOUND_COLUMNS
            );
            let mut statement = connection.prepare(&sql)?;
            let mut values = Vec::with_capacity(reply_to.len() + 1);
            values.push(rusqlite::types::Value::Text(bot_id.to_string()));
            values.extend(
                reply_to
                    .iter()
                    .map(|text| rusqlite::types::Value::Text(text.clone())),
            );
            let rows = statement.query_map(
                rusqlite::params_from_iter(values),
                crate::mappers::row_to_outbound,
            )?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(messages)
        })
    }

    /// 按主键读会话；不存在返回 `Ok(None)`。
    pub fn get_conversation(&self, id: &ConversationId) -> Result<Option<Conversation>> {
        let id = *id;
        self.submit(|reply| StoreOp::GetConversation { id, reply })
    }

    /// 按幂等键 `(bot_id, external_id)` 读会话；不存在返回 `Ok(None)`。
    ///
    /// 适配器落地第一条消息时用它把平台会话换成内部 `conversation_id`（写 inbound 前
    /// 必须有会话行，见 [`Store::insert_inbound_if_new`] 的外键顺序）。
    pub fn get_conversation_by_external(
        &self,
        bot_id: &BotId,
        external_id: &str,
    ) -> Result<Option<Conversation>> {
        let bot_id = *bot_id;
        let external_id = external_id.to_owned();
        self.submit(|reply| StoreOp::GetConversationByExternal {
            bot_id,
            external_id,
            reply,
        })
    }

    /// 按 bot 列出全部会话，顺序固定为 `(kind, external_id)` 升序。
    ///
    /// ## 为什么排序必须是这个
    ///
    /// 排序必须是**确定性**的：UI 每次刷新都换一个顺序会被当成"会话在跳"，测试也会
    /// flaky。选 `(kind, external_id)` 而不是 `(created_at, rowid)` 的理由：
    ///
    /// * 两列全是业务列，重建库 / 整库重写之后取值不变；`rowid` 是 TEXT 主键表的隐式
    ///   列，物理重排可以重写它，按它排等于把顺序绑在物理布局上；
    /// * 同一 bot 内 `(kind, external_id)` 本身唯一（`UNIQUE(bot_id, external_id)`），
    ///   所以这是**全序**，不需要再补 tiebreaker；
    /// * 同 kind 的会话按平台侧 id 排，列表顺序与"发现会话的先后"无关、只与内容有关。
    ///
    /// 不在这里判断 bot 行是否存在：那是业务判断（`bot_conversations` 要报 `-32001`），
    /// 存储层对不存在的 bot 返回空表。
    pub fn list_conversations_for_bot(&self, bot_id: &BotId) -> Result<Vec<Conversation>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ListConversationsForBot { bot_id, reply })
    }

    /// 读最近的日志，最新在前；`bot_id = None` 表示跨 bot 汇总。
    ///
    /// 按 `logs.id`（AUTOINCREMENT，写入序）降序取前 `limit` 条——不按 `at` 排序，避免调用方
    /// 时钟回拨或同一毫秒多条时顺序不稳定。
    pub fn list_logs(&self, bot_id: Option<&BotId>, limit: u32) -> Result<Vec<LogRow>> {
        let bot_id = bot_id.copied();
        self.submit(|reply| StoreOp::ListLogs {
            bot_id,
            limit,
            reply,
        })
    }

    /// 插入一条设备会话，返回落库后的行。
    ///
    /// 走 db 线程（不是只读连接池）：刚建好的会话必须能被紧接着的
    /// [`Store::latest_device_session`] 读到，否则会出现"刚批准就说没批准"。
    pub fn insert_device_session(&self, session: NewDeviceSession) -> Result<DeviceSessionRow> {
        self.submit(|reply| StoreOp::InsertDeviceSession { session, reply })
    }

    /// 取这个 bot 最近一次设备会话；从没有过时返回 `Ok(None)`。
    ///
    /// 排序是 `(created_at DESC, id DESC)`，**不是** `id` 的字典序——UUIDv4 没有时间含义，
    /// `id DESC` 只在同一毫秒的两行之间做确定性定序（不依赖 rowid / 插入顺序）。
    ///
    /// 走 db 线程与写同序：撤销（[`Store::end_live_device_sessions`]）返回之后，下一次读
    /// 一定看到撤销后的状态。这条是安全性质，不是性能选择（撤销必须立即生效，`docs/09` §4.3）。
    pub fn latest_device_session(&self, bot_id: &BotId) -> Result<Option<DeviceSessionRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::LatestDeviceSession { bot_id, reply })
    }

    /// 改一条会话的状态（`ended_at` 一起写）；返回是否命中一行。
    ///
    /// `state` 原样落库，存储层不校验：合法取值是 `hbot_core::DeviceSessionState` 的
    /// `as_str()` 清单，判据在上层（存储层再抄一份状态表就是第二份真源）。`ended_at = None`
    /// 会清成 NULL；只想改状态、保留结束时刻的调用方要把原值一起传回来。
    pub fn update_device_session_state(
        &self,
        id: &str,
        state: &str,
        ended_at: Option<i64>,
    ) -> Result<bool> {
        let id = id.to_owned();
        let state = state.to_owned();
        self.submit(|reply| StoreOp::UpdateDeviceSessionState {
            id,
            state,
            ended_at,
            reply,
        })
    }

    /// 把这个 bot 所有 `state='active'` 的会话置为新状态并写 `ended_at`，返回处理行数。
    ///
    /// 撤销与到期共用它（`docs/09` §7）：一条 UPDATE 原子完成全局失效，不需要先读再逐条改
    /// —— 后者会在两条语句之间留下"库里已撤销、另一个读者还看到 active"的窗口。
    /// 非 `active` 行不动（它们已经结束，改写会擦掉历史）。
    pub fn end_live_device_sessions(
        &self,
        bot_id: &BotId,
        state: &str,
        ended_at: i64,
    ) -> Result<usize> {
        let bot_id = *bot_id;
        let state = state.to_owned();
        self.submit(|reply| StoreOp::EndLiveDeviceSessions {
            bot_id,
            state,
            ended_at,
            reply,
        })
    }

    /// 追加一条设备审计。
    ///
    /// `target` 只允许进程名 / 窗口 id / 子能力名；**像素、剪贴板文本、OCR 原文一律不许**
    /// （`docs/09` §3.2、§4.3）。表里根本没有能装内容的列——这是结构约束，不是调用约定。
    pub fn append_device_audit(&self, entry: NewDeviceAudit) -> Result<()> {
        self.submit(|reply| StoreOp::AppendDeviceAudit { entry, reply })
    }

    /// 读某个 bot 最近的设备审计，最新在前：`at DESC, id DESC`。
    ///
    /// 只按 bot 过滤（不跨 bot 汇总）：审计要回答"这个 bot 刚才动了什么"。同一毫秒的多条
    /// 用 `id`（AUTOINCREMENT = 写入序）定序，`at` 是调用方给的时钟，回拨时不保证单调。
    pub fn list_device_audit(&self, bot_id: &BotId, limit: usize) -> Result<Vec<DeviceAuditRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ListDeviceAudit {
            bot_id,
            limit,
            reply,
        })
    }

    /// 读一个 KV 键；不存在或**已过期**（`expires_at IS NOT NULL AND expires_at <= now`）
    /// 都返回 `Ok(None)`。
    ///
    /// ## 为什么走 db 线程而不是只读连接池
    ///
    /// 与 [`Store::latest_device_session`] 同一条理由：**顺序可见性**。`set_kv` 返回后，
    /// 调用方紧接着的 `get_kv` 必须能看到刚写的值——生成代码里"写标记 → 立刻检查标记"
    /// 是最常见的形状，走只读池会读到还没追上 WAL 的快照，表现为"刚写的丢了"。
    /// 这条路排在本线程已入队的写之后，代价是一次排队，换来的是**读写同序**。
    ///
    /// 已过期的行**不删**（本方法只有一条 SELECT）：清理是另一件事，读不许变成写。
    pub fn get_kv(&self, bot_id: &BotId, key: &str, now: i64) -> Result<Option<String>> {
        let bot_id = *bot_id;
        let key = key.to_owned();
        self.submit(|reply| StoreOp::GetKv {
            bot_id,
            key,
            now,
            reply,
        })
    }

    /// 写一个 KV 键（存在则覆盖）。`expires_at = None` 表示永不过期，并且会**把旧的到期时间
    /// 清成 NULL**（不是"保留旧值"）。
    ///
    /// `now` 只用于写 `updated_at`，由调用方注入（存储层不读时钟）。
    pub fn set_kv(
        &self,
        bot_id: &BotId,
        key: &str,
        value_json: &str,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<()> {
        let bot_id = *bot_id;
        let key = key.to_owned();
        let value_json = value_json.to_owned();
        self.submit(|reply| StoreOp::SetKv {
            bot_id,
            key,
            value_json,
            expires_at,
            now,
            reply,
        })
    }

    /// 删一个 KV 键；返回是否真的删到行（`false` = 本来就不存在）。
    ///
    /// 已过期的行也算删到（`true`）：本方法按主键删，不看 `expires_at`。
    pub fn delete_kv(&self, bot_id: &BotId, key: &str) -> Result<bool> {
        let bot_id = *bot_id;
        let key = key.to_owned();
        self.submit(|reply| StoreOp::DeleteKv { bot_id, key, reply })
    }

    /// 按 `key` 升序列出未过期的键名，最多 `limit` 条；`prefix` 是字面量前缀
    /// （LIKE 通配符 `%` / `_` / `\` 会被转义，不会被当成通配符）。
    ///
    /// 与写同序（走 db 线程）：`set_kv_many` 返回后紧接着的 `list_kv` 必须看得到那批键。
    pub fn list_kv(
        &self,
        bot_id: &BotId,
        prefix: Option<&str>,
        limit: u32,
        now: i64,
    ) -> Result<Vec<String>> {
        let bot_id = *bot_id;
        let prefix = prefix.map(str::to_owned);
        self.submit(|reply| StoreOp::ListKv {
            bot_id,
            prefix,
            limit,
            now,
            reply,
        })
    }

    /// 一次事务里批量 upsert；任何一条失败整批回滚（**不会**留下写了一半的状态）。
    ///
    /// 同批出现重复键时后者覆盖前者。空切片是合法的（不写、不报错）。`now` 是整批的
    /// `updated_at`（同一批共享一个写入时刻，便于排障时看出"这是一次批量写"）。
    pub fn set_kv_many(&self, bot_id: &BotId, entries: &[NewKvEntry], now: i64) -> Result<()> {
        let bot_id = *bot_id;
        let entries = entries.to_vec();
        self.submit(|reply| StoreOp::SetKvMany {
            bot_id,
            entries,
            now,
            reply,
        })
    }

    /// 注册一条会话等待（`ctx.session.prompt` 的落库动作）。
    ///
    /// 同一个 `(bot_id, conversation_id)` 已有 `pending` 行时返回
    /// [`StoreError::WaitPending`]（带挡路那条的键）—— 判据是 `0007` 的部分唯一索引，
    /// 不是这里先查一遍（先查后插在并发下两次都会查到「没有」）。
    pub fn register_wait(&self, bot_id: &BotId, wait: &NewWait) -> Result<WaitId> {
        let bot_id = *bot_id;
        let wait = wait.clone();
        self.submit(|reply| StoreOp::RegisterWait {
            bot_id,
            wait,
            reply,
        })
    }

    /// 一条消息唤醒唯一那个等待：挑 + 改 `fulfilled` + 返回，一条 SQL。
    ///
    /// `Ok(None)` = 这条消息不回答任何等待（正常路径）。已过期的等待不会被唤醒
    /// （`deadline > now` 条件在 SQL 里）。
    pub fn claim_wait(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        sender_id: &str,
        message_id: &str,
        now: i64,
    ) -> Result<Option<WaitRow>> {
        let bot_id = *bot_id;
        let conversation_id = *conversation_id;
        let sender_id = sender_id.to_owned();
        let message_id = message_id.to_owned();
        self.submit(|reply| StoreOp::ClaimWait {
            bot_id,
            conversation_id,
            sender_id,
            message_id,
            now,
            reply,
        })
    }

    /// 超时结算：把 `deadline <= now` 的 `pending` 行改成 `expired` 并返回它们。
    pub fn expire_due_waits(&self, bot_id: &BotId, now: i64, limit: u32) -> Result<Vec<WaitRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ExpireDueWaits {
            bot_id,
            now,
            limit,
            reply,
        })
    }

    /// 读这个会话当前未结的那条等待；没有返回 `Ok(None)`。
    pub fn current_wait(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
    ) -> Result<Option<WaitRow>> {
        let bot_id = *bot_id;
        let conversation_id = *conversation_id;
        self.submit(|reply| StoreOp::CurrentWait {
            bot_id,
            conversation_id,
            reply,
        })
    }

    /// 取消这个会话未结的那条等待；返回是否命中一行（终态时 `false`，不报错）。
    pub fn cancel_wait(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        now: i64,
    ) -> Result<bool> {
        let bot_id = *bot_id;
        let conversation_id = *conversation_id;
        self.submit(|reply| StoreOp::CancelWait {
            bot_id,
            conversation_id,
            now,
            reply,
        })
    }

    /// 数一个 bot 的未结等待（`ctx.session.prompt` 的容量闸判据）。
    pub fn count_active_waits_for_bot(&self, bot_id: &BotId) -> Result<u32> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::CountPendingWaitsForBot { bot_id, reply })
    }

    /// `bot_stop`：取消这个 bot 的全部未结等待，返回取消了几条。
    pub fn cancel_waits_for_bot(&self, bot_id: &BotId, now: i64) -> Result<u32> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::CancelWaitsForBot { bot_id, now, reply })
    }

    /// 入队一条后台作业；命中 `(bot_id, idempotency_key)` 时返回 `Ok(None)`。
    ///
    /// 返回 `None` **不是错误**：它表示"这个幂等键已经排过队了"。至少一次语义靠这条
    /// 唯一约束落地，调用方据此记一条 debug 即可，不该把它当成失败。
    ///
    /// 走 db 线程而不是只读池：入队必须与"紧接着的 `claim_due_jobs`"同序，否则 tick
    /// 会在下一次轮到它之前看不到刚排进来的作业（表现为"入队了但一分钟没动").
    pub fn enqueue_job(&self, bot_id: &BotId, job: &NewJob) -> Result<Option<JobId>> {
        let bot_id = *bot_id;
        let job = job.clone();
        self.submit(|reply| StoreOp::EnqueueJob { bot_id, job, reply })
    }

    /// 取到期作业（`state = 'pending' AND next_at <= now`），就地改成 `running` 并把
    /// `attempts` 加一，返回改完的行；一次最多 `limit` 条。
    ///
    /// 一次 SQL 完成"挑 + 改"（理由见 `repositories::jobs` 模块文档）。`limit == 0`
    /// 返回空表。
    pub fn claim_due_jobs(&self, now: i64, limit: u32) -> Result<Vec<JobRow>> {
        self.submit(|reply| StoreOp::ClaimDueJobs { now, limit, reply })
    }

    /// 取**这个 bot** 的到期作业（`state = 'pending' AND next_at <= now`），就地改成
    /// `running` 并把 `attempts` 加一，返回改完的行；一次最多 `limit` 条。
    ///
    /// 与 [`Self::claim_due_jobs`] 同一份 SQL 判据，唯一区别是多一个 `bot_id` 条件。
    /// 作业派发**只能**走这一个：每个 bot 有自己的 tick 循环，全局 claim 会让 A 把 B 的
    /// 作业抢走并在 A 的 isolate 里跑（跨 bot 隔离当场破，而且日志里看不出来）。
    pub fn claim_due_jobs_for_bot(
        &self,
        bot_id: &BotId,
        now: i64,
        limit: u32,
    ) -> Result<Vec<JobRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ClaimDueJobsForBot {
            bot_id,
            now,
            limit,
            reply,
        })
    }

    /// 按主键读一条作业；不存在返回 `Ok(None)`。
    pub fn get_job(&self, id: &JobId) -> Result<Option<JobRow>> {
        let id = *id;
        self.submit(|reply| StoreOp::GetJob { id, reply })
    }

    /// 标成功：落 `done` 终态并清掉 `last_error`；返回是否命中一行。
    pub fn mark_job_done(&self, id: &JobId, now: i64) -> Result<bool> {
        let id = *id;
        self.submit(|reply| StoreOp::MarkJobDone { id, now, reply })
    }

    /// 排下一次重试：回 `pending`、把 `next_at` 换成调用方算好的时刻、记下本次错误。
    ///
    /// `attempts` 不动：它在 `claim_due_jobs` 里已经加过了（"这一次跑过了"）。
    /// 退避节奏不在这里算 —— 它是 `hbot_core::bot_jobs::retry_delay_ms` 的事。
    pub fn reschedule_job(&self, id: &JobId, next_at: i64, error: &str, now: i64) -> Result<bool> {
        let id = *id;
        let error = error.to_owned();
        self.submit(|reply| StoreOp::RescheduleJob {
            id,
            next_at,
            error,
            now,
            reply,
        })
    }

    /// 落 `failed` 终态并写错误正文；返回是否命中一行。
    ///
    /// 行保留不删：面板要看得到"哪条作业放弃了、为什么"，排障也需要原始载荷。
    pub fn mark_job_failed(&self, id: &JobId, error: &str, now: i64) -> Result<bool> {
        let id = *id;
        let error = error.to_owned();
        self.submit(|reply| StoreOp::MarkJobFailed {
            id,
            error,
            now,
            reply,
        })
    }

    /// `bot_stop`：把这个 bot 的未完成作业（`pending` / `running`）全改成 `cancelled`，
    /// 返回取消了几条。终态不动 —— 已经成功的不该被改写成"取消"。
    pub fn cancel_jobs_for_bot(&self, bot_id: &BotId, now: i64) -> Result<u32> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::CancelJobsForBot { bot_id, now, reply })
    }

    /// 进程启动时调用：把库里残留的 `running` 行退回 `pending`（`next_at = now`），
    /// 返回恢复了几条。
    ///
    /// 这是"至少一次"里 `running` 那一半：崩在一次执行中间时，没有任何东西能证明那次
    /// 跑完了，所以退回待跑、由作业体自己的幂等兜住重复执行。跨 bot 生效（进程刚起，
    /// 谁都没在跑）。
    pub fn resume_interrupted_jobs(&self, now: i64) -> Result<u32> {
        self.submit(|reply| StoreOp::ResumeInterruptedJobs { now, reply })
    }

    /// 数一个 bot 还有多少条未完成作业（`pending` / `running`）—— 作业池上限的判据。
    pub fn count_active_jobs_for_bot(&self, bot_id: &BotId) -> Result<u32> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::CountActiveJobsForBot { bot_id, reply })
    }

    /// 列一个 bot 的作业行，按入队时刻升序，最多 `limit` 条（面板与排障用）。
    pub fn list_jobs_for_bot(&self, bot_id: &BotId, limit: u32) -> Result<Vec<JobRow>> {
        let bot_id = *bot_id;
        self.submit(|reply| StoreOp::ListJobsForBot {
            bot_id,
            limit,
            reply,
        })
    }

    /// 会话历史：把 `inbound` 与 `outbound` 合成一张表，返回最近的 `limit` 条，
    /// **时间升序、最老的在前**（聊天记录的自然阅读顺序）。`limit == 0` 返回空表。
    ///
    /// 同一毫秒的多条用 `message_id` 作次级键，所以两次调用顺序一致。
    ///
    /// ## 为什么走只读连接池（不是 db 线程）
    ///
    /// 历史是**只追加**的账本：调用方读的是"已经发生过的对话"，不回读自己刚写的那一行
    /// （写历史的是 supervisor，读的是生成代码，两者不在同一个先后依赖里）。WAL 下只读
    /// 连接不会挡住正在写新消息的 db 线程，所以这条读不该排进写队列。
    /// 与之相反的是 KV 读（[`Store::get_kv`]）：那边"写完立刻读"是合法且常见的用法。
    pub fn list_history(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        limit: u32,
    ) -> Result<Vec<HistoryRow>> {
        let bot_id = *bot_id;
        let conversation_id = *conversation_id;
        self.read(|connection| {
            repositories::history::history(connection, &bot_id, &conversation_id, limit)
        })
    }

    /// [`Store::list_history`] 的 db 线程孪生路径（仅测试）。
    ///
    /// 存在的唯一目的是让 `crate::tests::history_reads_agree_on_both_paths` 对拍两条路径：
    /// 生产读走只读池，但如果将来有人把 `list_history` 改成走 db 线程（"读写同序"那条路），
    /// 结果必须一模一样。如果这里和只读池的结果出现差异，说明两条路径中间有人写了第二份 SQL。
    #[cfg(test)]
    pub(crate) fn list_history_via_writer(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        limit: u32,
    ) -> Result<Vec<HistoryRow>> {
        let bot_id = *bot_id;
        let conversation_id = *conversation_id;
        self.submit(|reply| StoreOp::ListHistory {
            bot_id,
            conversation_id,
            limit,
            reply,
        })
    }

    /// 把操作投递给 db 线程并等回复。
    ///
    /// `op` 在持有通道锁时构造，保证 `Shutdown` 之后不会再有请求入队。
    /// 阻塞到 db 线程执行完为止——**异步上下文里必须包 `tokio::task::spawn_blocking`**，
    /// 否则会占住 tokio worker（见 crate 文档）。
    #[must_use = "操作结果必须检查，否则失败会被静默丢掉"]
    fn submit<T>(&self, op: impl FnOnce(Reply<T>) -> StoreOp) -> Result<T> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        {
            let guard = self
                .ops
                .lock()
                .map_err(|_| StoreError::Poisoned("store op channel"))?;
            let sender = guard.as_ref().ok_or(StoreError::WriterStopped)?;
            sender
                .send(op(reply_sender))
                .map_err(|_| StoreError::WriterStopped)?;
        }
        reply_receiver
            .recv()
            .map_err(|_| StoreError::WriterStopped)?
    }

    /// 在只读连接上执行一次查询。
    ///
    /// 同样阻塞——异步上下文里必须包 `tokio::task::spawn_blocking`。
    #[must_use = "查询结果必须检查，否则读失败会被静默丢掉"]
    fn read<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.reads.with(read)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // 先让 db 线程处理完已排队的操作并退出，再关闭只读连接（字段顺序保证后者）。
        match self.ops.lock() {
            Ok(mut guard) => {
                if let Some(sender) = guard.take() {
                    if sender.send(StoreOp::Shutdown).is_err() {
                        tracing::debug!("store op channel already closed before shutdown");
                    }
                }
            }
            Err(poisoned) => {
                tracing::warn!("store op channel lock poisoned: {}", poisoned.to_string())
            }
        }

        match self.ops_thread.lock() {
            Ok(mut guard) => {
                if let Some(handle) = guard.take() {
                    if handle.join().is_err() {
                        tracing::warn!("store db thread panicked");
                    }
                }
            }
            Err(poisoned) => {
                tracing::warn!("store db thread lock poisoned: {}", poisoned.to_string())
            }
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("read_connections", &READ_CONNECTIONS)
            .field("busy_timeout_ms", &BUSY_TIMEOUT_MS)
            .finish_non_exhaustive()
    }
}

/// 简单轮转的只读连接池：连接数固定，`Mutex<Connection>` 保证一次只被一条查询使用。
struct ReadPool {
    connections: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

impl ReadPool {
    /// `connections` 必须非空；本类型只在 `Store::open` 内以固定数量构造。
    fn new(connections: Vec<Mutex<Connection>>) -> Self {
        Self {
            connections,
            next: AtomicUsize::new(0),
        }
    }

    fn with<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.connections.len();
        let guard = self.connections[index]
            .lock()
            .map_err(|_| StoreError::Poisoned("read connection"))?;
        read(&guard)
    }
}

/// db 线程主体：串行执行写与有序读，直到收到 [`StoreOp::Shutdown`] 或通道关闭。
fn store_loop(connection: Connection, receiver: Receiver<StoreOp>) {
    while let Ok(op) = receiver.recv() {
        match op {
            StoreOp::Shutdown => break,
            StoreOp::InsertProject { project, reply } => {
                reply_send(reply, repositories::project::insert(&connection, &project));
            }
            StoreOp::GetProjectByPath { path, reply } => {
                reply_send(
                    reply,
                    repositories::project::get_by_path(&connection, &path),
                );
            }
            StoreOp::InsertBot { bot, reply } => {
                reply_send(reply, repositories::bot::insert(&connection, &bot));
            }
            StoreOp::InsertConversation {
                conversation,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::conversation::insert(&connection, &conversation),
                );
            }
            StoreOp::InsertInbound {
                message,
                sender_id,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::inbound::insert_if_new(&connection, &message, &sender_id),
                );
            }
            StoreOp::MarkInboundProcessed {
                id,
                processed_at,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::inbound::mark_processed(&connection, &id, processed_at),
                );
            }
            StoreOp::EnqueueOutbound {
                message,
                target,
                reply,
            } => {
                let outcome = repositories::outbound::enqueue(&connection, &message, &target)
                    .map(|()| message.id);
                reply_send(reply, outcome);
            }
            StoreOp::MarkOutboundSent { id, sent_at, reply } => {
                reply_send(
                    reply,
                    repositories::outbound::mark_sent(&connection, &id, sent_at),
                );
            }
            StoreOp::MarkOutboundAttempt { id, reply } => {
                reply_send(
                    reply,
                    repositories::outbound::bump_attempt(&connection, &id),
                );
            }
            StoreOp::MarkOutboundFailed {
                id,
                attempts,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::outbound::mark_failed(&connection, &id, attempts),
                );
            }
            StoreOp::AppendLog { entry, reply } => {
                reply_send(reply, repositories::log::append(&connection, &entry));
            }
            StoreOp::AppendAgentLog { entry, reply } => {
                reply_send(reply, repositories::log::append_agent(&connection, &entry));
            }
            StoreOp::UpdateBotStatus {
                id,
                status,
                updated_at,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::bot::update_status(&connection, &id, &status, updated_at),
                );
            }
            StoreOp::ListPendingOutbound { limit, reply } => {
                reply_send(
                    reply,
                    repositories::outbound::list_pending(&connection, limit),
                );
            }
            StoreOp::ListPendingOutboundForBot {
                bot_id,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::outbound::list_pending_for_bot(&connection, &bot_id, limit),
                );
            }
            StoreOp::CountPendingOutbound { reply } => {
                reply_send(reply, repositories::outbound::count_pending(&connection));
            }
            StoreOp::GetOutbound { id, reply } => {
                reply_send(reply, repositories::outbound::get(&connection, &id));
            }
            StoreOp::ListUnprocessedInbound { limit, reply } => {
                reply_send(
                    reply,
                    repositories::inbound::list_unprocessed(&connection, limit),
                );
            }
            StoreOp::ListInboundReplay {
                bot_id,
                after,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::inbound::list_replay_for_bot(&connection, &bot_id, after, limit),
                );
            }
            StoreOp::GetConversation { id, reply } => {
                reply_send(reply, repositories::conversation::get(&connection, &id));
            }
            StoreOp::GetConversationByExternal {
                bot_id,
                external_id,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::conversation::get_by_external(&connection, &bot_id, &external_id),
                );
            }
            StoreOp::ListConversationsForBot { bot_id, reply } => {
                reply_send(reply, conversations_for_bot(&connection, &bot_id));
            }
            StoreOp::GetBotStatus { id, reply } => {
                reply_send(reply, repositories::bot::get_status(&connection, &id));
            }
            StoreOp::ListLogs {
                bot_id,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::log::list(&connection, bot_id.as_ref(), limit),
                );
            }
            StoreOp::InsertDeviceSession { session, reply } => {
                reply_send(
                    reply,
                    repositories::device::insert_session(&connection, &session),
                );
            }
            StoreOp::LatestDeviceSession { bot_id, reply } => {
                reply_send(
                    reply,
                    repositories::device::latest_session(&connection, &bot_id),
                );
            }
            StoreOp::UpdateDeviceSessionState {
                id,
                state,
                ended_at,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::device::update_state(&connection, &id, &state, ended_at),
                );
            }
            StoreOp::EndLiveDeviceSessions {
                bot_id,
                state,
                ended_at,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::device::end_live_sessions(&connection, &bot_id, &state, ended_at),
                );
            }
            StoreOp::AppendDeviceAudit { entry, reply } => {
                reply_send(
                    reply,
                    repositories::device::append_audit(&connection, &entry),
                );
            }
            StoreOp::ListDeviceAudit {
                bot_id,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::device::list_audit(&connection, &bot_id, limit),
                );
            }
            StoreOp::GetKv {
                bot_id,
                key,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::kv::get(&connection, &bot_id, &key, now),
                );
            }
            StoreOp::SetKv {
                bot_id,
                key,
                value_json,
                expires_at,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::kv::set(&connection, &bot_id, &key, &value_json, expires_at, now),
                );
            }
            StoreOp::DeleteKv { bot_id, key, reply } => {
                reply_send(reply, repositories::kv::delete(&connection, &bot_id, &key));
            }
            StoreOp::ListKv {
                bot_id,
                prefix,
                limit,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::kv::list(&connection, &bot_id, prefix.as_deref(), limit, now),
                );
            }
            StoreOp::SetKvMany {
                bot_id,
                entries,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::kv::set_many(&connection, &bot_id, &entries, now),
                );
            }
            StoreOp::EnqueueJob { bot_id, job, reply } => {
                reply_send(
                    reply,
                    repositories::jobs::enqueue(&connection, &bot_id, &job),
                );
            }
            StoreOp::ClaimDueJobs { now, limit, reply } => {
                reply_send(
                    reply,
                    repositories::jobs::claim_due(&connection, now, limit),
                );
            }
            StoreOp::ClaimDueJobsForBot {
                bot_id,
                now,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::jobs::claim_due_for_bot(&connection, &bot_id, now, limit),
                );
            }
            StoreOp::GetJob { id, reply } => {
                reply_send(reply, repositories::jobs::get(&connection, &id));
            }
            StoreOp::MarkJobDone { id, now, reply } => {
                reply_send(reply, repositories::jobs::mark_done(&connection, &id, now));
            }
            StoreOp::RescheduleJob {
                id,
                next_at,
                error,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::jobs::reschedule(&connection, &id, next_at, &error, now),
                );
            }
            StoreOp::MarkJobFailed {
                id,
                error,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::jobs::mark_failed(&connection, &id, &error, now),
                );
            }
            StoreOp::CancelJobsForBot { bot_id, now, reply } => {
                reply_send(
                    reply,
                    repositories::jobs::cancel_pending_for_bot(&connection, &bot_id, now),
                );
            }
            StoreOp::ResumeInterruptedJobs { now, reply } => {
                reply_send(
                    reply,
                    repositories::jobs::resume_interrupted(&connection, now),
                );
            }
            StoreOp::RegisterWait {
                bot_id,
                wait,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::waits::register(&connection, &bot_id, &wait),
                );
            }
            StoreOp::ClaimWait {
                bot_id,
                conversation_id,
                sender_id,
                message_id,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::waits::claim(
                        &connection,
                        &bot_id,
                        &conversation_id,
                        &sender_id,
                        &message_id,
                        now,
                    ),
                );
            }
            StoreOp::ExpireDueWaits {
                bot_id,
                now,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::waits::expire_due(&connection, &bot_id, now, limit),
                );
            }
            StoreOp::CurrentWait {
                bot_id,
                conversation_id,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::waits::current(&connection, &bot_id, &conversation_id),
                );
            }
            StoreOp::CancelWait {
                bot_id,
                conversation_id,
                now,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::waits::cancel(&connection, &bot_id, &conversation_id, now),
                );
            }
            StoreOp::CountPendingWaitsForBot { bot_id, reply } => {
                reply_send(
                    reply,
                    repositories::waits::count_pending_for_bot(&connection, &bot_id),
                );
            }
            StoreOp::CancelWaitsForBot { bot_id, now, reply } => {
                reply_send(
                    reply,
                    repositories::waits::cancel_for_bot(&connection, &bot_id, now),
                );
            }
            StoreOp::CountActiveJobsForBot { bot_id, reply } => {
                reply_send(
                    reply,
                    repositories::jobs::count_active_for_bot(&connection, &bot_id),
                );
            }
            StoreOp::ListJobsForBot {
                bot_id,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::jobs::list_for_bot(&connection, &bot_id, limit),
                );
            }
            StoreOp::ListHistory {
                bot_id,
                conversation_id,
                limit,
                reply,
            } => {
                reply_send(
                    reply,
                    repositories::history::history(&connection, &bot_id, &conversation_id, limit),
                );
            }
        }
    }
    tracing::debug!("store db thread stopped");
}

/// 把结果发回调用方。接收端已消失（调用方被取消）不是错误，记 debug 即可。
fn reply_send<T>(reply: Reply<T>, result: Result<T>) {
    if reply.send(result).is_err() {
        tracing::debug!("store caller dropped the reply channel");
    }
}

/// `SELECT … FROM conversations WHERE bot_id = ?1 ORDER BY kind, external_id`。
///
/// 列清单直接复用 [`crate::mappers::CONVERSATION_COLUMNS`] + [`crate::mappers::row_to_conversation`]：
/// 手抄列名会在加列时静默错位（见 mappers 模块文档）。
///
/// 为什么这段 SQL 在 `db.rs` 而不是 `repositories/conversation.rs`：本轮改动边界只允许
/// 碰 `db.rs`（`repositories/` 不在可改清单里）。`db.rs` 里已经有两处同形状的先例
/// （`list_inbound_history_for_bot` / `list_outbound_by_reply_to` 的 JOIN 读）。
/// 边界放开后整段搬进 repository 即可，行为与排序不变。
fn conversations_for_bot(connection: &Connection, bot_id: &BotId) -> Result<Vec<Conversation>> {
    let sql = format!(
        "SELECT {} FROM conversations WHERE bot_id = ?1 ORDER BY kind, external_id",
        crate::mappers::CONVERSATION_COLUMNS
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([bot_id.to_string()], crate::mappers::row_to_conversation)?;
    let mut conversations = Vec::new();
    for row in rows {
        conversations.push(row?);
    }
    Ok(conversations)
}

// 当前测试线程上打开过的连接数（仅 `cfg(test)` 编译）。
//
// 为什么需要这个探针：ADR-0002 §4 的启动预算靠"ready 之前只开必要连接"兜住，
// 而多开几条连接没有任何可观察的输出——功能测试照样全绿，只是 ready 晚几毫秒。
// 用它把"顺手加个预热连接/连接池"挡在启动路径外面（`crate::tests` 里断言）。
//
// 计到**线程本地**而不是全局 `AtomicUsize`：store 的单元测试是并行跑的，
// 别的测试同时 `Store::open` 会把全局计数抬高，断言就变成 flaky 的。
#[cfg(test)]
thread_local! {
    static CONNECTION_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 记一次连接打开（仅测试）。在 [`open_connection`] 里 `Connection::open` 成功之后调用。
#[cfg(test)]
fn note_connection_open() {
    CONNECTION_OPENS.with(|count| count.set(count.get() + 1));
}

/// 当前线程已打开的连接数（仅测试）。
#[cfg(test)]
pub(crate) fn connection_opens_on_this_thread() -> usize {
    CONNECTION_OPENS.with(|count| count.get())
}

/// 清零当前线程的连接计数（仅测试，断言前调用）。
#[cfg(test)]
pub(crate) fn reset_connection_opens_on_this_thread() {
    CONNECTION_OPENS.with(|count| count.set(0));
}

/// 打开一个连接并施加本层的 PRAGMA 契约。
///
/// `role` 只用于启动计时（`role=writer` / `role=read0` / `role=read1`），不参与逻辑。
fn open_connection(path: &Path, role: &'static str) -> Result<Connection> {
    let phase = startup_trace::Phase::start("store_conn_open");
    let connection = Connection::open(path)?;
    #[cfg(test)]
    note_connection_open();
    phase.done_with(role);

    // 先给忙等窗口，再切 WAL：切 journal_mode 需要短暂排他，否则并发启动会直接 SQLITE_BUSY。
    let phase = startup_trace::Phase::start("store_conn_wal");
    connection.execute_batch(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"))?;

    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case(JOURNAL_MODE_WAL) {
        return Err(StoreError::JournalMode {
            actual: journal_mode,
        });
    }
    phase.done_with(role);

    // synchronous 与 foreign_keys 都是“每连接”设置，必须对每个连接显式施加。
    let phase = startup_trace::Phase::start("store_conn_pragmas");
    connection.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    phase.done_with(role);

    Ok(connection)
}

/// Unix 毫秒。仅用于迁移记账；业务时间一律由调用方传入，便于测试注入。
pub(crate) fn now_unix_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // 系统时钟早于 1970：退化成 0，不让记账逻辑 panic。
        Err(_) => 0,
    }
}
