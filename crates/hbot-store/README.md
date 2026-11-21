# hbot-store

> SQLite 持久层：迁移、连接、repository。**只做存储，不放业务规则**。

## 概述

库的唯一真源是 `src/migrations/*.sql`（编号递增，`migrations.rs` 里登记）；SQL 语句只出现在 `src/repositories/`。
连接是单写多读：写走一条 `Mutex<Connection>`，读走独立只读连接（`PRAGMA query_only=ON`），所以长查询不会挡住写（`src/bot_sql.rs`）。

## 目录

- [关键文件](#关键文件)
- [迁移与表](#迁移与表)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/db.rs` | `Store` 本体：打开、迁移、`StoreOp` 枚举与 dispatch（每加一个操作要在这里加臂） |
| `src/migrations.rs` | 迁移清单：版本号 → SQL 文件，改这里与 `tests.rs` 的 schema 断言必须同批 |
| `src/models.rs` | 行结构与新建结构（`BotRow` / `InboundRow` / `NewOutbound` / `JobRow` …） |
| `src/mappers.rs` | 列常量与 `row_to_*`：列顺序与索引写死在这里，加列时索引要整体后移 |
| `src/repositories/history.rs` | 入站 / 出站历史与待发扫描（`list_pending_outbound` 必须按 `created_at, rowid` 排序） |
| `src/repositories/inbound.rs` | `insert_inbound_if_new`（唯一键去重，第 1 层 exactly-once） |
| `src/repositories/outbound.rs` | 出站入队、发送计数、死信 |
| `src/repositories/waits.rs` | `C2.session`：等待注册 / 认领 / 过期，各一条 `UPDATE … RETURNING` |
| `src/repositories/jobs.rs` | `C4.jobs`：入队、按 bot 认领、重排、结算、崩溃恢复 |
| `src/repositories/kv.rs` | `C1.kv`：批量写在一个事务里 |
| `src/repositories/files_usage.rs` | `C3.file` 的每 bot 磁盘用量（原子预留） |
| `src/repositories/device.rs` | 设备审计与限时会话 |
| `src/bot_sql.rs` | `C7.sql` 的每 bot 独立库文件与只读连接 |
| `src/test_support.rs` | 测试夹具：`open_store`（**内部已经 seed 过**，调用方不要再 seed） |

## 迁移与表

当前到 `0010_outbound_attachments`。命名 `00NN_<snake_case>`，一条迁移只做一件事；
新增列的默认值要能让旧行继续工作（`kind TEXT NOT NULL DEFAULT text` 就是范例）。
改完迁移必须同步 `src/tests.rs` 里的三个断言：`schema_version`、`user_version`、`recorded.len()`。

## 边界：能放什么 / 不能放什么

- **能放**：SQL、行映射、事务、索引。
- **不能放**：去重策略、重试节奏、审批规则（业务在 `hbot-bot` / `hbot-agent`）；ID 类型定义（重导出 `hbot_core::ids`）。

## 上游与下游

- 上游：`hbot-core`。
- 下游：`hbot-bot`、`hbot-rpc`、`hbot-cli`、`hbot-replay`、`hbot-bench`。

## 测试

`cargo test -p hbot-store` —— 一百多条，覆盖迁移升级链、并发认领、事务回滚与每个 repository 的边界。
特别注意 `per_bot_claim_never_takes_another_bots_row`：全局 claim 会让 A 跑 B 的作业（`AGENTS.md` 坑 68）。

## 已知限制

- 没有连接池：写的串行化是设计选择（单进程宿主），不是待优化项。
- 迁移只前进不后退，没有 down 脚本；线上库要靠备份，不靠回滚。
