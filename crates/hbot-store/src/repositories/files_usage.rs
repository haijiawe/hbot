//! `bot_file_usage` —— 每个 bot 在项目里已占用的字节数（C3.file 的磁盘配额）。
//!
//! 只做三件事：读当前占用、**原子地**预留一段额度、释放一段额度。
//! 不认识文件、不扫树、不读时钟 —— `now` 一律由调用方传入。
//!
//! ## 为什么预留必须是一条 SQL
//!
//! 同一 bot 的不同会话是**并行**的（`docs/02-architecture.md` §4：每会话串行、跨会话并行），
//! 所以「先读占用、再判够不够、最后加」三步在并发下会让两个写都看到同一个旧值、都通过，
//! 最终超出的量取决于并发度。把判据放进 `ON CONFLICT … WHERE` 里之后，
//! 每次预留都是一条语句、一个事务，SQLite 自己保证「读到的值与写回的值是同一个」。
//! 判据失败时 `changes()` 为 0（什么都没改），调用方据此报配额不足。
//!
//! ## 为什么 bytes 减到 0 要删行
//!
//! 一张「全是 0」的表会让「这个 bot 到底写过东西没有」不可判，也会让排查配额问题时
//! 多出一堆噪声行。删除全部文件之后占用就该回到「没有这一行」这个初始状态。

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::ids::BotId;

