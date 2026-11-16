-- 0003_agent_logs.sql
--
-- 让 Agent 的工具调用日志也能落进 `logs`：
--   1. `logs.bot_id` 改为可空：Agent 会话不属于任何 bot，不能塞假 bot_id；
--   2. 新增 `source` 区分日志来源：老行为 'bot'，Agent 行写 'agent'。
--
-- SQLite 不支持 `ALTER COLUMN`，按官方推荐的标准做法重建表：
--   建新表 → 复制旧行 → drop 旧表 → rename 新表。
-- 整个迁移由迁移执行器包在同一个事务里（`migrations.rs`），失败不会留下半张表。
--
-- 会话 ID 不单独建列：`logs` 是通用运行日志表，UI 只需要看到"这行属于哪个会话"，
-- 写进 `line` 前缀（`[session 3f2a…]`）即可，避免为一个展示字段加列并扩散到所有查询。

DROP TABLE IF EXISTS logs_new;

CREATE TABLE logs_new (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id  TEXT REFERENCES bots (id) ON DELETE CASCADE,
    level   TEXT NOT NULL,
    line    TEXT NOT NULL,
    at      INTEGER NOT NULL,
    source  TEXT NOT NULL DEFAULT 'bot'
);

-- 老库全部是 bot 日志；显式写 'bot'，不依赖新列默认值，复制语义更清楚。
INSERT INTO logs_new (id, bot_id, level, line, at, source)
SELECT id, bot_id, level, line, at, 'bot' FROM logs;

DROP TABLE logs;

ALTER TABLE logs_new RENAME TO logs;

-- 保留既有按 bot 的时间索引，并补一条全项目按时间读的索引（`logs_tail { bot_id: null }`）。
CREATE INDEX IF NOT EXISTS idx_logs_bot_at ON logs (bot_id, at);
CREATE INDEX IF NOT EXISTS idx_logs_at ON logs (at);
