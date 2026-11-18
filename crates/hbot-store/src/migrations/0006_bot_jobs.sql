-- 0006_bot_jobs.sql —— 后台作业（C4.jobs，docs/07-bot-runtime-blueprint.md 的 4.3 节）。
--
-- 为什么是一张新表：
--   * 作业是**跨分钟**的重活（结算、批量推送、抓取+分析），它的生命周期与入站消息、
--     出站路由、日志都不同：一条作业要能被"入队 → 派发 → 重试 → 终态"反复改写；
--   * 它是**持久化队列**，不是内存队列：进程重启后未完成的作业必须继续跑
--     （否则"至少一次"退化成"跑完才算数"，而真机上进程就是会被杀）；
--   * 它必须能被按 bot 整体取消（bot_stop），也要能按 id 单独定位，
--     所以既不能挂在 bots 上，也不能塞进 logs（日志是只追加的账本）。
--
-- ## 语义：至少一次，靠幂等键去重（硬规则）
--
-- 「至少一次」的落地判据是 §UNIQUE (bot_id, idempotency_key)§：
-- 同一个幂等键最多产生一行。重复入队（用户连点两次、重启后补跑、平台重投）
-- 都命中唯一约束并被 ON CONFLICT DO NOTHING 变成"无操作"，而不是插出第二行。
-- 作业体自身仍然要对"执行两次"保持无害 —— 表只能保证不重复**排队**，
-- 保证不了"跑完之后崩溃被重跑"（那是 running 行在重启时回到 pending 的那条路）。
--
-- ## 状态列是契约
--
-- §state§ 的拼写只有五种，与 hbot_core::bot_jobs::JobState::as_str() 同值：
-- pending / running / done / failed / cancelled。存储层不校验拼写
-- （那是 hbot-core 的 validate 与装配层的职责），但它必须与那一份拼写保持一致。
--
-- ## 为什么 attempts 与 budget_ms 落在行上
--
-- 作业行是"这一次入队的快照"：声明（hbot.json 的 capabilities.jobs）第二天可能就改了
-- 预算或重试次数。已经排队的那一条必须按**入队当时**的预算跑，否则改清单会让
-- 正在重试的作业突然换一套退避节奏 —— 那种"配置改一下就换语义"在排障时最费时间。
--
-- ## 级联
--
-- bot_id 级联删除，与 logs / bot_kv / device_* 同口径：删项目 → 级联删 bot → 级联删它的作业，
-- 不留"已删 bot 的作业还在队列里等着跑"这种孤儿行。
--
-- ## 索引
--
-- §UNIQUE (bot_id, idempotency_key)§ 不是索引的全部：tick 每轮要问"现在有哪些到期作业"，
-- 那条查询只按 (state, next_at) 走，外加一个 LIMIT。所以单独建一条
-- (state, next_at) 索引；主键 (id) 自带唯一索引，不必再建。
--
-- 规则沿用 0001–0005（迁移只增不改，docs/05-quality-gates.md 的 G6）：
--   * 本文件发布后永不编辑；要改就新开 0007_*.sql；
--   * 幂等：全部 CREATE … IF NOT EXISTS，重复执行安全；
--   * 不许在 SQL 里取当前时间（strftime / julianday / CURRENT_TIMESTAMP）：
--     时间一律由调用方传 now，否则测试无法注入边界、重放也无法复现。

CREATE TABLE IF NOT EXISTS bot_jobs (
    -- 作业行主键（UUID 文本）；一次**入队**一个 ID，重试不会换 ID。
    id              TEXT    NOT NULL PRIMARY KEY,
    bot_id          TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 作业类型名；对应 hbot.json 里 capabilities.jobs 的一项。长度由
    -- hbot_core::bot_jobs::MAX_JOB_NAME_BYTES 约束，存储层不校验。
    name            TEXT    NOT NULL,
    -- 不透明载荷的 JSON 文本（原样入队、原样回填给作业体）；存储层不解析。
    payload_json    TEXT    NOT NULL,
    -- 幂等键：同一个 (bot_id, key) 最多一行（见上面的语义一节）。
    idempotency_key TEXT    NOT NULL,
    -- 状态：pending / running / done / failed / cancelled（与 core 同拼写）。
    state           TEXT    NOT NULL,
    -- 已经跑过几次（含当前这一次）；重试与"至少一次"都靠它计数。
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- 入队当时冻结的单次预算（毫秒）；改清单不影响已在队列里的行。
    budget_ms       INTEGER NOT NULL,
    -- 回复目标的平台会话 external_id；NULL = 只能记日志（ctx.reply 会明确报错）。
    conversation    TEXT,
    -- 下一次可以派发的时刻（Unix 毫秒）；首次入队 = created_at，重试 = now + 退避。
    next_at         INTEGER NOT NULL,
    -- 终态的失败正文（failed）；中间态重试时记最近一次的错，便于排障。
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (bot_id, idempotency_key)
);

-- tick 每轮的查询形如 WHERE state = 'pending' AND next_at <= ?now ORDER BY next_at LIMIT ?n。
-- 前导列 state 定住只扫一个分区，next_at 让排序走索引而不是临时表。
CREATE INDEX IF NOT EXISTS bot_jobs_due ON bot_jobs (state, next_at);

-- 按 bot 数"还有多少条没完成"（池满判定）与"bot_stop 要取消哪些"：
-- 这两条都只按 bot_id 过滤，索引让它不必全表扫。
CREATE INDEX IF NOT EXISTS bot_jobs_by_bot ON bot_jobs (bot_id, state);