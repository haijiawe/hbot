# features/run

> 底部面板：运行日志（含筛选）、运行控制、试跑入口。

## 职责

把"这个 bot 正在做什么"丢给用户看：日志流（级别 + 关键词筛选、跟随底部）、
启动 / 停止按钮，以及本地试跑页签的容器。

**作业与等待不在这里**（2026-11-21 UI 收敛）：它们是整页面「作业看板」
（`features/workbench/jobs-board-view.tsx`）的内容，底部曾经并存的「作业」页签已删 ——
同一份 `bot_jobs` / `bot_waits` 快照在两个面上各渲染一次，用户要两处都看才敢下判断。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `bottom-panel.tsx` | 页签容器（日志 / 问题 / 搜索 / 调试 / 试跑 / 设备 / 回放） |
| `run-log.tsx` | 日志流（筛选工具栏、跟随底部、窗口化只渲染可视区） |
| `run-log-virtual.ts` | 兼容层：窗口数学已搬到 `lib/log-window.ts`，这里只 re-export |
| `run-controls.tsx` | 启动 / 停止 / 重启 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/run-store.ts` | 日志（有上限）、bot 状态 |
| `lib/ipc.ts` | `bot.start` / `bot.stop` / `bot.logs` |

## 约定

- 日志在 store 里有硬上限（旧行被丢弃），不要在下游再"保留全部"。
- 窗口化只保证渲染量，不改变"日志已经被截断"这一事实；行高固定是窗口化的前提。
- 筛选是**纯前端**的：`lib/log-filter.ts` 的 `filterLogs` 只过滤已到 store 的行，
  不要把筛选下推成一条新的后端查询（那会让"清掉筛选"要再等一次往返）。
- 空状态要能解释"为什么没日志"（没启动 / 没有 stdout / 过滤太严）。

## 测试

`run-log.test.tsx` / `run-log-virtual.test.ts` / `bottom-panel.test.tsx`。
