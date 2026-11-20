-- 表结构只能来自这里：运行时 ctx.sql 不许 DDL（HBOT_SQL_DENIED）。
-- 宿主在 bot_start 里、onStart 之前执行一次，按文件名去重（schema_meta）。
CREATE TABLE IF NOT EXISTS scores (
  sender_id  TEXT PRIMARY KEY,
  best       INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
