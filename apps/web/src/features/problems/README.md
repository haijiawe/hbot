# features/problems

> 诊断面板与快速修复。

## 职责

把编译 / 类型检查返回的诊断列表渲染出来，并把"快速修复"翻译成编辑动作。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `problems-panel.tsx` | 列表、按文件分组、跳转 |
| `problem-actions.ts` | 快速修复的可用性与应用 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `lib/ipc.ts` | `project.diagnostics` |
| `lib/diagnostic-text.ts` | 诊断文案与人话化 |
| `lib/quick-fix.ts` | 修复候选（只做确定性的替换） |

## 约定

- 诊断消息里**不许**出现项目根的绝对路径（`AGENTS.md` 坑 47）。
- 快速修复只允许确定性的文本替换；需要"推断意图"的修复宁可不做。

## 测试

`problems-panel.test.tsx` / `problems-actions.test.tsx` / `lib/quick-fix.test.ts`。
