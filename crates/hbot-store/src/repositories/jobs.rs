//! `bot_jobs` —— 后台作业的持久化队列（C4.jobs，docs/07-bot-runtime-blueprint.md 的 4.3 节）。
//!
//! 一层薄 SQL：入队（幂等）、取到期（claim）、成功 / 重试 / 失败 / 取消、按 bot 计数。
//! **不认识业务**：不读时钟（`now` 一律调用方传），不解析载荷，不决定重试节奏
//! （那是 `hbot_core::bot_jobs::retry_delay_ms` 与装配层的事）。
//!
//! ## 为什么"取到期"必须是"取到期 + 改状态"的一步（硬规则）
//!
//! `claim_due` 在一个事务里做三件事：选出 `state = 'pending' AND next_at <= now` 的行、
//! 把它们改成 `running`、`attempts` 加一，然后**返回改完之后的行**。
//! 如果拆成"先 SELECT 再 UPDATE"，两条语句之间就有一个窗口：
//!
//! * 同一个 bot 的两个派发者（tick 与重启恢复）会拿到同一行，作业跑两次；
//! * 进程在两者之间被杀，那一行留在 `pending` 但 `attempts` 没加，
//!   重试次数也就不再能从根上封顶（真机上表现为"这条永远失败、永远在重试"）。
//!
//! 用 `UPDATE … RETURNING` 一条语句拿到"我刚改掉的行"，是这个窗口唯一的关法。
//!
//! ## 为什么 enqueue 用 `ON CONFLICT DO NOTHING` 而不是先查后插
//!
//! 幂等键的去重依据是**唯一约束**，不是一次查询：先查后插在并发下会让两个请求都查到
//! "不存在"然后都去插，结果第二个撞约束报错 —— 调用方看到的是一个本该成功的入队失败。
//! `DO NOTHING` + 返回 `None` 把"重复入队"变成正常结果（这正是至少一次语义要的形状）。
//!
//! ## 越界的 attempts / budget 怎么读
//!
//! 库里的 `attempts` 是 INTEGER（i64），读回时 `u32::try_from` 失败按 `u32::MAX` 处理。
//! 这不是宽容，是"库被外部工具改过"时的可解释行为：读到天花板的行会被上层当成重试已耗尽。
//! 主键列读不成 UUID 则直接报 `FromSqlConversionFailure`：那说明库坏了，
//! 不该被伪装成一条能跑的作业。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::{BotId, JobId};
use crate::models::{JobRow, NewJob};

/// 作业状态字符串：与 `hbot_core::bot_jobs::JobState::as_str()` 同值。
///
/// 存储层需要它来写 WHERE / SET（SQL 里不能出现 Rust 枚举），所以这里只放一份常量。
/// 拼写对拍在 `crate::tests`。
pub(crate) const STATE_PENDING: &str = "pending";
/// 正在执行。
pub(crate) const STATE_RUNNING: &str = "running";
/// 成功终态。
pub(crate) const STATE_DONE: &str = "done";
/// 失败终态。
pub(crate) const STATE_FAILED: &str = "failed";
/// 被 bot_stop 取消。
pub(crate) const STATE_CANCELLED: &str = "cancelled";