/// 读当前占用；没有行 = 0（建 bot 时不插行，见迁移文件头）。
///
/// 用 `COALESCE` 包一层子查询而不是 `query_row(...).optional()`：这样**永远恰好一行**，
/// 「没有行」与「读到 0」在调用方看来是同一件事，不需要两处判空。
pub(crate) fn used(conn: &Connection, bot_id: &BotId) -> Result<u64> {
    let bytes = conn.query_row(
        "SELECT COALESCE((SELECT bytes FROM bot_file_usage WHERE bot_id = ?1), 0)",
        params![bot_id.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    // 表里的 bytes 由本模块维护，恒 >= 0；读成 i64 是因为 SQLite 没有无符号整数。
    Ok(bytes.max(0) as u64)
}

/// 预留 `delta` 字节；成功返回**预留之后**的总占用。
///
/// `limit == 0` 表示不限（与 `limits.quotaMb = 0` 同口径），此时无条件累加。
/// 超出上限时**一行都不改**，返回 `Ok(None)`。
///
/// `delta == 0` 是合法输入（覆盖写但长度没变）：直接返回当前占用，不写库 ——
/// 否则会凭空建出一行 `bytes = 0`，破坏「没有行 = 没写过」这条口径。
pub(crate) fn reserve(
    conn: &Connection,
    bot_id: &BotId,
    delta: u64,
    limit: u64,
    now: i64,
) -> Result<Option<u64>> {
    if delta == 0 {
        return Ok(Some(used(conn, bot_id)?));
    }
    let delta = delta.min(i64::MAX as u64) as i64;
    let changed = if limit == 0 {
        // 不限：INSERT 分支与 UPDATE 分支都无条件成立。
        conn.execute(
            "INSERT INTO bot_file_usage (bot_id, bytes, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (bot_id) DO UPDATE SET
                 bytes = bot_file_usage.bytes + excluded.bytes,
                 updated_at = excluded.updated_at",
            params![bot_id.to_string(), delta, now],
        )?
    } else {
        // 有上限：判据放在两个分支上，超了就是「一行都没改」。
        // 注意 UPDATE 分支的 WHERE 里必须写 `bot_file_usage.bytes` 而不是裸 `bytes`：
        // 在 DO UPDATE 的 WHERE 里裸列名指的是**旧行**，而 `excluded.bytes` 才是要加的量；
        // 写成 `bytes + excluded.bytes <= ?4` 是对的，但显式带上表名更难被改错。
        conn.execute(
            "INSERT INTO bot_file_usage (bot_id, bytes, updated_at)
             SELECT ?1, ?2, ?3 WHERE ?2 <= ?4
             ON CONFLICT (bot_id) DO UPDATE SET
                 bytes = bot_file_usage.bytes + excluded.bytes,
                 updated_at = excluded.updated_at
             WHERE bot_file_usage.bytes + excluded.bytes <= ?4",
            params![
                bot_id.to_string(),
                delta,
                now,
                limit.min(i64::MAX as u64) as i64
            ],
        )?
    };
    if changed == 0 {
        return Ok(None);
    }
    Ok(Some(used(conn, bot_id)?))
}

/// 释放 `delta` 字节（删除文件、覆盖写变短时调用）；返回释放之后的总占用。
///
/// 减到 0 或更少时**删行**（见模块文档）。减到负数按 0 处理：账本比真实磁盘小是允许的
/// （见迁移文件头的取舍），但**绝不允许变成负数** —— 负占用会让下一次预留多算出额度。
pub(crate) fn release(conn: &Connection, bot_id: &BotId, delta: u64, now: i64) -> Result<u64> {
    if delta == 0 {
        return used(conn, bot_id);
    }
    let delta = delta.min(i64::MAX as u64) as i64;
    conn.execute(
        "UPDATE bot_file_usage SET
             bytes = MAX(bytes - ?2, 0),
             updated_at = ?3
         WHERE bot_id = ?1",
        params![bot_id.to_string(), delta, now],
    )?;
    let remaining = used(conn, bot_id)?;
    if remaining == 0 {
        // 删掉归零的行：见模块文档「为什么 bytes 减到 0 要删行」。
        conn.execute(
            "DELETE FROM bot_file_usage WHERE bot_id = ?1 AND bytes <= 0",
            params![bot_id.to_string()],
        )?;
    }
    Ok(remaining)
}

/// 把占用直接设成 `bytes`（对账用：IDE 里手工改了文件之后重新算一遍）。
///
/// 生产路径**不调用**它 —— 配额账本只按写/删增减，扫树对账是显式动作，不该出现在热路径上。
/// 留着它是因为「用户手工删了文件，额度该还回来」这件事最终要有个入口。
pub(crate) fn set(conn: &Connection, bot_id: &BotId, bytes: u64, now: i64) -> Result<()> {
    if bytes == 0 {
        conn.execute(
            "DELETE FROM bot_file_usage WHERE bot_id = ?1",
            params![bot_id.to_string()],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO bot_file_usage (bot_id, bytes, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (bot_id) DO UPDATE SET
             bytes = excluded.bytes,
             updated_at = excluded.updated_at",
        params![bot_id.to_string(), bytes.min(i64::MAX as u64) as i64, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, bot_id, bot_id_at, open_store, raw};

    /// 一条 1 MiB 的配额，方便算边界。
    const MB: u64 = 1024 * 1024;

    fn usage_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM bot_file_usage", [], |row| row.get(0))
            .expect("count usage rows")
    }

    /// 没有行 = 占用 0；预留之后才有行；释放到 0 之后行必须消失。
    #[test]
    fn a_missing_row_means_zero_and_the_row_disappears_at_zero() {
        let dir = TempDir::new("usage-zero");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        assert_eq!(used(&conn, &bot_id()).expect("used"), 0);
        assert_eq!(usage_rows(&conn), 0, "没有写过就没有行");

        assert_eq!(
            reserve(&conn, &bot_id(), 100, 0, 1_000).expect("reserve"),
            Some(100)
        );
        assert_eq!(usage_rows(&conn), 1, "预留之后才建行");

        assert_eq!(release(&conn, &bot_id(), 100, 1_100).expect("release"), 0);
        assert_eq!(
            usage_rows(&conn),
            0,
            "减到 0 必须删行，不留 bytes = 0 的空壳"
        );
    }

    /// 上限判据看的是**预留之后**的总量；超了**一行都不改**。
    #[test]
    fn the_limit_looks_at_the_total_after_the_reservation() {
        let dir = TempDir::new("usage-limit");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);
        let limit = 1_000;

        assert_eq!(
            reserve(&conn, &bot_id(), 900, limit, 1_000).expect("900"),
            Some(900)
        );
        // 900 + 100 = 1000 == limit：放行（判据是 `>` 不是 `>=`）。
        assert_eq!(
            reserve(&conn, &bot_id(), 100, limit, 1_100).expect("100"),
            Some(1_000)
        );
        // 再多 1 字节就超了。
        assert_eq!(
            reserve(&conn, &bot_id(), 1, limit, 1_200).expect("1 over"),
            None,
            "超出上限必须拒绝"
        );
        assert_eq!(
            used(&conn, &bot_id()).expect("used"),
            1_000,
            "拒绝时不许改账"
        );
    }

    /// 全新 bot（还没有行）也要受上限约束：第一笔就超限必须拒绝，不能靠"没有行"绕过去。
    #[test]
    fn the_first_reservation_is_also_checked_against_the_limit() {
        let dir = TempDir::new("usage-first");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        assert_eq!(
            reserve(&conn, &bot_id(), 2 * MB, MB, 1_000).expect("first over"),
            None,
            "第一笔就超限必须拒绝"
        );
        assert_eq!(usage_rows(&conn), 0, "拒绝时连行都不该建");
    }

    /// `limit == 0` 表示不限：多大的量都收。
    #[test]
    fn a_zero_limit_means_unlimited() {
        let dir = TempDir::new("usage-unlimited");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        assert_eq!(
            reserve(&conn, &bot_id(), 10 * MB, 0, 1_000).expect("big"),
            Some(10 * MB)
        );
        assert_eq!(
            reserve(&conn, &bot_id(), 10 * MB, 0, 1_100).expect("bigger"),
            Some(20 * MB)
        );
    }

    /// 预留 0 字节不建行：否则「没写过」与「写了个空文件」就分不开了。
    #[test]
    fn a_zero_delta_never_creates_a_row() {
        let dir = TempDir::new("usage-zero-delta");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        assert_eq!(
            reserve(&conn, &bot_id(), 0, MB, 1_000).expect("zero"),
            Some(0)
        );
        assert_eq!(usage_rows(&conn), 0, "0 字节不该建行");
    }

    /// 释放比账上多的量时按 0 处理，**绝不允许变成负数**（负占用会让下一次预留多算额度）。
    #[test]
    fn releasing_more_than_used_clamps_at_zero() {
        let dir = TempDir::new("usage-clamp");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        reserve(&conn, &bot_id(), 100, MB, 1_000).expect("reserve");
        assert_eq!(
            release(&conn, &bot_id(), 500, 1_100).expect("over release"),
            0
        );
        assert_eq!(usage_rows(&conn), 0);

        // 夹成 0 之后还能正常再预留，不会带着负数起步。
        assert_eq!(
            reserve(&conn, &bot_id(), MB, MB, 1_200).expect("after clamp"),
            Some(MB)
        );
    }

    /// 两个 bot 各记各的账：A 的占用不影响 B 的额度。
    #[test]
    fn two_bots_keep_separate_ledgers() {
        let dir = TempDir::new("usage-two-bots");
        let store = open_store(&dir);
        let other = crate::test_support::seed_extra_bot(&store, 0x9999);
        drop(store);
        let conn = raw(&dir);
        let limit = 1_000;

        assert_eq!(
            reserve(&conn, &bot_id(), 1_000, limit, 1_000).expect("a"),
            Some(1_000)
        );
        // A 已经满了，B 仍然可以写满自己那份。
        assert_eq!(
            reserve(&conn, &other, 1_000, limit, 1_100).expect("b"),
            Some(1_000)
        );
        assert_eq!(used(&conn, &bot_id()).expect("a used"), 1_000);
        assert_eq!(used(&conn, &other).expect("b used"), 1_000);
    }

    /// `set` 用来对账（IDE 里手工改了文件之后重算）：写 0 等价于清账。
    #[test]
    fn set_replaces_the_whole_ledger_and_zero_clears_it() {
        let dir = TempDir::new("usage-set");
        let store = open_store(&dir);
        drop(store);
        let conn = raw(&dir);

        reserve(&conn, &bot_id(), 300, 0, 1_000).expect("reserve");
        set(&conn, &bot_id(), 42, 1_100).expect("set");
        assert_eq!(used(&conn, &bot_id()).expect("used"), 42);

        set(&conn, &bot_id(), 0, 1_200).expect("set zero");
        assert_eq!(usage_rows(&conn), 0, "对账成 0 也要删行");
    }

    /// 删 bot 级联删账（迁移里的 ON DELETE CASCADE）。
    #[test]
    fn deleting_the_bot_removes_its_ledger() {
        let dir = TempDir::new("usage-cascade");
        let store = open_store(&dir);
        let other = bot_id_at(0x9999);
        drop(store);
        let conn = raw(&dir);
        reserve(&conn, &bot_id(), 100, 0, 1_000).expect("reserve");
        assert_eq!(usage_rows(&conn), 1);

        conn.execute(
            "DELETE FROM bots WHERE id = ?1",
            params![bot_id().to_string()],
        )
        .expect("delete bot");
        assert_eq!(usage_rows(&conn), 0, "删 bot 必须级联删掉账本");
        // 对照：另一个 bot 的账不受影响（这里它本来就没有行，只是确认查询不报错）。
        assert_eq!(used(&conn, &other).expect("other"), 0);
    }
}
