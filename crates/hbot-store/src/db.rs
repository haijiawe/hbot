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
use crate::ids::{BotId, ConversationId, InboundId, OutboundId};
use crate::migrations;
use crate::models::{
    BOT_STATUSES, Bot, BotStatusRow, Conversation, InboundReplayRow, InboundRow, LogRow,
    NewInbound, NewLog, NewOutbound, OutboundRow, OutboundTarget, Project, is_valid_bot_status,
};
use crate::repositories;

/// 写/读连接统一的忙等窗口（毫秒）：遇到锁时最多重试 5s，再报 `SQLITE_BUSY`。
pub const BUSY_TIMEOUT_MS: u32 = 5_000;

/// 只读连接数量（`docs/02-architecture.md` §7 建议 2–4）。
const READ_CONNECTIONS: usize = 2;

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
    GetBotStatus {
        id: BotId,
        reply: Reply<Option<BotStatusRow>>,
    },
    ListLogs {
        bot_id: Option<BotId>,
        limit: u32,
        reply: Reply<Vec<LogRow>>,
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
        let mut writer = open_connection(path)?;
        let schema_version = migrations::apply(&mut writer, now_unix_ms())?;

        let mut read_connections = Vec::with_capacity(READ_CONNECTIONS);
        for _ in 0..READ_CONNECTIONS {
            read_connections.push(Mutex::new(open_connection(path)?));
        }

        let (sender, receiver) = mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let ops_thread = std::thread::Builder::new()
            .name("hbot-store-writer".to_owned())
            .spawn(move || store_loop(writer, receiver))?;

        tracing::info!(
            path = %path.display(),
            schema_version,
            queue_capacity = WRITE_QUEUE_CAPACITY,
            read_connections = READ_CONNECTIONS,
            "hbot-store opened"
        );

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

/// 打开一个连接并施加本层的 PRAGMA 契约。
fn open_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;

    // 先给忙等窗口，再切 WAL：切 journal_mode 需要短暂排他，否则并发启动会直接 SQLITE_BUSY。
    connection.execute_batch(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"))?;

    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case(JOURNAL_MODE_WAL) {
        return Err(StoreError::JournalMode {
            actual: journal_mode,
        });
    }

    // synchronous 与 foreign_keys 都是“每连接”设置，必须对每个连接显式施加。
    connection.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;

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
