-- 0004_device.sql —— 设备能力（C6）的会话与审计（docs/09-device-capability.md §3）。
--
-- 两张表都落在现有的 hbot.db 里，理由不是"顺手"：
--   * 会话必须能跨重启存活。进程崩了、被杀了、重开之后，"上次那个会话还算不算数"
--     这条判定不能靠"内存里没有 = 安全"来猜 —— 那会把一次崩溃变成一次静默的提权。
--   * 审计要能回看"上周二 15:03 那个会话是谁开的、开了多久、点了什么"。写进内存或
--     普通日志文件，重开就断了；而写入 hbot.db 正好沿用 logs 的同一套落盘/备份路径。
--
-- ## 审计不存内容（硬规则：docs/09 §3.2、§4.3；docs/07 §9.3 第 5 条）
--
-- device_audit 刻意**没有** content / text / png / image / ocr / clipboard 之类的列，
-- 以后也不许加。target 只允许写**进程名 / 窗口 id / 子能力名**这类标识符。
-- 记了内容，审计本身就变成一个新的泄漏面：密码框的像素、剪贴板里的 token、屏幕上的
-- OCR 原文会一直躺在库里，被导出、被 UI 展示、被喂进模型上下文 —— 而它们恰恰是
-- 设备能力最该防的东西。列清单由 src/device_tests.rs 的
-- device_audit_rejects_content_columns 用 PRAGMA table_info 钉死。
--
-- 规则沿用 0001–0003（迁移只增不改，docs/05-quality-gates.md §2 G6）：
--   * 本文件发布后永不编辑；要改就新开 0005_*.sql；
--   * 幂等：全部 CREATE … IF NOT EXISTS，重复执行安全（device_tests 会真的重跑一遍）；
--   * bot_id 级联删除：删项目 → 级联删 bot → 级联删它的会话与审计，与 logs 同口径，
--     不留"已删 bot 的授权还活着"这种孤儿行。

CREATE TABLE IF NOT EXISTS device_sessions (
    -- UUIDv4（调用方生成；存储层不造 ID，与 models.rs 的约定一致）。
    id                TEXT    PRIMARY KEY NOT NULL,
    bot_id            TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 取值见 hbot_core::DeviceSessionState::as_str()：
    -- requested / active / expired / revoked / denied。
    -- 存储层只当字符串存，不认识领域枚举（与 BotStatusRow.status 同口径）。
    state             TEXT    NOT NULL,
    -- 这一次批准的子能力 JSON 数组，例如 ["screen","input"]；[] = 一个都没有。
    -- 用 JSON 而不是逗号分隔：解析失败即坏数据，交给上层报错，不需要猜分隔符。
    sub_json          TEXT    NOT NULL,
    -- 允许的应用 JSON 数组（进程映像名）；[] = 全拒（不是全放）。
    allowed_apps_json TEXT    NOT NULL,
    -- Unix 毫秒。requested 态可能还没批，所以这两列可空。
    granted_at        INTEGER,
    deadline          INTEGER,
    -- 结束时刻；NULL = 还没结束（requested / active 态）。
    ended_at          INTEGER,
    created_at        INTEGER NOT NULL
);

-- latest_device_session 的读法：
--   WHERE bot_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1
-- 索引里带 DESC 是让这条查询直接按索引顺序取首行，不用给每个 bot 排一次临时表。
CREATE INDEX IF NOT EXISTS idx_device_sessions_bot_created
    ON device_sessions (bot_id, created_at DESC);

CREATE TABLE IF NOT EXISTS device_audit (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Unix 毫秒（调用方注入的时钟；存储层不读时钟）。
    at      INTEGER NOT NULL,
    bot_id  TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    -- 谁在请求：'bot_code' | 'agent_tool'（hbot_core::DeviceActor::as_str()）。
    actor   TEXT    NOT NULL,
    -- 动作名：screenshot / click / key / move / focus / clipboard_read / clipboard_write …
    action  TEXT    NOT NULL,
    -- 目标：只允许进程名 / 窗口 id / 子能力名。禁止像素、剪贴板文本、OCR 原文（见文件头）。
    target  TEXT,
    -- 结果：ok / denied / failed。
    outcome TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_device_audit_bot_at ON device_audit (bot_id, at DESC);