# hbot-command

> 在受控条件下启动子进程、限制它，并在超时 / 超限时杀掉**整棵进程树**。

## 概述

这个 crate 只有一个目的：实现 `hbot_core::ports::CommandRunner`。它不认识 Agent、工具、审批 —— 只回答"这台机器的这个平台上，这条命令能不能跑；能跑的话怎么保证跑完或被杀干净"。

## 目录

- [关键文件](#关键文件)
- [诚实的能力矩阵](#诚实的能力矩阵)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/runner.rs` | `CommandRunner` 实现：建进程 → 装限额 → 等待 → 超时杀树 |
| `src/capability.rs`（`Runner::capability`） | 运行时探测：这台机器能装哪两层隔离 |
| `src/containment_win.rs` | Windows：Job Object（进程树 + 资源上限） |
| `src/containment_linux.rs` | Linux：cgroup v2 / rlimit 限额 |
| `src/containment_unsupported.rs` | 其它平台：明确拒绝，不静默降级 |
| `src/confinement_win.rs` / `src/landlock.rs` | 写隔离（Windows 用受限令牌 / ACL；Linux 用 Landlock） |
| `src/limits.rs` | CPU / 内存 / 进程数 / IO 四条限额，装不上就报错 |
| `src/probe.rs` | 启动时探测各原语是否可用（结果反映到能力矩阵） |
| `src/program.rs` / `src/path.rs` | 程序白名单、路径规范化与 realpath |
| `src/output.rs` | 输出截断与 `ResourceUsage` |
| `src/env.rs` | 子进程环境变量清洗（不继承密钥） |
| `src/cache.rs` | 能力探测结果缓存 |

## 诚实的能力矩阵

两层隔离是**分开的两件事**：`Containment` 管"跑飞了要能杀干净"，`WriteConfinement` 管"写不出工作区"。
判据：`read-only` 一律拒绝（这一层不提供只读视图）、`workspace-write` 要求写隔离、`danger` 只要 Containment。
装不上隔离就拒绝执行，**不许回退成名字白名单**（`AGENTS.md` 坑 7、坑 32、坑 52）。

## 边界：能放什么 / 不能放什么

- **能放**：进程创建、隔离、限额、杀树、输出截断。
- **不能放**：审批、工具语义、Agent 决策。

## 上游与下游

- 上游：`hbot-core`。
- 下游：`hbot-agent`（shell 工具）、`hbot-parity`、`hbot-redteam`。

## 测试

`cargo test -p hbot-command`：能力矩阵、限额装不上时的拒绝、超时杀树（子进程要真的生孙进程再验）。
Linux 侧只有交叉编译与纯逻辑单测的证据 —— 本机没有 Linux 内核，别在文档里写成"验证过"（坑 49）。

## 已知限制

- macOS 无实现：能力矩阵如实报"没有"，不假装有。
- 没有沙箱内文件系统视图（只做拒绝，不做虚拟化）。
