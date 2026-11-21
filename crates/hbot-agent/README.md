# hbot-agent

> 唯一的 Agent 循环、工具网关、审批与 provider 抽象。

## 概述

只做 Agent 决策与工具编排：不给子代理写第二套循环（`AGENTS.md` §0.3），不定义 UI 契约，不碰平台协议。

## 目录

- [关键文件](#关键文件)
- [闸门顺序](#闸门顺序)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/loop.rs` | **唯一的** Agent 循环，带步数 / token / 时长预算 |
| `src/gateway.rs` | 工具网关：固定闸门顺序与审计 |
| `src/approvals.rs` | 审批规则键 `(tool, 规范化参数, sandbox_mode)` 与策略存储 |
| `src/edits.rs` | 待审编辑：Agent 写入产出 Diff，逐块接受 / 拒绝 |
| `src/tools/` | 工具注册表与实现：`fs`（读/写/编辑/列目录）、`search`、`shell`、`http`、`device` |
| `src/provider/` | provider 抽象 + 两个 SSE 客户端（OpenAI 兼容、Anthropic Messages）+ 真实 HTTP 传输 |
| `src/prompt.rs` | 系统提示与工具描述拼装 |
| `src/cancel.rs` | 取消贯穿 provider / 工具 / HTTP / 命令 |

## 闸门顺序

参数 → 存在 → 沙箱档 → 路径 realpath → 权限 → 审批 → 执行 → 审计。禁止"后台特批"，所有命令走同一个 `CommandRunner`。

## 边界：能放什么 / 不能放什么

- **能放**：Agent 决策、工具、审批、provider。
- **不能放**：Bot 生命周期、平台协议、UI DTO、第二套循环。

## 上游与下游

- 上游：`hbot-core`、`hbot-workspace`、`hbot-command`、`hbot-code`。
- 下游：`hbot-rpc`（`app/agent/*` 是它的会话与审批接线）、`hbot-parity`、`hbot-redteam`。

## 测试

`cargo test -p hbot-agent`：工具与审批的拒绝路径是重点（无权限、路径逃逸、超时、取消）。
provider 用夹具 HTTP 服务测，不发真实请求（CI 不允许外网）。

## 已知限制

- 没有多轮对话历史压缩、没有流式工具调用协商之外的协议扩展。
- 同一时间一个 Agent 会话一条循环；并发靠多个会话而不是线程池。
