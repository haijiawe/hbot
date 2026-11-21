# features/run

> 底部面板：运行日志、作业面板、运行控制、试跑入口。

## 职责

把"这个 bot 正在做什么"丢给用户看：日志流、作业队列（`C4.jobs`）、启动 / 停止按钮，以及本地试跑页签的容器。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `bottom-panel.tsx` | 页签容器（日志 / 作业 / 试跑 / 问题 / 回放） |
| `run-log.tsx` | 日志流（级别过滤、跟随底部、自动滚动） |
| `run-log-virtual.ts` | 虚拟滚动的可见窗口计算（纯函数，可测） |
| `jobs-panel.tsx` | 作业列表：状态、尝试次数、下次重试时间、错误 |
| `run-controls.tsx` | 启动 / 停止 / 重启 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/run-store.ts` | 日志（有上限）、bot 状态、作业行 |
| `lib/ipc.ts` | `bot.start` / `bot.stop` / `bot.logs` / `jobs.list` |

## 约定

- 日志在 store 里有硬上限（旧行被丢弃），不要在下游再"保留全部"。
- 虚拟滚动只保证渲染量，不改变"日志已经被截断"这一事实。
- 空状态要能解释"为什么没日志"（没启动 / 没有 stdout / 过滤太严）。

## 测试

`run-log.test.tsx` / `run-log-virtual.test.ts` / `jobs-panel.test.tsx` / `bottom-panel.test.tsx`。
