//! `bot_kv` —— bot 侧键值存储（C2 的 kv 能力，`crates/hbot-core/src/bot_state.rs`）。
//!
//! 存储层只做四件事：按主键读写、按前缀列出未过期的键、一次事务里批量写、按主键删。
//! **不认识值的语义**（`value_json` 是原样字符串），也**不读时钟** —— `now` 一律由调用方
//! 传入，过期判定才可注入、可重放。
//!
//! ## 过期是读语义，不是删除动作（硬规则）
//!
//! `get` / `list` 把 `expires_at IS NOT NULL AND expires_at <= now` 的行当作不存在，但
//! **不顺手 `DELETE`**。理由有两条，缺一条都会出问题：
//!
//! * 读路径要能跑在只读连接上（`Store::read`）；顺手删会让一次读变成一次写，
//!   在只读连接上直接失败；
//! * 就算跑在写连接上，删也会和单 writer 线程抢锁、把本来亚毫秒的读变成一次写事务。
//!
//! 物理清理是另一件事：将来加一条 `DELETE FROM bot_kv WHERE expires_at <= ?1` 的
//! 维护语句即可，不需要改 schema，也不需要改这里的读语义。
//!
//! ## 为什么 `set` 必须能"把到期时间改回永不过期"
//!
//! `ON CONFLICT … DO UPDATE` 里显式写 `expires_at = excluded.expires_at`。用
//! `COALESCE(excluded.expires_at, expires_at)` 这种"记住旧值"的写法是**错的**：
//! 调用方把 TTL 改成 `None`（永不过期）时，旧的非 NULL 到期时间会留下来，键会在
//! 某个时刻静默消失。测试
//! `resetting_a_key_to_never_expire_really_clears_expires_at` 钉住这一条。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::ids::BotId;
use crate::models::NewKvEntry;

/// 读一个键的值；不存在或**已过期**都返回 `None`。
///
/// 只返回未过期的行：`expires_at IS NULL`（永不过期）或 `expires_at > now`。
/// 边界取严格大于 —— `expires_at == now` 视为已到期（"到期那一刻起就不可见"）。
///
/// 已过期的行**不删**（见模块文档）：本函数是纯读，可以跑在只读连接上。
pub(crate) fn get(
    conn: &Connection,
    bot_id: &BotId,
    key: &str,
    now: i64,
) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value_json FROM bot_kv
             WHERE bot_id = ?1 AND key = ?2
               AND (expires_at IS NULL OR expires_at > ?3)",
            params![bot_id.to_string(), key, now],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(value)
}

/// 写一个键（存在则覆盖），`updated_at` 记这次写入的 `now`。
///
/// 用 upsert 而不是"先查再决定 insert/update"：后者在并发下会在两条语句之间留下窗口，
/// 且多一次往返。`expires_at` 原样覆盖，所以 `None` 能把旧的到期时间**清成 NULL**。
pub(crate) fn set(
    conn: &Connection,
    bot_id: &BotId,
    key: &str,
    value_json: &str,
    expires_at: Option<i64>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO bot_kv (bot_id, key, value_json, updated_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (bot_id, key) DO UPDATE SET
             value_json = excluded.value_json,
             updated_at = excluded.updated_at,
             expires_at = excluded.expires_at",
        params![bot_id.to_string(), key, value_json, now, expires_at],
    )?;
    Ok(())
}

/// 删一个键，返回是否真的删掉了行（`false` = 本来就不存在）。
///
/// 已过期的行**也算删掉了**（`true`）：本函数是写，按主键删，不看 `expires_at`。
/// 调用方拿它做"我有没有删掉东西"的判断，而不是"有没有未过期的键"。
pub(crate) fn delete(conn: &Connection, bot_id: &BotId, key: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM bot_kv WHERE bot_id = ?1 AND key = ?2",
        params![bot_id.to_string(), key],
    )?;
    Ok(changed > 0)
}

