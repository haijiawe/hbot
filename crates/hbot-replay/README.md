# hbot-replay

> 用历史入站事件重跑**当前代码**并对比输出（`hbot replay` 与 IDE `replay_run` 的共用内核）。

## 概述

历史库由**调用方**打开并传进来，本 crate 不自己开库、也不推断路径：CLI 打开项目库 <<BT><project>/.hbot/hbot.db`，IDE 把 `AppState` 那份 `Arc<Store><` 传进来。
`ReplayOptions::database` 只是"这份历史来自哪个文件"的**文案**，不参与打开。

## 目录

- [关键文件](#关键文件)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/lib.rs` | `replay()`：按 `(received_at, rowid)` 升序读入站 → 起 isolate 重跑 → 比对出站 |
| `src/fixture.rs` | 夹具导出 / 导入（`hbot fixture`） |

## 边界：能放什么 / 不能放什么

- **能放**：重放流程、输出比对、报告结构。
- **不能放**：自己开库、拉 bot 起来、写 UI。

## 上游与下游

- 上游：`hbot-core`、`hbot-store`、`hbot-code`、`hbot-bot`、`hbot-workspace`。
- 下游：`hbot-cli`（`replay`）、`hbot-rpc`（`replay_run`）。

## 测试

`cargo test -p hbot-replay`：一条端到端（造历史 → 重放 → 比对）。

## 已知限制

- 只重放入站消息，不重放定时器 / 作业 / 等待（这些依赖真实时钟）。
- 比对是"出站文本 + 目标"，不做逐字节的富媒体比对。
