# hbot-rpc

> UI 协议与应用层入口：Command/Event DTO、dispatch、WS 服务，以及所有能力的宿主侧装配。

## 概述

桌面壳、无头 `hbot serve`、CLI 都进同一个 `Dispatcher::dispatch` —— 所以"桌面能做的事后台做不了"这种分叉在本项目里不存在。
契约唯一真源是 `src/dto/`（`command.rs` / `event.rs` / `common.rs`），TS 由 `src/ts/` 生成到 `apps/web/src/generated/`（`cargo xtask gen`）。

## 目录

- [关键文件](#关键文件)
- [能力是怎么装配起来的](#能力是怎么装配起来的)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [扩展点](#扩展点)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/dto/command.rs` / `event.rs` / `common.rs` | 唯一契约：命令、事件、公共类型 |
| `src/dispatch.rs` | 唯一入口：鉴权后的命令分发与错误映射 |
| `src/ws.rs` | 后台模式：auth、限流、背压、`event_sink` |
| `src/app/mod.rs` | `AppState::open`：数据目录、`Store`、`EventBus`、bot 注册表、shutdown |
| `src/app/commands/` | 每组命令一个文件：`bots` / `files` / `agent` / `debug` / `device` / `replay` / `system` |
| `src/app/runtime/sandbox_handler.rs` | 最大的一处：把消息 / 定时器 / 作业 / 等待恢复派发进 isolate，并回收回复与日志 |
| `src/app/runtime/{send,state,ai,jobs,waits,sql,files,net}.rs` | 每个能力的宿主端口实现（相对路径 → 项目根内 → 落库 / 入队） |
| `src/app/runtime/{onebot,qq,local}.rs` | `BotAdapter` 的具体实现（`local` 把回复写进 run log） |
| `src/app/runtime/fingerprint_cache.rs` | 项目指纹（含 `hbot.json`）与热重载节流 |
| `src/app/device/` | 设备能力的判据实现：`DeviceGate`、`capabilities`、`grants`、`settings` |
| `src/webhook.rs` | 入站 HTTP 端口（`C1.webhook`） |

## 能力是怎么装配起来的

`AppState::start_bot` → `SandboxHandlerConfig` → `SandboxHandler::new`（**每个字段只能从 config 取**，写 `None` 会静默丢掉能力）→ `worker()` 里调 `sandbox.set_*`。
新增能力要同批改五处：`SandboxHandlerConfig` 字段 / `new()` / `worker()` / `start_bot` 字面量 / `crates/hbot-parity/tests/support/mod.rs` 字面量（`AGENTS.md` 坑 66）。

## 边界：能放什么 / 不能放什么

- **能放**：参数校验、编排、端口实现、错误映射。
- **不能放**：业务规则（判据在 `hbot-core`）、UI 组件、平台协议细节（`hbot-adapters`）。

## 上游与下游

- 上游：几乎所有实现 crate（core/store/workspace/code/bot/adapters/agent/command/device/replay）。
- 下游：`hbot-desktop`（Tauri 壳）、`hbot-cli`（`serve`）。

## 测试

34 个 test target，是本仓最多的一处：`tests/*_capability.rs` 按能力分文件（`send` / `jobs` / `session` / `timer` / `state` / `crash`…），每个文件自带一套 TempDir + `AppState::open` + `Dispatcher` 夹具。
端到端测试要断言"真的派发到了 isolate"，所以失败往往先表现为超时而不是断言失败；`wait_for` 的轮询上限要留够。

## 扩展点

新增 Command 见 `AGENTS.md` §8：`dto/command.rs` → `commands/*` 实现 → `dispatch.rs` 分支 → `cargo xtask gen` → web 消费点。

## 已知限制

- 单进程：没有跨进程 bot 协调。
- 桌面与无头共用一套状态，因此同一数据目录不能被两个进程同时打开写（SQLite 锁会挡住，但不是优雅降级）。
