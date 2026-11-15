-- 0001_init.sql — 初始 schema（projects / bots / conversations / inbound / outbound / logs）。
--
-- 规则（docs/05-quality-gates.md §2 G6、§7）：
--   * 迁移只增不改：已发布的迁移永不编辑，改动一律新开 0002_*.sql。
--   * 执行顺序按版本号单调递增，由 src/migrations.rs 负责；本文件只含 DDL。
--   * 跨表引用用 ON DELETE CASCADE：删除项目 = 级联删除其 bot / 会话 / 消息 / 日志。

CREATE TABLE IF NOT EXISTS projects (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL,
    path       TEXT    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS bots (
    id          TEXT    PRIMARY KEY NOT NULL,
    project_id  TEXT    NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    platform    TEXT    NOT NULL,
    config_json TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_bots_project ON bots (project_id);

CREATE TABLE IF NOT EXISTS conversations (
    id          TEXT    PRIMARY KEY NOT NULL,
    bot_id      TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    external_id TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (bot_id, external_id)
);

CREATE TABLE IF NOT EXISTS inbound (
    id              TEXT    PRIMARY KEY NOT NULL,
    bot_id          TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    external_id     TEXT    NOT NULL,
    conversation_id TEXT    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    payload_json    TEXT    NOT NULL,
    received_at     INTEGER NOT NULL,
    processed_at    INTEGER,
    UNIQUE (bot_id, external_id)
);

CREATE INDEX IF NOT EXISTS idx_inbound_pending ON inbound (bot_id, processed_at);

CREATE TABLE IF NOT EXISTS outbound (
    id              TEXT    PRIMARY KEY NOT NULL,
    bot_id          TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    conversation_id TEXT    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    text            TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    sent_at         INTEGER
);

CREATE INDEX IF NOT EXISTS idx_outbound_state ON outbound (bot_id, state, created_at);

CREATE TABLE IF NOT EXISTS logs (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id TEXT    NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    level  TEXT    NOT NULL,
    line   TEXT    NOT NULL,
    at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_logs_bot_at ON logs (bot_id, at);