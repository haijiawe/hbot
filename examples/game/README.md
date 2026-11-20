# 示例：游戏记账（sql + onSchedule）

一个用 SQLite 存最高分的排行榜。它演示两件事：

| 能力 | 用在哪 |
|---|---|
| `ctx.sql` | `query` 只读、`exec` 才写；每 bot 一个库文件 |
| `onSchedule` | 清单里具名的计划到点了调这个钩子 |

## 命令

| 发什么 | 效果 |
|---|---|
| `/score 42` | 只在刷出新纪录时更新 |
| `/me` | 看自己的最高分 |
| `/top` | 看前 10 名 |

## 表结构从哪来

`migrations/0001_init.sql` 在 bot 启动时由宿主执行一次（记在 `schema_meta` 里，不会重复跑）。
**运行时不能建表**：`CREATE` / `ALTER` / `DROP` 一律报 `HBOT_SQL_DENIED`。
迁移写错不会挡住启动（只落一行 error 日志），改好再重启即可。

## 定时器为什么不能查库

`onSchedule` 的 ctx 里**没有** `ctx.sql`：定时事件没有消息身份，SQL 槽位需要一个 bot 身份才会装配。
所以 `daily-reset` 只做「记一行 + 提醒一声」，真正的数据清理留给下一次消息或一次显式的 `exec`。

## 跑起来

```bash
cargo run -q -p hbot-cli -- check examples/game
```
