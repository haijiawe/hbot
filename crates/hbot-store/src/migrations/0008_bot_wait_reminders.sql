-- 0008_bot_wait_reminders.sql —— 等待超时后的重发（C2.session，docs/07 §5.1）。
--
-- ## 为什么需要这两列
--
-- 0007 里超时是单程的：到点就把行改成 expired，然后在日志里留一句「没等到」。
-- 对很多场景那已经够用，但对「问一句要紧的事」不够 —— 用户只是没看见，不是不想答，
-- 而 bot 这边没有任何再问一次的出口（handler 早就跑完了，没法再 prompt 一次）。
--
-- ## 为什么 reminders_left 与 timeout_ms 落在行上
--
-- 重发要回答两个问题：「还能重发几次」（reminders_left，每重发一次减一）与
-- 「每次等多久」（timeout_ms）。后者不能现读调用方参数：重发发生在 tick 里，
-- 那里只有一个已经落库的行；而用「deadline - created_at」反推在重发之后就不再成立
-- （deadline 已经被推后过），所以必须把那一轮的间隔存下来。
--
-- ## 默认值
--
-- reminders_left = 0 与 timeout_ms = 300000（DEFAULT_WAIT_TIMEOUT_MS）让 0007 的历史行
-- 保持既有语义：一次到点就 expired，不重发。
--
-- 规则沿用 0001–0007（迁移只增不改）：本文件发布后永不编辑；ALTER 都带
-- NOT NULL DEFAULT，老行自动得到「不重发」的形状；不在 SQL 里取当前时间。

ALTER TABLE bot_waits ADD COLUMN reminders_left INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bot_waits ADD COLUMN timeout_ms INTEGER NOT NULL DEFAULT 300000;
