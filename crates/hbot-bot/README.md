# hbot-bot

> 平台无关的 bot 运行时：supervisor、每会话队列、outbox、入站去重。

## 概述

把"适配器送来一条事件"变成"处理器按会话串行执行一次、回复按序发出"。它不认识 OneBot，也不认识 QuickJS —— 那两边分别是 `hbot-adapters` 与 `hbot-code`。

## 目录

- [关键文件](#关键文件)
- [主链路与两条硬约束](#主链路与两条硬约束)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/supervisor.rs` | 起停、读循环、状态机、`ReadLoopExit`（读循环意外结束的信号） |
| `src/queue.rs` | 每 `(bot, conversation)` 一条有界串行队列；满时暂停读取，不丢 |
| `src/outbox.rs` | 出站重试、退避、死信、重启续发（带附件时读回 `attachment`） |
| `src/dedupe.rs` | 入站去重（执行层的 at-least-once 兜底） |
| `src/handler.rs` | `MessageHandler` 端口：谁来实现"跑一次 handler" |
| `src/adapter.rs` | `BotAdapter` / `OutboundEnvelope` 协议边界 |
| `src/event.rs` / `src/event_json.rs` | 平台事件到统一事件形状的转换辅助 |
| `src/bot.rs` | 面向调用方的薄封装（`Bot::snapshot` 等） |
| `src/store_ops.rs` | 落库操作用的 trait 形状 |

## 主链路与两条硬约束

`BotAdapter::subscribe` → 读循环 → `inbox.admit` → 每会话队列 → handler → outbox → 平台发送 → `mark_processed`。

1. **每会话串行**：同一 `(bot_id, conversation_id)` 不并发，不同会话可并行。
2. **队列有界**：满时暂停读取而不是丢弃；唤醒要按会话通知，不能只在"全局归零"时通知（`AGENTS.md` 坑 20）。

## 边界：能放什么 / 不能放什么

- **能放**：调度、串行化、去重、重试、崩溃恢复。
- **不能放**：平台协议、QuickJS、Agent 循环、SQL（走 `hbot-store`）。

## 上游与下游

- 上游：`hbot-core`、`hbot-store`。
- 下游：`hbot-rpc`（`AppState` 用它起停 bot）、`hbot-cli`、`hbot-replay`、`hbot-parity`、`hbot-redteam`。

## 测试

`cargo test -p hbot-bot`：`tests/runtime.rs`（起停、串行、崩溃信号）、`tests/outbox_exhaustion.rs`（重试到死信）。
需要适配器的用例用 `tests/common/mod.rs` 的 `TestAdapter`；`close_subscription()` 用来造"订阅被关掉"。

## 已知限制

- 不承诺 exactly-once：执行层是 at-least-once，幂等键是 `ctx.messageId`，有副作用的代码要自己判重。
- 没有跨进程协调：同一 bot 被两个进程拉起会互相抢消息，这属于部署错误而不是运行时能修的问题。
