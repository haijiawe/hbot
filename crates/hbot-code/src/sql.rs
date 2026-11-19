//! SQL 能力（`C2.sql`）在沙箱里的宿主侧装配：`ctx.sql.query` / `ctx.sql.exec`。
//!
//! 契约类型、错误码与语句闸都在 `hbot_core::bot_sql`（已冻结），本模块只管三件事：
//!
//! 1. [`SqlSlot`] —— `Sandbox` 上的可替换槽位（与 `StateSlot` / `JobSlot` 同一写法：出生为空，
//!    装配方在 isolate 起来之后按清单写进去）；
//! 2. **声明闸** —— 清单 `capabilities.state.sql` 没开就报 `HBOT_SQL_NOT_DECLARED`；开了但宿主没
//!    端口就报 `HBOT_SQL_UNAVAILABLE`（两个码分开：改清单 vs 去装配）；
//! 3. **端口** —— [`SqlBackend`]：真正跑 SQL 的是宿主（`hbot-store` 的 per-bot `ATTACH` 库）。
//!
//! # 顺序：先过语句闸，再判装配
//!
//! `normalize` + `validate` 都在**这一侧**先跑一遍：一条 `CREATE TABLE` 不该走到端口才被拒
//! （那时它已经占了一次 `spawn_blocking`），而「参数个数不对」更不该变成「宿主没装配」。
//!
//! # 为什么端口是两个方法而不是一个
//!
//! `query` 与 `exec` 返回的东西不同（行 vs 受影响条数），混成一个让调用方拿 `SqlRows` 猜
//! 「这是结果还是影响数」的返回值，是那种「看起来能用、读起来要想三秒」的设计。

use std::sync::{Arc, Mutex, PoisonError};

use hbot_core::{BotId, SqlChange, SqlError, SqlRows, SqlStatement, SqlValue};

/// 宿主没装配 SQL 后端时的说明（`HBOT_SQL_UNAVAILABLE` 前缀由 [`unavailable`] 加）。
pub(crate) const UNASSEMBLED: &str = "宿主还没有可用的 SQL 后端（bot 未启动或存储未就绪）";

/// 真正跑 SQL 的端口。
///
/// 实现方是 `hbot-store`：它按 bot 维护一个 `ATTACH` 的库文件，并把 SQL 里的表名改写成
/// `bot_<id>_<table>`。沙箱这边**不认识**表名前缀 —— 那是实现的事，调用方写裸表名。
pub trait SqlBackend: Send + Sync {
    /// 跑一条已经过闸的读语句。
    fn query(
        &self,
        bot_id: &BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlRows, SqlError>;

    /// 跑一条已经过闸的写语句。
    fn exec(
        &self,
        bot_id: &BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlChange, SqlError>;
}

/// 槽位内部状态：声明开关 + 可替换的端口 + 本事件的 bot 身份。
#[derive(Default)]
struct SqlSlotState {
    declared: bool,
    backend: Option<Arc<dyn SqlBackend>>,
    /// 本事件属于哪个 bot；没有它就没有 schema 可用。
    bot: Option<BotId>,
}

/// SQL 槽位（一个 isolate 一个）。
pub(crate) struct SqlSlot {
    state: Mutex<SqlSlotState>,
}

impl SqlSlot {
    /// 空槽位：没声明、没端口、没身份。
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(SqlSlotState::default()),
        }
    }

    /// 装声明开关（`hbot.json` 的 `capabilities.state.sql`）。
    pub(crate) fn set_declared(&self, declared: bool) {
        self.lock().declared = declared;
    }

    /// 装入（或替换）后端。
    pub(crate) fn set_backend(&self, backend: Arc<dyn SqlBackend>) {
        self.lock().backend = Some(backend);
    }

    /// 是否已经装配了后端。
    #[must_use]
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().backend.is_some()
    }

    /// 清单是否声明了 SQL 能力。
    #[must_use]
    pub(crate) fn is_declared(&self) -> bool {
        self.lock().declared
    }

    /// 记下本事件的 bot 身份。**每次整体覆盖**（包括覆盖成 `None`）。
    pub(crate) fn set_current_bot(&self, bot: Option<BotId>) {
        self.lock().bot = bot;
    }

    /// 一次 `ctx.sql.query`：语句闸 -> 声明闸 -> 身份 -> 装配闸 -> 后端。
    pub(crate) fn query(&self, sql: &str, params: Vec<SqlValue>) -> Result<SqlRows, SqlError> {
        let statement = hbot_core::normalize_sql(sql)?;
        hbot_core::validate_sql_query(&statement, &params)?;
        let (backend, bot) = self.gate()?;
        backend.query(&bot, &statement, &params)
    }

    /// 一次 `ctx.sql.exec`：语句闸 -> 声明闸 -> 身份 -> 装配闸 -> 后端。
    pub(crate) fn exec(&self, sql: &str, params: Vec<SqlValue>) -> Result<SqlChange, SqlError> {
        let statement = hbot_core::normalize_sql(sql)?;
        hbot_core::validate_sql(&statement, &params)?;
        let (backend, bot) = self.gate()?;
        backend.exec(&bot, &statement, &params)
    }

    /// 两道闸 + 身份检查（`query` / `exec` 共用）。
    fn gate(&self) -> Result<(Arc<dyn SqlBackend>, BotId), SqlError> {
        let (declared, backend, bot) = {
            let state = self.lock();
            (state.declared, state.backend.clone(), state.bot)
        };
        if !declared {
            return Err(SqlError::NotDeclared { capability: "sql" });
        }
        let Some(bot) = bot else {
            return Err(SqlError::Invalid {
                message: String::from(
                    "ctx.sql 需要 bot 身份：定时器 / 作业 / 生命周期事件里没有它，不能在那些事件里跑 SQL",
                ),
            });
        };
        let Some(backend) = backend else {
            return Err(SqlError::Unavailable {
                reason: unavailable(UNASSEMBLED),
            });
        };
        Ok((backend, bot))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SqlSlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for SqlSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqlSlot")
            .field("declared", &self.is_declared())
            .field("assembled", &self.is_assembled())
            .finish()
    }
}

