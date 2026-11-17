-- 0005_bot_kv.sql —— bot 侧键值存储（C2 的 kv 能力，docs/07-bot-runtime-blueprint.md §5）。
--
-- 为什么是一张新表而不是复用 config_json / logs：
--   * 生成的 bot 代码读写的是**运行时状态**（计数器、会话标记、小存档），它随消息变化、
--     必须跨重启存活、且要能被多键原子写（`setMany` 的"读计数器 → 加一 → 写回"）；
--   * 它不属于任何一行业务记录，所以既不能挂在 bots 上（会跟配置互相踩），也不能塞进
--     logs（日志是只追加的账本，改一行就破坏审计）；
--   * 上下文里只有值本身（`KvEntry.value` 是任意 JSON），所以值统一按 JSON 文本存。
--
-- ## 过期是**读语义**，不是删除动作（硬规则）
--
-- `expires_at` 为 NULL = 永不过期；否则是 Unix 毫秒的到期时刻，由**调用方**注入
-- （存储层不读时钟，测试才能注入 `now` 精确断言边界）。到期行在
-- `get` / `list` 里被当作不存在，但**不顺手删**：读路径要能跑在只读连接上，
-- 顺手 DELETE 会让一次读变成一次写，且会与单 writer 线程抢锁。
-- 物理清理是另一件事（将来单独一条 DELETE 语句即可，不需要改 schema）。
--
-- ## 主键承担唯一索引
--
-- `PRIMARY KEY (bot_id, key)` 已经是 `(bot_id, key)` 上的唯一索引，
-- `get` / `set`（upsert） / `delete` 全部按它定位，**不再额外建索引**：
-- 多一个重复索引只会让每次写多维护一棵 B 树。
-- `list … WHERE bot_id = ? ORDER BY key` 也走这条主键（前导列 bot_id，键按 key 有序）。
--
-- ## 级联
--
-- bot_id 级联删除：删项目 → 级联删 bot → 级联删它的 KV，与 logs / device_* 同口径，
-- 不留"已删 bot 的状态还活着"这种孤儿行。
--
-- 规则沿用 0001–0004（迁移只增不改，docs/05-quality-gates.md §2 G6）：
--   * 本文件发布后永不编辑；要改就新开 0006_*.sql；
--   * 幂等：全部 CREATE … IF NOT EXISTS，重复执行安全；
--   * 不许在 SQL 里取当前时间（`strftime` / `julianday` / `CURRENT_TIMESTAMP`）：
--     时间一律由调用方传 `now`，否则测试无法注入边界、重放也无法复现。

CREATE TABLE IF NOT EXISTS bot_kv (
    bot_id     TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 键名（UTF-8）；长度上限由 hbot_core::bot_state::MAX_KV_KEY_BYTES 约束，存储层不校验。
    key        TEXT    NOT NULL,
    -- 值的 JSON 文本（`serde_json::to_string` 的产物）；存储层不解析。
    value_json TEXT    NOT NULL,
    -- 最后一次写入时刻（Unix 毫秒，调用方注入的时钟）。
    updated_at INTEGER NOT NULL,
    -- Unix 毫秒到期时刻；NULL = 永不过期（`set` 里可以把非 NULL 改回 NULL）。
    expires_at INTEGER,
    PRIMARY KEY (bot_id, key)
);
