/**
 * 发给 Agent 的请求文本（`lib/agent-requests.ts`）。
 *
 * 这些字符串就是用户与模型之间的全部协议，所以逐条钉住"必须出现什么"：
 * 文件相对路径、行范围、选区原文、指令、错误码 —— 少一项模型就只能猜位置，
 * 猜错之后出现在 `diff-card` 里的 hunk 也是错的。有未保存改动时额外要求带提示
 * （docs/06-ide.md §5.2：Diff 以磁盘为基准）。
 */

import { describe, expect, it } from 'vitest'
import {
  AGENT_DIFF_RULE,
  inlineEditPrompt,
  problemFixBatchPrompt,
  problemFixPrompt,
  unsavedBufferNote,
} from './agent-requests'
import type { Diagnostic } from './ipc'

function diagnostic(patch: Partial<Diagnostic> = {}): Diagnostic {
  return {
    path: 'src/plugins/ping.ts',
    line: 12,
    column: 7,
    severity: 'error',
    message: "bare module specifier 'lib/util' is not allowed",
    code: 'HBOT_BARE_IMPORT',
    ...patch,
  }
}

describe('unsavedBufferNote', () => {
  it('没有脏文件时返回 null（空提示比错提示更糟）', () => {
    expect(unsavedBufferNote([])).toBeNull()
  })

  it('有脏文件时点名路径，并说明以磁盘为基准', () => {
    const note = unsavedBufferNote(['src/a.ts', 'src/b.ts'])
    expect(note).toContain('src/a.ts、src/b.ts')
    expect(note).toContain('未保存')
    expect(note).toContain('磁盘')
  })
})

describe('inlineEditPrompt（Ctrl/Cmd+K）', () => {
  const base = {
    path: 'src/plugins/ping.ts',
    startLine: 3,
    endLine: 5,
    text: 'function ping() {\n  return true\n}',
    instruction: '改成早返回',
    dirty: false,
  }

  it('带上文件相对路径、行范围与选区原文', () => {
    const prompt = inlineEditPrompt(base)
    expect(prompt).toContain('src/plugins/ping.ts')
    expect(prompt).toContain('第 3–5 行')
    expect(prompt).toContain('function ping() {')
    expect(prompt).toContain('改成早返回')
  })

  it('只改这一段的约束与 Diff 交付方式都在里面（否则模型会顺手改别的文件）', () => {
    const prompt = inlineEditPrompt(base)
    expect(prompt).toContain(AGENT_DIFF_RULE)
    expect(prompt).toContain('这个范围之外一个字都别动')
    expect(prompt).toContain('Diff')
  })

  it('单行选区写成「第 N 行」，不写成区间', () => {
    const prompt = inlineEditPrompt({ ...base, startLine: 9, endLine: 9 })
    expect(prompt).toContain('第 9 行')
    expect(prompt).not.toContain('第 9–9 行')
  })

  it('有未保存改动时附上缓冲区提示', () => {
    const prompt = inlineEditPrompt({ ...base, dirty: true })
    expect(prompt).toContain('未保存')
    expect(prompt).toContain('src/plugins/ping.ts')
  })

  it('没有未保存改动时**不**附提示（避免每次请求都喊狼来了）', () => {
    expect(inlineEditPrompt(base)).not.toContain('未保存')
  })
})

describe('problemFixPrompt（请求 Agent 修复一条）', () => {
  it('file / line / column / code / message 全部出现', () => {
    const prompt = problemFixPrompt(diagnostic())
    expect(prompt).toContain('src/plugins/ping.ts')
    expect(prompt).toContain('src/plugins/ping.ts:12:7')
    expect(prompt).toContain('HBOT_BARE_IMPORT')
    expect(prompt).toContain("bare module specifier 'lib/util' is not allowed")
    expect(prompt).toContain('错误')
  })

  it('项目级问题说清"没有具体文件"，不留下空白的文件行', () => {
    const prompt = problemFixPrompt(diagnostic({ path: '', line: 0, column: 0, code: null }))
    expect(prompt).toContain('项目级（没有具体文件）')
    expect(prompt).toContain('（核心没给）')
  })

  it('脏文件列表非空时附上未保存提示', () => {
    const prompt = problemFixPrompt(diagnostic(), ['src/plugins/ping.ts'])
    expect(prompt).toContain('未保存')
  })
})

describe('problemFixBatchPrompt（批量修复）', () => {
  const items = [
    diagnostic({ path: 'src/a.ts', line: 1, column: 1, message: '第一个' }),
    diagnostic({ path: 'src/b.ts', line: 2, column: 3, message: '第二个' }),
  ]

  it('逐条编号列出，序号与面板顺序一致', () => {
    const prompt = problemFixBatchPrompt(items)
    expect(prompt).toContain('1. src/a.ts:1:1')
    expect(prompt).toContain('2. src/b.ts:2:3')
    expect(prompt).toContain('第一个')
    expect(prompt).toContain('第二个')
    expect(prompt).toContain('2 条错误')
  })

  it('每条都带错误码（模型据此判断该不该动这一处）', () => {
    expect(problemFixBatchPrompt(items)).toContain('HBOT_BARE_IMPORT')
  })

  it('脏文件列表非空时附上未保存提示', () => {
    expect(problemFixBatchPrompt(items, ['src/b.ts'])).toContain('src/b.ts')
  })
})