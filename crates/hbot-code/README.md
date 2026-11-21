# hbot-code

> 机器人代码沙箱：TypeScript 进，QuickJS 出。编译、清单、宿主 API、隔离与热重载全在这里。

## 概述

一条链路：`manifest` 解析 `hbot.json` → `compile`（SWC 去类型、保留行映射）→ `modules` 解析 import（只允许 `hbot` 与项目内相对路径）→ `runtime` 在一个独立线程的 QuickJS isolate 里跑 handler，宿主能力通过 promise 桥接。
`dts.rs` 从同一份契约生成项目的 `hbot.d.ts`：声明与运行时不符是本项目最贵的一类 bug（`AGENTS.md` 坑 42）。

## 目录

- [关键文件](#关键文件)
- [能力的槽位模式](#能力的槽位模式)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [扩展点](#扩展点)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/runtime/isolate.rs` | 最大的一个文件（约 3200 行）：isolate 生命周期、`Command` / `EventOutcome`、`build_event` 与每种事件各自的 ctx |
| `src/manifest.rs` | `hbot.json` 的解析、校验（`deny_unknown_fields`）与新项目脚手架 |
| `src/compile.rs` | SWC 编译、行映射、按内容哈希缓存 |
| `src/dts.rs` | 生成 `hbot.d.ts`；含"声明与运行时必须一致"的漂移测试 |
| `src/runtime/host_api.rs` | 宿主绑定表（`HOST_BINDING_KEYS`）与 `hbot` 模块的 JS 源码 |
| `src/runtime/send_api.rs` 等 | 每个能力一个桥接模块（`send` / `state` / `ai` / `jobs` / `session` / `file` / `sql` / `device` / `debug` 各一份，JS 函数 → 槽位） |
| `src/send.rs` / `state.rs` / `ai.rs` / `jobs.rs` / `session.rs` / `file.rs` / `sql.rs` / `device/` | 端口 trait + 槽位（可替换实现 + 声明闸 + 装配闸） |
| `src/permissions.rs` | `capabilities` / `permissions` 的判据（网络授权只有这一份） |
| `src/instrument.rs` | 调试探针 `__hbot_trace`（只有 debug 编译才插，见坑 37） |
| `src/limits.rs` | isolate 的时间 / 内存 / 调用上限 |
| `src/error.rs` | `HBOT_*` 错误码的装配与 JS 侧抛错形状 |

## 能力的槽位模式

每个能力（`send` / `kv` / `ai` / `jobs` / `session` / `file` / `sql` / `device`）都是同一个形状：
**端口 trait**（在 `hbot-code`）+ **槽位**（`Mutex` 里的可替换实现 + `set_*` / `is_*_assembled`）+ **声明闸**（每次重读清单，不缓存）。
两道闸分开报码：`HBOT_*_NOT_DECLARED` = 清单没写，`HBOT_*_UNAVAILABLE` = 宿主没装配（`AGENTS.md` 坑 59）。

## 边界：能放什么 / 不能放什么

- **能放**：编译、清单、QuickJS 运行时、宿主 API 桥、能力槽位。
- **不能放**：平台协议（`hbot-adapters`）、DB（`hbot-store`）、Agent 循环（`hbot-agent`）、UI 契约（`hbot-rpc/src/dto`）。
- 生成代码没有 `fs` / `net` / `process` / `require`，只有 `hbot.*` 白名单；不要为了"方便"开一个通用桥。

## 上游与下游

- 上游：`hbot-core`（契约、错误、ID）。
- 下游：`hbot-rpc`（`sandbox_handler` 装配与派发）、`hbot-agent`、`hbot-cli`（`check`）、`hbot-replay`、`hbot-bench`。

## 测试

`cargo test -p hbot-code`：单测在文件内，端到端在 `tests/`（`s1_isolate_lifecycle` / `s2_ai` / `s3_device` 等）。
测试进程退出时崩在 QuickJS GC 断言上，几乎总是把 `Ctx` 存进了 `Async` 闭包（坑 53）。

## 扩展点

新增宿主 API 必须四处同批改，少一处 `dts.rs` 的漂移测试就红：`HOST_BINDING_KEYS` → `install()` 的 `host.set` → `HOST_MODULE_SOURCE` → `HOST_DTS`，再进 `build_event` 的 `event.set`。
详见 `AGENTS.md` §8"新增宿主 API"。

## 已知限制

- 只有 QuickJS：不支持 Node 内建模块，也没有 npm 运行时依赖（产品硬规则）。
- 同一会话不并发：一次事件一个 isolate 任务，跨会话并行靠多个 worker。
- 热重载是整份重建 isolate，不是增量替换模块。
