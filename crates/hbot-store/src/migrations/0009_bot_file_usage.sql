-- 0009_bot_file_usage.sql —— 每个 bot 在项目里已占用的字节数（C3.file 的磁盘配额）。
--
-- 为什么是一张新表而不是每次扫项目树：
--   * 配额判据在**每一次写**之前都要问一次「还放得下吗」。扫树是 O(项目) 的磁盘操作，
--     而热路径上只允许 O(1)（docs/pitfalls.md 坑 23：sandbox_handler 曾每条消息重算
--     全项目指纹，全量读盘 + blake3）；
--   * 计数与真实磁盘的差值是**可接受的**：用户在 IDE 里手工改文件、别的工具写项目，
--     都不会经过这里。配额不是安全边界（路径已经钉在项目根里），它只挡「生成代码把盘写满」
--     这一种事故，宁可少算也不要让每条消息付一次扫树的代价。
--
-- ## 为什么主键是 bot_id 而不是 (bot_id, project_id)
--
-- 文件能力的装配粒度就是 bot：`ProjectFiles` 由 `start_bot` 创建，一个 bot 一个实例，
-- 路径解析也钉在这个 bot 的项目根上。同一项目里跑两个 bot 时，配额各算各的
-- ——这与 `bot_kv` / `bot_jobs` / `bot_waits` 同口径（都是 per-bot 的资源）。
--
-- ## 为什么 bytes 允许没有行
--
-- 没有行 = 占用 0，不需要在 bot 建行时插一条。`add` 用 upsert 补行，
-- 读用 `COALESCE(..., 0)`：少一次「先建行」的写，也就少一处「建行失败但 bot 已经起来了」。
--
-- ## 级联
--
-- bot_id 级联删除：删项目 → 级联删 bot → 级联删它的占用计数，不留孤儿行。
-- 注意这里**不删文件**：配额表是账本，不是文件清单；项目根里的文件归用户所有，
-- 删 bot 不该顺手删用户的文件。
--
-- 规则沿用 0001–0008（迁移只增不改，docs/05-quality-gates.md §2 G6）：
--   * 本文件发布后永不编辑；要改就新开 0010_*.sql；
--   * 幂等：全部 CREATE … IF NOT EXISTS，重复执行安全；
--   * 不许在 SQL 里取当前时间（`strftime` / `julianday` / `CURRENT_TIMESTAMP`）：
--     时间一律由调用方传 `now`。

CREATE TABLE IF NOT EXISTS bot_file_usage (
    bot_id     TEXT    PRIMARY KEY NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 已占用的字节数。减到 0 时**删行**（见 repositories/files_usage.rs），不留 bytes = 0 的行：
    -- 一张「全是 0」的表会让「这个 bot 有没有写过东西」变得不可判。
    bytes      INTEGER NOT NULL,
    -- 最后一次变动时刻（Unix 毫秒，调用方注入的时钟）。
    updated_at INTEGER NOT NULL
);
