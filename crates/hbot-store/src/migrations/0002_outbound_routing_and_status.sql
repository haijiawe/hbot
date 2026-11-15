-- 0002_outbound_routing_and_status.sql — 出站路由、入站发送者与 bot 运行状态。
--
-- 为什么新开文件而不是改 0001：已发布的迁移永不编辑（docs/05-quality-gates.md §2 G6）。
-- 0001 已随首个版本落库，任何字段增补都必须走新版本，user_version 单调递增。
--
-- 新增列一律带 DEFAULT：SQLite 的 `ALTER TABLE ADD COLUMN` 对 NOT NULL 列要求默认值，
-- 且老库里已有的行必须在不重写表的前提下拿到确定的初始值（不留 NULL 漂移）。

-- bot 生命周期状态：'stopped' | 'running' | 'degraded' | 'error'。
-- 取值由 Store::update_bot_status 校验（models::is_valid_bot_status），非法值不落库。
ALTER TABLE bots ADD COLUMN status TEXT NOT NULL DEFAULT 'stopped';

-- 状态最后一次变更时间（Unix 毫秒，调用方注入）；0 = 自建库起从未变更。
ALTER TABLE bots ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;

-- 发送者平台 ID（字符串形态，不是 UUID）。空串 = 平台没有提供（例如系统消息），
-- 不伪造 "-1"/"system" 之类的占位值，避免和真实 id 混淆。
ALTER TABLE inbound ADD COLUMN sender_id TEXT NOT NULL DEFAULT '';

-- 出站发送目标（平台相关的 JSON，由适配器解释）；'{}' = 未知/未指定。
ALTER TABLE outbound ADD COLUMN target_json TEXT NOT NULL DEFAULT '{}';

-- 被动回复引用的平台消息 ID；NULL = 主动消息（主动消息不得冒用旧事件，AGENTS.md §4.1）。
ALTER TABLE outbound ADD COLUMN reply_to TEXT;

-- 续发扫描：`WHERE state = 'pending' ORDER BY id`（Store::list_pending_outbound / count）。
CREATE INDEX IF NOT EXISTS idx_outbound_pending ON outbound (state, id);

-- 未处理扫描：`WHERE processed_at IS NULL ORDER BY received_at, id`。
CREATE INDEX IF NOT EXISTS idx_inbound_unprocessed ON inbound (processed_at, received_at, id);