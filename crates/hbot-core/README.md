# hbot-core

> 领域模型、端口 trait、错误与 ID。整个 workspace 的根依赖，**不做 IO、不依赖任何其它 hbot crate**。

## 概述

这是唯一一处放"规则"的地方：能力参数的上下限、错误码、ID newtype、时钟与 UUID 的注入端口。
其它 crate 通过它共享判据，而不是各自抄一份 —— 抄出来的第二份判据一定会和第一份打架（`AGENTS.md` 坑 48）。

典型用法：`crates/hbot-code/src/manifest.rs` 解析清单后调用这里的 `validate_specs`；
`crates/hbot-rpc/src/app/runtime/*.rs` 把这里的 trait 当成端口来装配实现。

## 目录

- [关键文件](#关键文件)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [扩展点](#扩展点)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/domain.rs` | 实体：`Project` / `Bot``Conversation` / `Message` / `Session` 等 |
| `src/ids.rs` | 全部 ID newtype（`ProjectId``BotId``JobId``WaitId`…），解析与展示只有一份 |
| `src/ports.rs` | 端口 trait：`Clock` / `IdGenerator` / `CommandRunner` / `BotState` / `HistoryReader`… |
| `src/errors.rs` | `HbotError` 与 RPC 错误码映射（`code()`） |
| `src/command.rs` | 命令执行的能力矩阵、沙箱档、参数与结果类型 |
| `src/device.rs` | 设备能力的授权判据（`DeviceGate`）、能力表、参数闸 |
| `src/debug.rs` | 断点 / 日志点两张表（`Breakpoints`）与 `DebugSink` |
| `src/redact.rs` | 日志与错误里的脱敏（密钥、token、URL 凭据） |
| `src/paths.rs` | 数据目录、项目内 `.hbot/` 的唯一算法（`data_dir()`） |
| `src/bot_state.rs` | `C1.kv`：键值、TTL、批量写与上限 |
| `src/bot_ai.rs` | `C2.ai`：一次补全请求的契约与预算 |
| `src/bot_egress.rs` | `C3.send`：发消息与**附件**（`SendRequest` / `AttachmentRequest` / `AttachmentRef`） |
| `src/bot_file.rs` | `C3.file`：读写、目录、配额判据（`check_quota`） |
| `src/bot_jobs.rs` | `C4.jobs`：作业声明、重试退避、状态机 |
| `src/bot_timer.rs` | `C5.timer`：定时计划与到点结算 |
| `src/bot_session.rs` | `C2.session`：等待注册、作用域匹配、超时 |
| `src/bot_sql.rs` | `C7.sql` 的语句闸（只允许单条、拒 DDL 之外的写形状） |
| `src/bot_webhook.rs` | `C1.webhook`：入站端口与鉴权契约 |
| `src/bot_lifecycle.rs` | 生命周期钩子的阶段与原因（`onStart` / `onStop` / `crash`） |
| `src/base64.rs` | 唯一的 base64 编码器（沙箱与适配器共用） |
| `src/lib.rs` | 模块表与再导出 |

## 边界：能放什么 / 不能放什么

- **能放**：实体、值类型、端口 trait、错误、纯函数判据、默认实现 `SystemClock`（只读系统时钟）。
- **不能放**：任何文件 / 网络 / SDK 访问；任何对 workspace 内其它 crate 的依赖；任何 UI 契约（那是 `hbot-rpc/src/dto/`）。
- 新增一个能力时，**规则与参数闸必须先落在这里**（带单测），下游只调用不重写（`AGENTS.md` §8）。

## 上游与下游

- 上游：无（只依赖 `serde` / `thiserror` / `uuid`）。
- 下游：**所有** crate 都依赖它。所以这里加一个 pub 项等于给全仓加一个稳定 API，改动前先想清楚名字。

## 测试

`cargo test -p hbot-core` —— 单元测试与实现同文件（`#[cfg(test)] mod tests`），另有 `tests/` 下两条跨文件用例。
规则：判据函数的边界值（下界、上界、上界+1）都要有用例，`threshold` 类常量被改时测试要能红。

## 扩展点

见 `AGENTS.md` §8"新增清单能力"：① core 定义规则与参数闸 → ② `hbot-code` 的 `Capabilities` 加字段 → ③ 槽位 → ④ `hbot-rpc` 装配 → ⑤ 端到端测试。

## 已知限制

- 没有任何持久化：重启后存活的东西一律在 `hbot-store`。
- 端口 trait 的形状由实现方倒逼过几次（例如 `JobQueue` 要能阻塞写库），改 trait 时先跑 `cargo check --workspace --all-targets` 看要同步改多少处。