/// 按 `key` 升序列出未过期的键名，最多 `limit` 条；`prefix` 是可选前缀。
///
/// ## 前缀必须转义 LIKE 的通配符
///
/// `LIKE` 里 `%` 与 `_` 是通配符。不转义的话，调用方传 `a_b` 会把 `axb` 也匹配进来 ——
/// 这是一个**静默的错误结果**（键名是用户数据，`_` 在层级前缀里很常见）。
/// 所以模式串由 [`like_prefix_pattern`] 生成（`\`、`%`、`_` 前面都加 `\`），
/// SQL 里配 `ESCAPE '\'`。
///
/// 前缀为空串时等价于"全部键"（`LIKE '%'`）—— 与 `Option::None` 的差别只是多一次
/// 恒真的 LIKE。排序用 `ORDER BY key`：主键 `(bot_id, key)` 的前导列已经定住 bot，
/// 剩下的顺序天然按 `key` 排，不需要额外索引或临时排序。
pub(crate) fn list(
    conn: &Connection,
    bot_id: &BotId,
    prefix: Option<&str>,
    limit: u32,
    now: i64,
) -> Result<Vec<String>> {
    let pattern = prefix.map(like_prefix_pattern);
    let mut statement = conn.prepare(
        "SELECT key FROM bot_kv
         WHERE bot_id = ?1
           AND (expires_at IS NULL OR expires_at > ?3)
           AND (?2 IS NULL OR key LIKE ?2 ESCAPE '\\')
         ORDER BY key
         LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![bot_id.to_string(), pattern, now, i64::from(limit)],
        |row| row.get::<_, String>(0),
    )?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row?);
    }
    Ok(keys)
}

