//! 测试夹具：临时目录 + 固定 UUID + 种子数据。
//!
//! 只给 `#[cfg(test)]` 用。临时目录建在系统临时区，`Drop` 时清理，不写用户目录
//! （`docs/05-quality-gates.md` §3 测试规则）。
//!
//! ID 统一用 [`Uuid::from_u128`] 构造小整数，断言里能直接看出是哪一行
//! （例如 `inbound_id(101)` ↔ `1001…`），且不会因为随机 UUID 导致失败不可复现。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use uuid::Uuid;

use crate::Store;
use crate::ids::{BotId, ConversationId, InboundId, JobId, OutboundId, ProjectId};
use crate::models::{
    Bot, Conversation, NewInbound, NewJob, NewLog, NewOutbound, OutboundTarget, Project,
};

/// 种子项目 id 的 `u128` 值。
pub(crate) const PROJECT_SEED: u128 = 1;
/// 种子 bot id 的 `u128` 值。
pub(crate) const BOT_SEED: u128 = 2;
/// 种子会话 id 的 `u128` 值。
pub(crate) const CONVERSATION_SEED: u128 = 3;

/// 固定 UUID：测试里用可读的小整数，不用随机值。
pub(crate) fn fixed_uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

/// 种子项目 ID。
pub(crate) fn project_id() -> ProjectId {
    ProjectId::from_uuid(fixed_uuid(PROJECT_SEED))
}

/// 任意项目 ID（同值可复现）。
pub(crate) fn project_id_at(value: u128) -> ProjectId {
    ProjectId::from_uuid(fixed_uuid(value))
}

/// 种子 bot ID。
pub(crate) fn bot_id() -> BotId {
    BotId::from_uuid(fixed_uuid(BOT_SEED))
}

/// 任意 bot ID。
pub(crate) fn bot_id_at(value: u128) -> BotId {
    BotId::from_uuid(fixed_uuid(value))
}

/// 种子会话 ID。
pub(crate) fn conversation_id() -> ConversationId {
    ConversationId::from_uuid(fixed_uuid(CONVERSATION_SEED))
}

/// 任意入站行 ID。
pub(crate) fn inbound_id(value: u128) -> InboundId {
    InboundId::from_uuid(fixed_uuid(value))
}

/// 任意出站行 ID。
pub(crate) fn outbound_id(value: u128) -> OutboundId {
    OutboundId::from_uuid(fixed_uuid(value))
}

/// 任意作业 ID（同值可复现）。
pub(crate) fn job_id(value: u128) -> JobId {
    JobId::from_uuid(fixed_uuid(value))
}

/// 造一条指向种子 bot 的待入队作业：`next_at` 与 `created_at` 都取 `at`。
pub(crate) fn new_job(id: u128, key: &str, at: i64) -> NewJob {
    NewJob {
        id: job_id(id),
        name: "settle-war".to_owned(),
        payload_json: "{\"warId\":\"w-1\"}".to_owned(),
        idempotency_key: key.to_owned(),
        budget_ms: 60_000,
        conversation: None,
        next_at: at,
        created_at: at,
    }
}

/// 一次测试独占的临时目录。
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        // 进程内计数 + 纳秒，避免并行测试互相撞目录。
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "hbot-store-test-{label}-{}-{nanos}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir for store test");
        Self { path }
    }

    /// 库文件路径。
    pub(crate) fn db_path(&self) -> PathBuf {
        self.path.join("hbot.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            // Windows 上 WAL/SHM 仍被占用时会失败；留给系统清理，不算测试失败。
            eprintln!("could not remove temp dir {}: {error}", self.path.display());
        }
    }
}

/// 打开库并写入一个项目 / bot / 会话。
pub(crate) fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.db_path()).expect("open store");
    seed(&store);
    store
}

/// 写入基础行：项目 → bot → 会话（外键顺序，`AGENTS.md` §4.1）。
pub(crate) fn seed(store: &Store) {
    store
        .insert_project(&Project {
            id: project_id(),
            name: "demo project".to_owned(),
            path: "/tmp/hbot-demo".to_owned(),
            created_at: 1,
        })
        .expect("insert project");
    store
        .insert_bot(&Bot {
            id: bot_id(),
            project_id: project_id(),
            name: "demo-bot".to_owned(),
            platform: "onebot".to_owned(),
            config_json: "{\"token\":\"redacted\"}".to_owned(),
            enabled: true,
            created_at: 2,
        })
        .expect("insert bot");
    store
        .insert_conversation(&Conversation {
            id: conversation_id(),
            bot_id: bot_id(),
            external_id: "group-42".to_owned(),
            kind: "group".to_owned(),
            created_at: 3,
        })
        .expect("insert conversation");
}

/// 再写一只 bot（同一个项目下），用于跨 bot 隔离的断言。
pub(crate) fn seed_extra_bot(store: &Store, value: u128) -> BotId {
    let id = bot_id_at(value);
    store
        .insert_bot(&Bot {
            id,
            project_id: project_id(),
            name: format!("extra-{value}"),
            platform: "onebot".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 2,
        })
        .expect("insert extra bot");
    id
}

/// 造一条指向种子 bot / 会话的入站消息。`id` 是固定 UUID 的小整数形式。
pub(crate) fn new_inbound(id: u128, external_id: &str) -> NewInbound {
    new_inbound_at(id, external_id, 1_000)
}

/// 同 [`new_inbound`]，但指定 `received_at`（用于断言未处理队列的排序）。
pub(crate) fn new_inbound_at(id: u128, external_id: &str, received_at: i64) -> NewInbound {
    NewInbound {
        id: inbound_id(id),
        bot_id: bot_id(),
        external_id: external_id.to_owned(),
        conversation_id: conversation_id(),
        payload_json: "{\"text\":\"hello\"}".to_owned(),
        received_at,
    }
}

/// 造一条指向种子 bot / 会话的出站消息。
pub(crate) fn new_outbound(id: u128) -> NewOutbound {
    new_outbound_at(id, "reply", 2_000)
}

/// 同 [`new_outbound`]，但指定正文与 `created_at`。
pub(crate) fn new_outbound_at(id: u128, text: &str, created_at: i64) -> NewOutbound {
    NewOutbound {
        id: outbound_id(id),
        bot_id: bot_id(),
        conversation_id: conversation_id(),
        text: text.to_owned(),
        created_at,
    }
}

/// 造一份出站路由信息。
pub(crate) fn outbound_target(target_json: &str, reply_to: Option<&str>) -> OutboundTarget {
    OutboundTarget::new(target_json, reply_to.map(str::to_owned))
}

/// 造一条指向种子 bot 的日志。
pub(crate) fn new_log(level: &str, line: &str, at: i64) -> NewLog {
    NewLog {
        bot_id: bot_id(),
        level: level.to_owned(),
        line: line.to_owned(),
        at,
    }
}

/// 另开一条裸连接，用于断言落库结果（不经过本层的连接池）。
///
/// 调用前应 `drop(store)`，避免与单 writer 线程竞争同一份写状态。
pub(crate) fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.db_path()).expect("open raw sqlite connection")
}
