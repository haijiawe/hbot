# apps/web/src

> IDE 前端（React + TypeScript + Vite）。七个目录，各有明确归属。

## 目录

| 目录 | 放什么 | 不能放什么 |
|---|---|---|
| `app/` | 外壳：`App.tsx`、活动栏、状态栏、全局快捷键、核心事件订阅、错误边界 | 业务面板 |
| `components/` | 跨 feature 的通用组件（目前只有 `splitter.tsx`） | 某个 feature 专用组件 |
| `features/` | 13 个功能面板（含 bot 工作台 `workbench/`），一个目录一个面板，**各自有 README** | 跨 feature 的内部 import |
| `stores/` | 状态：`workspace` / `agent` / `run` 三个主 store，加 `debug` / `device` / `search` / `tryrun` | 直接调 IPC 之外的副作用 |
| `lib/` | IPC 客户端、生成的契约消费、纯函数工具（格式化、Diff 应用、路径、快捷键、布局） | React 组件 |
| `generated/` | Rust 契约的 TS 生成物（`cargo xtask gen` 产出） | **手改** |
| `styles/` | `app.css`（布局）与 `theme.css`（颜色 / 变量） | 组件内部写死的颜色值 |

## feature 目录一览

| 目录 | 一句话 |
|---|---|
| `features/agent` | Agent 面板、审批卡、Diff 卡 |
| `features/command-palette` | 命令面板与项目对话框 |
| `features/debug` | 断点 / 日志点 / 单步 |
| `features/device` | 设备能力与授权 |
| `features/editor` | Monaco 编辑区与标签 |
| `features/explorer` | 文件树 |
| `features/problems` | 诊断与快速修复 |
| `features/replay` | 回放 |
| `features/run` | 运行日志、作业、运行控制 |
| `features/search` | 全局搜索 |
| `features/settings` | 设置 |
| `features/tryrun` | 本地试跑 |
| `features/workbench` | bot 工作台六个面（概览 / Bot / 能力 / 会话 / 运行态 / 渠道） |

## 三条硬规则

1. **只经 `lib/ipc.ts` 的 `dispatch`**：不直接 `fetch` / WebSocket；纯浏览器开发走 `lib/ipc-mock.ts` 的内存后端，Tauri 走 `lib/ipc-tauri.ts`。
2. **三个 store 不许互相写**：`workspace`（文件 / 标签 / 布局）、`agent`（消息 / Diff / 审批）、`run`（bot 状态 / 日志）只经命令或事件协作。
3. **布局与快捷键只有一份**：数值从 `lib/layout.ts` 取，键名从 `lib/keybindings.ts` 取，组件里不写死。

## 构建与检查

`pnpm --dir apps/web run check`（= `tsc --noEmit` + biome），`pnpm --dir apps/web test`（vitest，49 文件）。
`dist/` 是构建产物，被 `crates/hbot-desktop` 的 `frontendDist` 引用 —— 所以 `cargo check --workspace --all-targets` 之前它必须存在。
