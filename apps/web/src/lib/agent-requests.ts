/**
 * 发给 Agent 的请求文本：行内编辑（Cmd/Ctrl+K）与问题面板的「请求 Agent 修复」。
 *
 * 为什么要集中一份：两条入口的骨架是同一件事 —— 文件、位置、原文/消息，再加一句
 * 「用 Diff 交付」和「缓冲区有未保存改动」的提示。拼在各组件里就会变成两份，改一份
 * 忘一份，模型收到的上下文就开始漂。这里全是纯函数，单测直接断言字符串。
 *
 * 交付方式由既有链路决定（docs/06-ide.md §5.2 / §5.3）：模型给出的改动走
 * `agent.diff` → `features/agent/diff-card.tsx` 逐块审阅，`edit_apply` 才写盘。
 * 所以请求里必须要求模型**只动说到的这一处**，并且不要自己宣布"已修改"。
 */

import { SEVERITY_TEXT, agentLocation } from './diagnostic-text'
import type { Diagnostic } from './ipc'

/** 所有请求共用的交付约定：一句写死，别在每个拼接处各写一遍。 */
export const AGENT_DIFF_RULE =
  '请只改这里说到的部分，不要顺手改别的文件，也不要直接宣布完成；改动用 Diff 的形式给出，我逐块审阅后自己应用。'

/**
 * 「缓冲区 ≠ 磁盘」的提示（docs/06-ide.md §5.2）。
 *
 * 没有脏文件时返回 `null`，调用方据此决定要不要印 —— 空提示比错误提示更糟：
 * 用户会以为"没提示 = 没有未保存改动"。
 */
export function unsavedBufferNote(paths: readonly string[]): string | null {
  if (paths.length === 0) return null
  return (
    `注意：${paths.join('、')} 在编辑器里有未保存改动（缓冲区 ≠ 磁盘）。` +
    '生成 Diff 时以磁盘内容为基准，不要假设你能看到我还没保存的字。'
  )
}

export interface InlineEditTarget {
  /** 文件相对路径（与 `workspace-store` 的 path 同形）。 */
  path: string
  /** 1-based，闭区间；单行时两者相等。 */
  startLine: number
  endLine: number
  /** 选区原文（整行切片，行间用 `\n`）。 */
  text: string
}

export interface InlineEditRequest extends InlineEditTarget {
  instruction: string
  /** 该文件在编辑器里有未保存改动。 */
  dirty: boolean
}

function lineRange(startLine: number, endLine: number): string {
  return startLine === endLine ? `第 ${startLine} 行` : `第 ${startLine}–${endLine} 行`
}

/** 行内编辑的请求文本：文件 + 选区范围 + 选区原文 + 用户指令（+ 未保存提示）。 */
export function inlineEditPrompt(request: InlineEditRequest): string {
  const range = lineRange(request.startLine, request.endLine)
  const lines = [
    `请只改 ${request.path} 的${range}（行内编辑），这个范围之外一个字都别动。`,
    AGENT_DIFF_RULE,
    '',
    `文件：${request.path}`,
    `选区：${range}`,
    '',
    '选区原文：',
    '```',
    request.text,
    '```',
    '',
    `要求：${request.instruction}`,
  ]
  const note = request.dirty ? unsavedBufferNote([request.path]) : null
  if (note !== null) lines.push('', note)
  return lines.join('\n')
}

/** 单条问题的请求文本：file / line / code / message 都要给，模型才知道改哪一行。 */
export function problemFixPrompt(item: Diagnostic, dirtyPaths: readonly string[] = []): string {
  const lines = [
    `问题面板报了一条${SEVERITY_TEXT[item.severity]}，请把它修掉。`,
    AGENT_DIFF_RULE,
    '',
    `文件：${item.path === '' ? '项目级（没有具体文件）' : item.path}`,
    `位置：${agentLocation(item)}`,
    `级别：${item.severity}（${SEVERITY_TEXT[item.severity]}）`,
    `错误码：${item.code ?? '（核心没给）'}`,
    `消息：${item.message}`,
  ]
  const note = unsavedBufferNote(dirtyPaths)
  if (note !== null) lines.push('', note)
  return lines.join('\n')
}

/** 批量修复的请求文本：一次把当次诊断里的全部 error 说清楚，序号与面板顺序一致。 */
export function problemFixBatchPrompt(
  items: readonly Diagnostic[],
  dirtyPaths: readonly string[] = [],
): string {
  const lines = [
    `问题面板里有 ${items.length} 条${SEVERITY_TEXT.error}，请逐条修掉。`,
    AGENT_DIFF_RULE,
    '',
    '待修的问题：',
    ...items.map(
      (item, index) =>
        `${index + 1}. ${agentLocation(item)} ${item.code ?? '(无错误码)'} —— ${item.message}`,
    ),
  ]
  const note = unsavedBufferNote(dirtyPaths)
  if (note !== null) lines.push('', note)
  return lines.join('\n')
}