# features/replay

> 回放面板：用历史入站事件重跑当前代码并对比输出。

## 职责

选择 bot 与时间范围 → 调 `replay.run` → 展示每条入站的"原来回了什么 / 现在回了什么 / 是否一致"。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `replay-panel.tsx` | 控制区 + 结果表（差异高亮） |
| `replay-entry-row.tsx` | 单条结果行（原输出 / 新输出 / 判定） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `lib/ipc.ts` | `replay.run` / `replay.cancel` |
| `lib/ipc-mock.ts` | 纯浏览器模式下用内存夹具回放 |

## 约定

- 回放是**离线**的：不拉 bot 起来、不写真库（Rust 侧读的是同一份历史，但引擎是独立的 isolate）。
- 比对口径是"出站文本 + 目标"，不是逐字节富媒体比对。

## 测试

`replay-panel.test.tsx` 与 `lib/ipc-mock.replay.test.ts`。
