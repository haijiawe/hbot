-- 0007_bot_waits.sql —— 会话等待（C2.session，docs/07-bot-runtime-blueprint.md 的 5.1 节）。
--
-- ## 为什么等待必须落库
--
-- 旧项目的 wait_input 把等待表放在内存里（docs/00-assessment.md 的 P0-3）：进程一重启
-- 等待就消失，而用户那边看到的是「bot 问了我一句，我回了，它没反应」—— 一个只会
-- 在真机上、只在重启后出现的静默失败。所以等待行必须落库，重启后仍能被回答。
--
-- ## 语义：同会话同时只能有一个未结等待（硬规则）
--
-- 落地判据是那条**部分唯一索引** bot_waits_conv_pending：只在 state = 'pending' 上唯一。
-- 两个未结等待并存时，「用户这句话回答的是哪一个」没有正确答案 —— 猜最近的那个会被
-- 乱序消息打穿，猜最老的那个会让后开的等待饿死。放到数据库里兜住，是因为先查后插
-- 在并发下两次都会查到「没有」。
--
-- 为什么是部分索引而不是普通唯一索引：同一会话**历史上有过**很多等待（超时的、被回答
-- 的），它们要能共存；只有「还开着的那条」唯一。
--
-- ## 状态列是契约
--
-- state 的拼写只有四种，与 hbot_core::bot_session::WaitState::as_str() 同值：
-- pending / fulfilled / expired / cancelled。存储层不校验拼写（那是 hbot-core 的
-- validate 与装配层的职责），但必须与那一份拼写保持一致。
--
-- ## 为什么 scope 也落在行上
--
-- 「谁能回答」是这次的语义，不是一条全局策略：同一个 bot 可能既开「只认提问者」的
-- 等待，也开「群里谁都能答」的等待。写进行里，claim 的那条 SQL 才能一步判完
-- （scope = 'conversation' OR owner = ?sender），不必先读出来再回 JS 判一遍。
--
-- ## 为什么 deadline 在行上而不是靠 tick 扫
--
-- 超时必须能**离线成立**：一个没人回的问题在「没有任何新消息」时也要到点结束。
-- deadline 是绝对时刻（调用方按 now + timeout 算好），tick 只做 WHERE deadline <= ?now。
-- 库不读时钟（不许出现 strftime / CURRENT_TIMESTAMP），否则测试注入不了边界。
--
-- ## 级联
--
-- bot_id 级联删除，与 logs / bot_kv / bot_jobs 同口径：删 bot 就带走它的等待，
-- 不留孤儿行。
--
-- 规则沿用 0001–0006（迁移只增不改）：本文件发布后永不编辑；全部 CREATE … IF NOT EXISTS；
-- 不在 SQL 里取当前时间。

CREATE TABLE IF NOT EXISTS bot_waits (
    -- 等待行主键（UUID 文本）；一次 prompt 一个 ID，唤醒 / 超时 / 取消都只是改这一行。
    id                        TEXT    NOT NULL PRIMARY KEY,
    bot_id                    TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 会话行主键（conversations.id）；回复与归属都按它算。
    conversation_id           TEXT    NOT NULL,
    -- 会话的平台外部 ID（externalId）：回复目标直接用它，免得每次唤醒都回查一次会话表。
    conversation_external_id  TEXT    NOT NULL,
    -- 开等待的人（平台 sender_id）；scope = sender 时只有他能回答。
    owner                     TEXT    NOT NULL,
    -- 等待键：onResume 里 ctx.waitKey 就是它。长度由 hbot-core 的 MAX_WAIT_KEY_BYTES 约束。
    key                       TEXT    NOT NULL,
    -- 注册时发给用户的那句话（超时日志里要能说清「哪个等待过期了」）。
    prompt                    TEXT    NOT NULL,
    -- 谁可以回答：sender / conversation（与 WaitScope::as_str() 同拼写）。
    scope                     TEXT    NOT NULL,
    -- 状态：pending / fulfilled / expired / cancelled（与 WaitState::as_str() 同拼写）。
    state                     TEXT    NOT NULL,
    -- 到这个时刻还没人回答就算超时（Unix 毫秒，调用方注入）。
    deadline                  INTEGER NOT NULL,
    -- 唤醒它的那条消息（本仓库的 message id）；未唤醒为 NULL。
    fulfilled_message_id      TEXT,
    created_at                INTEGER NOT NULL,
    updated_at                INTEGER NOT NULL
);

-- 「同一会话同时只有一个未结等待」（上面语义一节）。部分索引：只约束 pending。
CREATE UNIQUE INDEX IF NOT EXISTS bot_waits_conv_pending
    ON bot_waits (bot_id, conversation_id)
    WHERE state = 'pending';

-- tick 每轮的超时结算：WHERE state = 'pending' AND deadline <= ?now ORDER BY deadline。
-- 前导列 state 定住只扫一个分区，deadline 让排序走索引而不是临时表。
CREATE INDEX IF NOT EXISTS bot_waits_due
    ON bot_waits (state, deadline);

-- bot_stop 取消全部未结等待：只按 bot_id 过滤。
CREATE INDEX IF NOT EXISTS bot_waits_by_bot
    ON bot_waits (bot_id, state);
