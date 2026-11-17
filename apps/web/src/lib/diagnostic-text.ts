/**
 * 诊断的文案：严重级标签 + 位置串。**唯一真源**。
 *
 * 为什么单独一份：同一个诊断要出现在三个地方 —— 问题面板的行（展示）、
 * 行内错误提示、以及发给 Agent 的请求文本（`lib/agent-requests.ts`）。三处各写一遍
 * 就会出现"面板写「（项目级）」、请求里写「无」"这类漂移，而模型看到的上下文一旦和
 * 用户看到的不一致，它改的位置就会错。
 *
 * 两种位置格式刻意保留两份语义，不是重复：
 * - `displayLocation`：给人扫一眼的短串（`src/a.ts:4` / `src/a.ts` /「（项目级）」）；
 * - `agentLocation`：给模型定位的完整串（带列号；项目级会明说"没有具体文件"）。
 */

import type { Diagnostic } from './ipc'

/** 严重级 → 中文标签；键是全集，漏一个 `tsc` 就会红（不给默认值兜底）。 */
export const SEVERITY_TEXT: Record<Diagnostic['severity'], string> = {
  error: '错误',
  warning: '警告',
  info: '信息',
  hint: '提示',
}

/** 展示用：`path:line` 或 `path`（位置未知）/「（项目级）」。 */
export function displayLocation(item: Diagnostic): string {
  if (item.path === '') return '（项目级）'
  if (item.line <= 0) return item.path
  return `${item.path}:${item.line}`
}

/** 请求用：`path:line:column` / `path`（位置未知）/「项目级（没有具体文件）」。 */
export function agentLocation(item: Diagnostic): string {
  if (item.path === '') return '项目级（没有具体文件）'
  if (item.line <= 0) return item.path
  if (item.column > 0) return `${item.path}:${item.line}:${item.column}`
  return `${item.path}:${item.line}`
}