# hbot-cli

> `hbot` 二进制：`version` / `serve` / `check` / `replay` / `fixture` / `token`。

## 概述

只做参数解析、进程生命周期与输出；协议与传输在 `hbot-rpc`。
依赖面刻意收窄：**不**依赖 `hbot-bot` / `hbot-agent`（那些由 `AppState` 装配，CLI 只拿到装好的 `Dispatcher`）。两个例外是 `hbot-code`（`check` 要真编译、`replay` 要真起 isolate）与 `hbot-store`（`replay` 只读查历史）。

## 目录

- [关键文件](#关键文件)
- [输出约定](#输出约定)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/cli.rs` | 子命令定义与参数解析 |
| `src/serve.rs` | 无头宿主：起 `AppState`、开 WS、`forward_events` 把业务事件接到 WS 出口、等退出信号 |
| `src/check.rs` | `hbot check`：读清单 + 真跑一次 SWC 编译，输出 `{"ok":…,"errors":[…]}` |
| `src/replay.rs` | `hbot replay`：打开项目库、调 `hbot-replay`、输出报告 |
| `src/fixture.rs` | 夹具导出 / 导入 |
| `src/token.rs` | `hbot token`：生成 / 查看 WS 访问令牌 |
| `src/version.rs` | 版本与构建信息 |

## 输出约定

stdout **只写数据**（`version` 文本、`serve` 的 ready JSON、`check` 的 JSON、`replay` 报告）；日志与告警一律 `tracing` 走 stderr。
这条约定被 `scripts/soak-serve.ps1` 与桌面探针依赖，改输出格式等于改协议。

## 边界：能放什么 / 不能放什么

- **能放**：参数、进程生命周期、stdout/stderr 形态。
- **不能放**：命令实现、平台协议、业务规则。

## 上游与下游

- 上游：`hbot-rpc`、`hbot-adapters`、`hbot-code`、`hbot-replay`、`hbot-store`、`hbot-core`。
- 下游：无（它是二进制）。

## 测试

`cargo test -p hbot-cli`：4 个 target，含"示例项目能 check 通过""serve 的 ready 行形状""事件桥确实转发了业务事件"。

## 已知限制

- 没有 `run` 子命令：跑 bot 只在 IDE / 桌面里做（`hbot serve` 是宿主，不是"跑某个项目"）。
- Windows 上独立进程收不到 Ctrl-C，强杀不会走优雅停机（压测报告 S15 已注明这一点）。