/// 一次入队：成功返回 Some(id)，命中幂等键（这一行早就存在）返回 None。
///
/// 返回 None **不是错误**：它表示"这个幂等键已经排过队了"。调用方可以记一条 debug
/// 日志，但绝不能把它当成失败向上抛 —— 用户连点两次不该看到红字。
pub(crate) fn enqueue(conn: &Connection, bot_id: &BotId, job: &NewJob) -> Result<Option<JobId>> {
    // RETURNING 的空结果在这里的含义是 ON CONFLICT 什么都没写，所以用 query_row + optional。
    let inserted: Option<String> = conn
        .query_row(
            INSERT_SQL,
            params![
                job.id.to_string(),
                bot_id.to_string(),
                job.name,
                job.payload_json,
                job.idempotency_key,
                STATE_PENDING,
                i64::try_from(job.budget_ms).unwrap_or(i64::MAX),
                job.conversation,
                job.next_at,
                job.created_at,
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if inserted.is_none() {
        return Ok(None);
    }
    Ok(Some(job.id))
}

/// 取到期作业并就地改成 running（attempts 加一），返回改完的行。
///
/// 一次调用最多 limit 行；ORDER BY next_at 保证先到期的先跑
/// （同一 next_at 时用 created_at 兜底，避免两轮之间顺序抖动）。
/// limit 为 0 直接返回空表：SQL 的 LIMIT 0 会静默返回空行，调用方分不清
/// "没有到期"和"上界传错"，所以这里不发出那条语句。
pub(crate) fn claim_due(conn: &Connection, now: i64, limit: u32) -> Result<Vec<JobRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let transaction = conn.unchecked_transaction()?;
    let rows = {
        let mut statement = transaction.prepare(CLAIM_SQL)?;
        let mapped = statement.query_map(
            params![STATE_PENDING, STATE_RUNNING, now, i64::from(limit)],
            row_to_job,
        )?;
        let mut jobs = Vec::new();
        for row in mapped {
            jobs.push(row?);
        }
        jobs
    };
    transaction.commit()?;
    Ok(rows)
}

/// 按主键读一条作业；不存在返回 None。
pub(crate) fn get(conn: &Connection, id: &JobId) -> Result<Option<JobRow>> {
    Ok(conn
        .query_row(&select_all("WHERE id = ?1"), [id.to_string()], row_to_job)
        .optional()?)
}

/// 标成功：state = done、清掉 last_error；返回是否命中一行。
pub(crate) fn mark_done(conn: &Connection, id: &JobId, now: i64) -> Result<bool> {
    let changed = conn.execute(MARK_DONE_SQL, params![id.to_string(), STATE_DONE, now])?;
    Ok(changed == 1)
}

/// 安排下一次重试：回到 pending、把 next_at 换成调用方算好的时刻、记下本次错误。
///
/// attempts 不动：它在 claim_due 里已经加过了（"这一次跑过了"）。
pub(crate) fn reschedule(
    conn: &Connection,
    id: &JobId,
    next_at: i64,
    error: &str,
    now: i64,
) -> Result<bool> {
    let changed = conn.execute(
        RESCHEDULE_SQL,
        params![id.to_string(), STATE_PENDING, next_at, error, now],
    )?;
    Ok(changed == 1)
}

/// 落失败终态：state = failed 并写错误正文；返回是否命中一行。
///
/// 保留行而不是删掉：面板要能看到"哪条作业放弃了、为什么"，排障也需要原始载荷。
pub(crate) fn mark_failed(conn: &Connection, id: &JobId, error: &str, now: i64) -> Result<bool> {
    let changed = conn.execute(
        MARK_FAILED_SQL,
        params![id.to_string(), STATE_FAILED, error, now],
    )?;
    Ok(changed == 1)
}

/// 取消一个 bot 的所有未完成作业（pending / running），返回取消了几条。
///
/// bot_stop 用它：终态（done / failed / cancelled）不再动 —— 已经成功的不该被
/// 改成"取消"，那会把一条真实的成功记录抹掉。
pub(crate) fn cancel_pending_for_bot(conn: &Connection, bot_id: &BotId, now: i64) -> Result<u32> {
    let changed = conn.execute(
        CANCEL_SQL,
        params![
            bot_id.to_string(),
            STATE_CANCELLED,
            now,
            STATE_PENDING,
            STATE_RUNNING
        ],
    )?;
    Ok(u32::try_from(changed).unwrap_or(u32::MAX))
}

/// 重启恢复：把所有 running 行退回 pending（next_at = now），返回恢复了几条。
///
/// 这是"至少一次"里 running 那一半的落地：进程崩在一次执行中间时，库里只有一行
/// running，没有任何东西能证明那次执行跑完了。退回 pending = 承认"它可能跑过一半"，
/// 由作业体自己的幂等来兜住重复执行。
///
/// attempts 不动（见 reschedule）。
pub(crate) fn resume_interrupted(conn: &Connection, now: i64) -> Result<u32> {
    let changed = conn.execute(RESUME_SQL, params![STATE_PENDING, now, STATE_RUNNING])?;
    Ok(u32::try_from(changed).unwrap_or(u32::MAX))
}

/// 数一个 bot 还有多少条未完成作业（pending / running）—— 作业池上限的判据。
pub(crate) fn count_active_for_bot(conn: &Connection, bot_id: &BotId) -> Result<u32> {
    let count: i64 = conn.query_row(
        COUNT_SQL,
        params![bot_id.to_string(), STATE_PENDING, STATE_RUNNING],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// 列一个 bot 的作业行（按入队时刻升序，最多 limit 条）—— 面板与排障用。
pub(crate) fn list_for_bot(conn: &Connection, bot_id: &BotId, limit: u32) -> Result<Vec<JobRow>> {
    let mut statement = conn.prepare(&select_all(LIST_TAIL))?;
    let mapped = statement.query_map(params![bot_id.to_string(), i64::from(limit)], row_to_job)?;
    let mut jobs = Vec::new();
    for row in mapped {
        jobs.push(row?);
    }
    Ok(jobs)
}

/// SELECT 的列清单（与 row_to_job 一一对应；列的出处在 SQL 里只这一份）。
const COLUMNS: &str = "id, name, payload_json, idempotency_key, state, attempts, budget_ms, conversation, next_at, last_error, created_at";

/// 入队：成功时返回刚写进去的主键；命中幂等键时 ON CONFLICT 直接跳过（RETURNING 空结果）。
const INSERT_SQL: &str = "INSERT INTO bot_jobs (id, bot_id, name, payload_json, idempotency_key, state, attempts, budget_ms, conversation, next_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?10) ON CONFLICT (bot_id, idempotency_key) DO NOTHING RETURNING id";

/// 取到期 + 改状态 + 加 attempts，一条语句完成（见模块文档第一节）。
const CLAIM_SQL: &str = "UPDATE bot_jobs SET state = ?2, attempts = attempts + 1, updated_at = ?3 WHERE id IN (SELECT id FROM bot_jobs WHERE state = ?1 AND next_at <= ?3 ORDER BY next_at, created_at LIMIT ?4) RETURNING id, name, payload_json, idempotency_key, state, attempts, budget_ms, conversation, next_at, last_error, created_at";

/// 标成功。
const MARK_DONE_SQL: &str =
    "UPDATE bot_jobs SET state = ?2, last_error = NULL, updated_at = ?3 WHERE id = ?1";

/// 排下一次重试。
const RESCHEDULE_SQL: &str =
    "UPDATE bot_jobs SET state = ?2, next_at = ?3, last_error = ?4, updated_at = ?5 WHERE id = ?1";

/// 落失败终态。
const MARK_FAILED_SQL: &str =
    "UPDATE bot_jobs SET state = ?2, last_error = ?3, updated_at = ?4 WHERE id = ?1";

/// bot_stop 时取消未完成的作业。
const CANCEL_SQL: &str =
    "UPDATE bot_jobs SET state = ?2, updated_at = ?3 WHERE bot_id = ?1 AND state IN (?4, ?5)";

/// 重启时把 running 退回 pending。
const RESUME_SQL: &str =
    "UPDATE bot_jobs SET state = ?1, next_at = ?2, updated_at = ?2 WHERE state = ?3";

/// 数未完成作业。
const COUNT_SQL: &str = "SELECT COUNT(*) FROM bot_jobs WHERE bot_id = ?1 AND state IN (?2, ?3)";

/// list_for_bot 的尾部。
const LIST_TAIL: &str = "WHERE bot_id = ?1 ORDER BY created_at LIMIT ?2";

/// 拼一条从 COLUMNS 出发的 SELECT（尾部的 WHERE / ORDER / LIMIT 由调用方给）。
fn select_all(tail: &str) -> String {
    format!("SELECT {COLUMNS} FROM bot_jobs {tail}")
}

/// 把一行映射成 JobRow。
///
/// 主键读不成 UUID 直接报错（库坏了），attempts / budget_ms 越界按 MAX（见模块文档最后一节）。
fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    let raw_id: String = row.get(0)?;
    let id = raw_id.parse::<JobId>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(JobRow {
        id,
        name: row.get(1)?,
        payload_json: row.get(2)?,
        idempotency_key: row.get(3)?,
        state: row.get(4)?,
        attempts: u32::try_from(row.get::<_, i64>(5)?).unwrap_or(u32::MAX),
        budget_ms: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(u64::MAX),
        conversation: row.get(7)?,
        next_at: row.get(8)?,
        last_error: row.get(9)?,
        created_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::jobs;
    use crate::test_support::{
        TempDir, bot_id, bot_id_at, job_id, new_job, open_store, raw, seed_extra_bot,
    };

    /// 用裸连接直接读一行（不经过仓库层的映射，断言原样列值）。
    fn raw_row(conn: &Connection, id: &JobId) -> (String, i64, Option<String>, i64) {
        conn.query_row(
            "SELECT state, attempts, last_error, next_at FROM bot_jobs WHERE id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )
        .expect("read job row")
    }

    /// 状态常量必须与 core 的枚举拼写逐字一致 —— 这份映射是跨 crate 的隐性契约。
    ///
    /// 存储层在 SQL 里只能写字符串，core 在 serde 里写枚举；两边一旦分叉，
    /// 表现是"作业永远停在 pending"（写进去的 'Pending' 谁也认不出），
    /// 而不是一条编译错误。
    #[test]
    fn state_spellings_match_the_core_enum() {
        use hbot_core::bot_jobs::JobState;
        assert_eq!(STATE_PENDING, JobState::Pending.as_str());
        assert_eq!(STATE_RUNNING, JobState::Running.as_str());
        assert_eq!(STATE_DONE, JobState::Done.as_str());
        assert_eq!(STATE_FAILED, JobState::Failed.as_str());
        assert_eq!(STATE_CANCELLED, JobState::Cancelled.as_str());
    }

    /// 入队 → 取到期 → 标成功：全链路的最小骨架。
    #[test]
    fn enqueue_claim_and_mark_done_round_trip() {
        let dir = TempDir::new("jobs-round-trip");
        let store = open_store(&dir);
        let created = store
            .enqueue_job(&bot_id(), &new_job(7, "war-1", 1_000))
            .expect("enqueue");
        assert_eq!(created, Some(job_id(7)), "首次入队必须返回新行主键");

        let claimed = store.claim_due_jobs(1_000, 10).expect("claim");
        assert_eq!(claimed.len(), 1, "一条到期的作业该被取走");
        assert_eq!(claimed[0].id, job_id(7));
        assert_eq!(claimed[0].state, STATE_RUNNING, "取走时必须已经是 running");
        assert_eq!(claimed[0].attempts, 1, "取走时必须已经加了 attempts");
        assert_eq!(claimed[0].payload_json, "{\"warId\":\"w-1\"}");

        assert!(store.mark_job_done(&job_id(7), 2_000).expect("mark done"));
        let row = store.get_job(&job_id(7)).expect("get").expect("row exists");
        assert_eq!(row.state, STATE_DONE);
        assert_eq!(row.last_error, None, "成功后不许留下旧错误");
    }

    /// 幂等：同一个 (bot, key) 再入队一次返回 None，且库里仍然只有一行。
    ///
    /// 这是"至少一次"能落地的前提（模块文档第一节的承诺）。
    #[test]
    fn a_repeated_idempotency_key_is_a_second_no_not_an_error() {
        let dir = TempDir::new("jobs-idempotent");
        let store = open_store(&dir);
        assert_eq!(
            store
                .enqueue_job(&bot_id(), &new_job(7, "war-1", 1_000))
                .expect("first enqueue"),
            Some(job_id(7))
        );
        assert_eq!(
            store
                .enqueue_job(&bot_id(), &new_job(8, "war-1", 1_000))
                .expect("second enqueue"),
            None,
            "命中幂等键必须返回 None（不是错误，也不是新的 id）"
        );
        let rows = store.list_jobs_for_bot(&bot_id(), 100).expect("list");
        assert_eq!(rows.len(), 1, "重复入队不许长出第二行");
        assert_eq!(rows[0].id, job_id(7), "保留的是第一次那一行");

        // 同一个键换一只 bot 是另一条身份：幂等键的作用域是 (bot_id, key)。
        seed_extra_bot(&store, 99);
        assert_eq!(
            store
                .enqueue_job(&bot_id_at(99), &new_job(9, "war-1", 1_000))
                .expect("enqueue other bot"),
            Some(job_id(9)),
            "幂等键不跨 bot 去重"
        );
    }

    /// 未到期的不许被取走；排序与 limit 都要生效。
    #[test]
    fn claim_only_takes_the_due_rows_up_to_the_limit() {
        let dir = TempDir::new("jobs-claim-limit");
        let store = open_store(&dir);
        store
            .enqueue_job(&bot_id(), &new_job(7, "a", 1_000))
            .expect("enqueue a");
        store
            .enqueue_job(&bot_id(), &new_job(8, "b", 2_000))
            .expect("enqueue b");
        store
            .enqueue_job(&bot_id(), &new_job(9, "c", 3_000))
            .expect("enqueue c");

        assert!(
            store
                .claim_due_jobs(500, 10)
                .expect("claim early")
                .is_empty(),
            "一条都没到期"
        );
        let all_due = store.claim_due_jobs(3_000, 10).expect("claim all");
        assert_eq!(all_due.len(), 3, "三条都到期");
        assert_eq!(
            all_due.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![job_id(7), job_id(8), job_id(9)],
            "按 next_at 升序"
        );
        assert!(
            store
                .claim_due_jobs(3_000, 10)
                .expect("claim again")
                .is_empty(),
            "刚取走的三条已经是 running，不该被再取一次"
        );

        // limit 单独验：重新入队三条，limit = 2 只走两条。
        let dir2 = TempDir::new("jobs-limit-only");
        let store2 = open_store(&dir2);
        for (id, at) in [(7, 1_000), (8, 1_000), (9, 1_000)] {
            store2
                .enqueue_job(&bot_id(), &new_job(id, &format!("k{id}"), at))
                .expect("enqueue");
        }
        let two = store2.claim_due_jobs(1_000, 2).expect("claim limited");
        assert_eq!(two.len(), 2, "limit = 2 只能取两条");
        assert!(
            store2
                .claim_due_jobs(1_000, 0)
                .expect("claim zero")
                .is_empty(),
            "limit = 0 必须空表"
        );
    }

    /// 重试：回 pending、next_at 改到重排时刻、记下错误，attempts 不加。
    #[test]
    fn reschedule_returns_to_pending_without_bumping_attempts() {
        let dir = TempDir::new("jobs-reschedule");
        let store = open_store(&dir);
        store
            .enqueue_job(&bot_id(), &new_job(7, "k", 1_000))
            .expect("enqueue");
        let claimed = store.claim_due_jobs(1_000, 10).expect("claim");
        assert_eq!(claimed[0].attempts, 1);

        assert!(
            store
                .reschedule_job(&job_id(7), 5_000, "boom", 2_000)
                .expect("reschedule")
        );
        let row = store.get_job(&job_id(7)).expect("get").expect("row");
        assert_eq!(row.state, STATE_PENDING, "重试是回 pending，不是新状态");
        assert_eq!(
            row.attempts, 1,
            "reschedule 不该再加 attempts（claim 已经加过）"
        );
        assert_eq!(row.next_at, 5_000);
        assert_eq!(row.last_error.as_deref(), Some("boom"));
        assert!(
            store
                .claim_due_jobs(4_000, 10)
                .expect("claim early")
                .is_empty(),
            "next_at 之前不该被取走"
        );
        assert_eq!(
            store
                .claim_due_jobs(5_000, 10)
                .expect("claim on time")
                .len(),
            1,
            "到 next_at 那一刻就该被取走"
        );
    }

    /// 失败终态：落 failed、写错误、行保留（不删）。
    #[test]
    fn a_failed_job_is_kept_not_deleted() {
        let dir = TempDir::new("jobs-failed");
        let store = open_store(&dir);
        store
            .enqueue_job(&bot_id(), &new_job(7, "k", 1_000))
            .expect("enqueue");
        store.claim_due_jobs(1_000, 10).expect("claim");
        assert!(
            store
                .mark_job_failed(&job_id(7), "budget exceeded", 2_000)
                .expect("mark failed")
        );
        let row = store.get_job(&job_id(7)).expect("get").expect("row");
        assert_eq!(row.state, STATE_FAILED);
        assert_eq!(row.last_error.as_deref(), Some("budget exceeded"));
        assert_eq!(row.attempts, 1);
    }

    /// bot_stop：未完成的全落 cancelled，终态的不许被改写。
    ///
    /// "已经成功的行被改成取消" 会把一条真实的成功记录抹掉，所以这条要单独钉。
    #[test]
    fn cancel_touches_only_the_unfinished_jobs() {
        let dir = TempDir::new("jobs-cancel");
        let store = open_store(&dir);
        for (id, key) in [(7, "a"), (8, "b"), (9, "c")] {
            store
                .enqueue_job(&bot_id(), &new_job(id, key, 1_000))
                .expect("enqueue");
        }
        store.claim_due_jobs(1_000, 1).expect("claim one");
        store.mark_job_done(&job_id(7), 1_500).expect("mark done");

        assert_eq!(
            store.cancel_jobs_for_bot(&bot_id(), 3_000).expect("cancel"),
            2,
            "pending + running = 2 条"
        );
        assert_eq!(
            store
                .get_job(&job_id(7))
                .expect("get 7")
                .expect("row")
                .state,
            STATE_DONE,
            "已经成功的行不许被改成 cancelled"
        );
        assert_eq!(
            store
                .get_job(&job_id(8))
                .expect("get 8")
                .expect("row")
                .state,
            STATE_CANCELLED
        );
        // 8 号是被置为 running 的那一条（claim 只取一条）；9 号还是 pending。
        assert_eq!(
            store
                .get_job(&job_id(9))
                .expect("get 9")
                .expect("row")
                .state,
            STATE_CANCELLED
        );

        // 另一只 bot 的作业不受影响。
        let other = seed_extra_bot(&store, 99);
        store
            .enqueue_job(&other, &new_job(10, "d", 1_000))
            .expect("enqueue other");
        assert_eq!(
            store
                .get_job(&job_id(10))
                .expect("get 10")
                .expect("row")
                .state,
            STATE_PENDING,
            "取消一只 bot 的作业不许碰另一只"
        );
    }

    /// 重启恢复：running 退回 pending（next_at 拉到 now），attempts 不动。
    #[test]
    fn resume_turns_interrupted_running_rows_back_to_pending() {
        let dir = TempDir::new("jobs-resume");
        let store = open_store(&dir);
        store
            .enqueue_job(&bot_id(), &new_job(7, "k", 1_000))
            .expect("enqueue");
        store.claim_due_jobs(1_000, 10).expect("claim");
        drop(store);

        let conn = raw(&dir);
        assert_eq!(raw_row(&conn, &job_id(7)).0, STATE_RUNNING);
        assert_eq!(jobs::resume_interrupted(&conn, 9_000).expect("resume"), 1);
        let (state, attempts, _, next_at) = raw_row(&conn, &job_id(7));
        assert_eq!(state, STATE_PENDING, "running 必须退回 pending");
        assert_eq!(attempts, 1, "attempts 不动（那一次确实跑过一半）");
        assert_eq!(next_at, 9_000, "next_at 拉到 now，进程一起来就该跑");
    }

    /// 作业池上限的判据：只数未完成的行。
    #[test]
    fn active_count_ignores_the_terminal_rows() {
        let dir = TempDir::new("jobs-count");
        let store = open_store(&dir);
        for (id, key) in [(7, "a"), (8, "b"), (9, "c")] {
            store
                .enqueue_job(&bot_id(), &new_job(id, key, 1_000))
                .expect("enqueue");
        }
        assert_eq!(
            store.count_active_jobs_for_bot(&bot_id()).expect("count"),
            3
        );
        store.claim_due_jobs(1_000, 1).expect("claim");
        store.mark_job_done(&job_id(7), 1_500).expect("done");
        assert_eq!(
            store
                .count_active_jobs_for_bot(&bot_id())
                .expect("count again"),
            2,
            "终态的行不占池位"
        );
        assert_eq!(
            store
                .count_active_jobs_for_bot(&bot_id_at(99))
                .expect("count other"),
            0
        );
    }

    /// 外键：bot 行删掉时作业行跟着走（ON DELETE CASCADE）。
    #[test]
    fn jobs_are_cascaded_when_the_bot_row_goes_away() {
        let dir = TempDir::new("jobs-cascade");
        let store = open_store(&dir);
        store
            .enqueue_job(&bot_id(), &new_job(7, "a", 1_000))
            .expect("enqueue");
        drop(store);
        let conn = raw(&dir);
        conn.execute("DELETE FROM bots WHERE id = ?1", [bot_id().to_string()])
            .expect("delete bot");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM bot_jobs", [], |row| row.get(0))
            .expect("count");
        assert_eq!(left, 0, "bot 行删掉后作业行必须级联消失");
    }

    /// 迁移 0006 真的应用了：`schema_version` 到 6，表也查得到。
    #[test]
    fn the_bot_jobs_migration_is_applied() {
        let dir = TempDir::new("jobs-migration");
        let store = open_store(&dir);
        assert_eq!(store.schema_version().expect("schema version"), 6);
        drop(store);
        let conn = raw(&dir);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM bot_jobs", [], |row| row.get(0))
            .expect("bot_jobs table exists");
        assert_eq!(count, 0, "新表是空的");
    }
}