/// 一次事务里逐条 upsert，返回 `()` 表示**全部**落库（任何一条失败整批回滚）。
///
/// ## 为什么必须是"一次事务"
///
/// 生成代码最常见的一段是"读计数器 → 加一 → 写回"。如果批量写不是原子的，进程在中途
/// 被杀就会留下写了一半的状态，而且这种坏状态**没有任何报错**，只会在几天后表现为
/// "数据对不上"（`crates/hbot-core/src/bot_state.rs` 模块头）。
///
/// 用 [`Connection::unchecked_transaction`] 而不是 `Connection::transaction`：db 线程拿到的
/// 是 `Connection`（不是 `&mut Connection`），`transaction()` 需要 `&mut self` 因而借不到。
/// `unchecked_transaction` 正是为这种"已经在独占写连接上、编译器证明不了独占"的场景提供
/// 的入口 —— 它仍然开一条**真事务**，失败时 `Transaction` 在 `Drop` 里回滚。
///
/// 同批出现重复键时**后者覆盖前者**：循环里按数组顺序逐条 upsert，后一条覆盖前一条
/// （与调用方看到"最后写的赢"一致）。
///
/// 空切片是合法输入：不报错、不写、也不开事务。
pub(crate) fn set_many(
    conn: &Connection,
    bot_id: &BotId,
    entries: &[NewKvEntry],
    now: i64,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    for entry in entries {
        // 失败直接 `?` 返回：`transaction` 在此处被 `Drop`，默认行为是 ROLLBACK。
        set(
            &transaction,
            bot_id,
            &entry.key,
            &entry.value_json,
            entry.expires_at,
            now,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// 把调用方给的前缀变成 LIKE 模式串：转义 `\`、`%`、`_`，再补一个尾部 `%`。
///
/// 只转义这三个字符是够的：SQLite 的 LIKE 默认没有别的元字符（`ESCAPE '\'` 显式指定了
/// 转义符，不依赖默认值）。`\` 本身必须转义，否则前缀 `a\` 会吃掉后一个字符的转义。
fn like_prefix_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for character in prefix.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::repositories::kv;
    use crate::test_support::{TempDir, bot_id, bot_id_at, open_store, raw};

    /// 写一条 KV（`expires_at` / `now` 由调用方给）。
    fn set_entry(store: &Store, key: &str, value: &str, expires_at: Option<i64>, now: i64) {
        store
            .set_kv(&bot_id(), key, value, expires_at, now)
            .unwrap_or_else(|error| panic!("set_kv({key}) failed: {error}"));
    }

    /// 造一条待写入的 KV 批次元素。
    fn entry(key: &str, value: &str, expires_at: Option<i64>) -> NewKvEntry {
        NewKvEntry {
            key: key.to_owned(),
            value_json: value.to_owned(),
            expires_at,
        }
    }

    /// 钉住最基本的读写语义：没写过 → `None`；写过读得到；覆盖后是新值；
    /// `delete` 第一次 `true`、第二次 `false`。走 `Store`（= 写线程 + 只读池两条路径都验）。
    #[test]
    fn get_set_delete_round_trip() {
        let dir = TempDir::new("kv-round-trip");
        let store = open_store(&dir);

        assert_eq!(
            store
                .get_kv(&bot_id(), "missing", 1_000)
                .expect("get empty"),
            None,
            "没写过的键必须是 None"
        );

        set_entry(&store, "counter", "1", None, 1_000);
        assert_eq!(
            store.get_kv(&bot_id(), "counter", 1_000).expect("get 1"),
            Some("1".to_owned())
        );

        set_entry(&store, "counter", "2", None, 1_100);
        assert_eq!(
            store.get_kv(&bot_id(), "counter", 1_200).expect("get 2"),
            Some("2".to_owned()),
            "重设必须覆盖旧值"
        );

        assert!(
            store.delete_kv(&bot_id(), "counter").expect("delete hit"),
            "删已存在的键返回 true"
        );
        assert!(
            !store.delete_kv(&bot_id(), "counter").expect("delete miss"),
            "删不存在的键返回 false"
        );
        assert_eq!(
            store
                .get_kv(&bot_id(), "counter", 1_300)
                .expect("get after delete"),
            None
        );
    }

    /// TTL：到期的行 `get` 不到、`list` 不出现；未到期的拿得到；
    /// **重设成 `None` 之后到期时间真的被清成 NULL**（最容易写错的一条）。
    #[test]
    fn expired_rows_are_invisible_and_can_be_reset_to_never_expire() {
        let dir = TempDir::new("kv-ttl");
        let store = open_store(&dir);
        let now = 10_000;

        // 已经过期：expires_at = now - 1。
        set_entry(&store, "stale", "\"old\"", Some(now - 1), now - 100);
        // 刚到期：expires_at == now 也算过期（严格大于才活着）。
        set_entry(&store, "edge", "\"edge\"", Some(now), now - 100);
        // 还没到期。
        set_entry(&store, "fresh", "\"fresh\"", Some(now + 1_000), now);

        assert_eq!(
            store.get_kv(&bot_id(), "stale", now).expect("get stale"),
            None
        );
        assert_eq!(
            store.get_kv(&bot_id(), "edge", now).expect("get edge"),
            None
        );
        assert_eq!(
            store.get_kv(&bot_id(), "fresh", now).expect("get fresh"),
            Some("\"fresh\"".to_owned())
        );

        let keys = store
            .list_kv(&bot_id(), None, 100, now)
            .expect("list live keys");
        assert_eq!(keys, vec!["fresh".to_owned()], "list 只返回未过期的键");

        // 过期行不能被"顺手删掉"——读是纯读，行还在库里。
        drop(store);
        let conn = raw(&dir);
        let stale_json: Option<String> = conn
            .query_row(
                "SELECT value_json FROM bot_kv WHERE key = 'stale'",
                [],
                |row| row.get(0),
            )
            .expect("expired row is still stored");
        assert_eq!(
            stale_json.as_deref(),
            Some("\"old\""),
            "过期只是读语义：行必须还在库里（读不能变成写）"
        );
        let expired_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bot_kv WHERE expires_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count ttl rows");
        assert_eq!(
            expired_count, 3,
            "三条带 TTL 的行（含未到期的 fresh）都还在"
        );

        // 重设成 None：到期时间必须真的清掉，而不是保留旧的（保留 = 键会静默消失）。
        drop(conn);
        let store = Store::open(dir.db_path()).expect("reopen store");
        set_entry(&store, "stale", "\"renewed\"", None, now + 1);
        assert_eq!(
            store.get_kv(&bot_id(), "stale", now).expect("get renewed"),
            Some("\"renewed\"".to_owned()),
            "重设成永不过期之后必须立刻读得到"
        );
        let keys = store
            .list_kv(&bot_id(), None, 100, now)
            .expect("list again");
        assert_eq!(keys, vec!["fresh".to_owned(), "stale".to_owned()]);

        drop(store);
        let conn = raw(&dir);
        let expires_at: Option<i64> = conn
            .query_row(
                "SELECT expires_at FROM bot_kv WHERE key = 'stale'",
                [],
                |row| row.get(0),
            )
            .expect("read expires_at");
        assert_eq!(expires_at, None, "expires_at 必须被清成 NULL");
    }

    /// list：按 key 升序、受 limit 限制、prefix 生效。
    #[test]
    fn list_is_sorted_limited_and_prefix_filtered() {
        let dir = TempDir::new("kv-list");
        let store = open_store(&dir);
        for key in ["user:1:step", "user:2:step", "admin:1", "counter"] {
            set_entry(&store, key, "0", None, 1_000);
        }

        let all = store
            .list_kv(&bot_id(), None, 100, 1_000)
            .expect("list all");
        assert_eq!(
            all,
            vec![
                "admin:1".to_owned(),
                "counter".to_owned(),
                "user:1:step".to_owned(),
                "user:2:step".to_owned(),
            ],
            "必须按 key 升序"
        );

        let limited = store
            .list_kv(&bot_id(), None, 2, 1_000)
            .expect("list limited");
        assert_eq!(limited.len(), 2, "limit 是条数上限");
        assert_eq!(limited, vec!["admin:1".to_owned(), "counter".to_owned()]);

        let prefixed = store
            .list_kv(&bot_id(), Some("user:"), 100, 1_000)
            .expect("list by prefix");
        assert_eq!(
            prefixed,
            vec!["user:1:step".to_owned(), "user:2:step".to_owned()]
        );

        assert!(
            store
                .list_kv(&bot_id(), Some("nobody:"), 100, 1_000)
                .expect("list unknown prefix")
                .is_empty(),
            "没有匹配前缀时是空表"
        );
        assert!(
            store
                .list_kv(&bot_id(), None, 0, 1_000)
                .expect("list zero limit")
                .is_empty(),
            "limit = 0 是空表"
        );
    }

    /// prefix 里的 `_` / `%` / `\` 必须被转义：`a_b` 不许匹配 `axb`，`a%b` 不许匹配 `azb`。
    ///
    /// 这是 LIKE 通配符的经典静默错误：不转义时结果"看起来对"，只是多了几行。
    #[test]
    fn list_escapes_like_wildcards_in_the_prefix() {
        let dir = TempDir::new("kv-like-escape");
        let store = open_store(&dir);
        for key in ["a_b", "axb", "a%b", "azb", "a\\b", "aXb"] {
            set_entry(&store, key, "0", None, 1_000);
        }

        let underscore = store
            .list_kv(&bot_id(), Some("a_"), 100, 1_000)
            .expect("list underscore prefix");
        assert_eq!(
            underscore,
            vec!["a_b".to_owned()],
            "`a_` 只该匹配字面量 a_，不能把 axb 也带进来"
        );

        let percent = store
            .list_kv(&bot_id(), Some("a%b"), 100, 1_000)
            .expect("list percent prefix");
        assert_eq!(percent, vec!["a%b".to_owned()], "`%` 必须转义");

        let backslash = store
            .list_kv(&bot_id(), Some("a\\"), 100, 1_000)
            .expect("list backslash prefix");
        assert_eq!(backslash, vec!["a\\b".to_owned()], "`\\` 必须转义");

        // 对照：不带通配符的前缀仍然是"按字面量前缀"匹配（这里是 a + 任意）。
        let broad = store
            .list_kv(&bot_id(), Some("a"), 100, 1_000)
            .expect("list broad prefix");
        assert_eq!(broad.len(), 6, "`a` 前缀匹配全部 6 个键");
    }

    /// `set_many` 的原子性：先写两条合法行（都在），再让一条**必定失败**的批次整批回滚。
    ///
    /// 失败是靠临时触发器造的（`RAISE(ABORT)`）：API 只给一个 `bot_id`，没法在批里换一个
    /// 不存在的外键；同理也没法让 `value_json` 违反 NOT NULL（它是 `&str`）。触发器是唯一
    /// 能真造出"第二条语句失败"的本地手段，用完立刻 DROP。
    #[test]
    fn set_many_is_atomic_and_rolls_back_the_whole_batch() {
        let dir = TempDir::new("kv-set-many-atomic");
        let store = open_store(&dir);
        set_entry(&store, "untouched", "\"1\"", None, 1_000);
        drop(store);

        // 裸连接：要在连接上挂触发器，且断言"整批没落库"也用它。
        let conn = raw(&dir);

        // ① 两条合法行必须都在。
        kv::set_many(
            &conn,
            &bot_id(),
            &[entry("a", "1", None), entry("b", "2", None)],
            2_000,
        )
        .expect("valid batch");
        assert_eq!(count_keys(&conn), 3, "两条合法行都应该落库（含 untouched）");

        // ② 挂一个只对 key = 'boom' 生效的触发器，让第二条语句在事务中途失败。
        conn.execute_batch(
            "CREATE TRIGGER kv_test_reject_boom BEFORE INSERT ON bot_kv
             WHEN NEW.key = 'boom'
             BEGIN
                 SELECT RAISE(ABORT, 'boom rejected by test trigger');
             END;",
        )
        .expect("create test trigger");

        let outcome = kv::set_many(
            &conn,
            &bot_id(),
            &[
                entry("c", "3", None),
                entry("boom", "4", None),
                entry("d", "5", None),
            ],
            3_000,
        );
        assert!(
            outcome.is_err(),
            "第二条违反触发器约束时整批必须报错：{outcome:?}"
        );
        assert_eq!(
            count_keys(&conn),
            3,
            "失败批次的第一条（c）也必须回滚，库里不能多出行"
        );
        for key in ["c", "boom", "d"] {
            let found: Option<String> = conn
                .query_row("SELECT key FROM bot_kv WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .optional()
                .expect("query rolled back key");
            assert_eq!(found, None, "回滚后不该存在键 {key}");
        }

        // ③ 触发器删掉后，同样的批次必须成功（证明失败来自触发器，不是别的原因）。
        conn.execute_batch("DROP TRIGGER kv_test_reject_boom")
            .expect("drop test trigger");
        kv::set_many(
            &conn,
            &bot_id(),
            &[entry("c", "3", None), entry("boom", "4", None)],
            3_000,
        )
        .expect("batch after dropping trigger");
        assert_eq!(count_keys(&conn), 5, "删掉触发器后两条都该落库");
    }

    /// 同批重复键：后者覆盖前者（数组顺序即写入顺序）。
    #[test]
    fn set_many_lets_the_last_duplicate_key_win() {
        let dir = TempDir::new("kv-set-many-duplicate");
        let store = open_store(&dir);

        store
            .set_kv_many(
                &bot_id(),
                &[
                    entry("dup", "\"first\"", Some(9_000)),
                    entry("other", "1", None),
                    entry("dup", "\"last\"", None),
                ],
                1_000,
            )
            .expect("batch with duplicate key");

        assert_eq!(
            store.get_kv(&bot_id(), "dup", 1_000).expect("get dup"),
            Some("\"last\"".to_owned()),
            "同批重复键必须后者覆盖前者"
        );

        drop(store);
        let conn = raw(&dir);
        let expires_at: Option<i64> = conn
            .query_row(
                "SELECT expires_at FROM bot_kv WHERE key = 'dup'",
                [],
                |row| row.get(0),
            )
            .expect("read dup expires_at");
        assert_eq!(expires_at, None, "后一条的 expires_at 覆盖前一条");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM bot_kv", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(rows, 2, "重复键只留一行");
    }

    /// 空批次合法：不报错、不写。
    #[test]
    fn set_many_with_empty_slice_is_a_no_op() {
        let dir = TempDir::new("kv-set-many-empty");
        let store = open_store(&dir);
        store
            .set_kv_many(&bot_id(), &[], 1_000)
            .expect("empty batch must be accepted");
        assert!(
            store
                .list_kv(&bot_id(), None, 100, 1_000)
                .expect("list after empty batch")
                .is_empty(),
            "空批次不许写任何行"
        );
    }

    /// 写一个不存在的 bot：外键必须报错，不许静默成功。
    ///
    /// `open_store` 只种了一个 bot（`BOT_SEED`）；`bot_id_at(999)` 不存在。
    /// `foreign_keys=ON` 由 `Store::open` 对每条连接施加，这里顺带钉住"KV 也在外键覆盖内"。
    #[test]
    fn writing_kv_for_an_unknown_bot_is_a_foreign_key_error() {
        let dir = TempDir::new("kv-foreign-key");
        let store = open_store(&dir);
        let ghost = bot_id_at(999);

        let error = store
            .set_kv(&ghost, "k", "1", None, 1_000)
            .expect_err("外键必须拦住不存在的 bot");
        assert!(
            error.to_string().to_lowercase().contains("foreign key"),
            "错误必须是外键违规，实际是：{error}"
        );

        // 批量写里同一条 FK 也必须失败（否则"一半落库"就变成了静默丢数据）。
        let error = store
            .set_kv_many(&ghost, &[entry("k", "1", None)], 1_000)
            .expect_err("批量写也必须被外键拦住");
        assert!(
            error.to_string().to_lowercase().contains("foreign key"),
            "批量写的外键错误，实际是：{error}"
        );
    }

    /// 统计 `bot_kv` 行数（裸连接）。
    fn count_keys(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM bot_kv", [], |row| row.get(0))
            .expect("count bot_kv rows")
    }
}