/// 没装配时的错误正文（以 `HBOT_SQL_UNAVAILABLE` 开头）。
#[must_use]
pub fn unavailable(detail: &str) -> String {
    SqlError::Unavailable {
        reason: detail.to_owned(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hbot_core::{BotId, SqlChange, SqlError, SqlRows, SqlStatement, SqlValue};

    use super::{SqlBackend, SqlSlot, unavailable};

    /// 记下后端收到的语句；可预置失败。
    #[derive(Default)]
    struct Recording {
        queries: Mutex<Vec<(String, Vec<SqlValue>)>>,
        execs: Mutex<Vec<(String, Vec<SqlValue>)>>,
        failure: Mutex<Option<SqlError>>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
    }

    impl SqlBackend for Recording {
        fn query(
            &self,
            _bot_id: &BotId,
            statement: &SqlStatement,
            params: &[SqlValue],
        ) -> Result<SqlRows, SqlError> {
            if let Some(error) = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return Err(error);
            }
            self.queries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((statement.text.clone(), params.to_vec()));
            Ok(SqlRows::empty())
        }

        fn exec(
            &self,
            _bot_id: &BotId,
            statement: &SqlStatement,
            params: &[SqlValue],
        ) -> Result<SqlChange, SqlError> {
            if let Some(error) = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return Err(error);
            }
            self.execs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((statement.text.clone(), params.to_vec()));
            Ok(SqlChange { affected: 1 })
        }
    }

    fn bot() -> BotId {
        "00000000-0000-0000-0000-000000000b07"
            .parse::<BotId>()
            .expect("bot uuid")
    }

    /// 一个声明了、装好后端、也有身份的槽位。
    fn assembled() -> (SqlSlot, Arc<Recording>) {
        let slot = SqlSlot::new();
        let backend = Recording::new();
        slot.set_declared(true);
        slot.set_backend(backend.clone());
        slot.set_current_bot(Some(bot()));
        (slot, backend)
    }

    #[test]
    fn an_empty_slot_reports_not_declared() {
        let slot = SqlSlot::new();
        assert!(!slot.is_declared());
        assert_eq!(
            slot.query("SELECT 1", vec![]).unwrap_err().code(),
            "HBOT_SQL_NOT_DECLARED"
        );
    }

    #[test]
    fn a_declared_but_unassembled_slot_reports_unavailable() {
        let slot = SqlSlot::new();
        slot.set_declared(true);
        slot.set_current_bot(Some(bot()));
        assert_eq!(
            slot.query("SELECT 1", vec![]).unwrap_err().code(),
            "HBOT_SQL_UNAVAILABLE"
        );
    }

    #[test]
    fn the_statement_gate_runs_before_the_assembly_gate() {
        // 空槽位 + DDL：必须报 DENIED 而不是 NOT_DECLARED —— 语句本身就不许跑。
        let slot = SqlSlot::new();
        let error = slot.exec("DROP TABLE users", vec![]).unwrap_err();
        assert_eq!(error.code(), "HBOT_SQL_DENIED");
    }

    #[test]
    fn a_missing_bot_identity_is_invalid() {
        let (slot, _backend) = assembled();
        slot.set_current_bot(None);
        let error = slot.query("SELECT 1", vec![]).unwrap_err();
        assert_eq!(error.code(), "HBOT_SQL_INVALID");
        assert!(error.to_string().contains("bot 身份"), "{error}");
    }

    #[test]
    fn a_valid_query_reaches_the_backend_verbatim() {
        let (slot, backend) = assembled();
        slot.query(
            "SELECT * FROM users WHERE id = ?",
            vec![SqlValue::Integer(7)],
        )
        .expect("合法查询");
        let taken = backend.queries.lock().unwrap().clone();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, "SELECT * FROM users WHERE id = ?");
        assert_eq!(taken[0].1, vec![SqlValue::Integer(7)]);
    }

    #[test]
    fn a_write_through_query_is_denied_before_the_backend() {
        let (slot, backend) = assembled();
        let error = slot
            .exec(
                "INSERT INTO users (id) VALUES (?)",
                vec![SqlValue::Integer(1)],
            )
            .expect("exec 允许写");
        assert_eq!(error.affected, 1);
        let error = slot.query("DELETE FROM users", vec![]).unwrap_err();
        assert_eq!(error.code(), "HBOT_SQL_DENIED");
        assert!(
            backend.queries.lock().unwrap().is_empty(),
            "被拒的语句不许到后端"
        );
    }

    #[test]
    fn a_backend_failure_surfaces_unchanged() {
        let (slot, backend) = assembled();
        *backend.failure.lock().unwrap() = Some(SqlError::Failed {
            message: "constraint failed".to_owned(),
        });
        let error = slot.query("SELECT 1", vec![]).unwrap_err();
        assert_eq!(error.code(), "HBOT_SQL_FAILED");
        assert!(error.to_string().contains("constraint failed"), "{error}");
    }

    #[test]
    fn clearing_the_identity_does_not_inherit_the_previous_bot() {
        let (slot, _backend) = assembled();
        slot.set_current_bot(None);
        assert_eq!(
            slot.query("SELECT 1", vec![]).unwrap_err().code(),
            "HBOT_SQL_INVALID"
        );
    }

    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        let message = unavailable("宿主还没有可用的 SQL 后端");
        assert!(message.starts_with("HBOT_SQL_UNAVAILABLE"));
    }
}
