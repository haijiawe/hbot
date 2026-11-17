/**
 * 诊断文案（`lib/diagnostic-text.ts`）：面板、行内提示、Agent 请求三处共用同一份。
 *
 * 这里钉住的是**语义差别**：展示要短（`src/a.ts:4`），请求要能定位（带列号），
 * 项目级条目两种都不能假装知道位置。
 */

import { describe, expect, it } from 'vitest'
import { SEVERITY_TEXT, agentLocation, displayLocation } from './diagnostic-text'
import type { Diagnostic } from './ipc'

function diagnostic(patch: Partial<Diagnostic> = {}): Diagnostic {
  return {
    path: 'src/a.ts',
    line: 4,
    column: 2,
    severity: 'error',
    message: '出错了',
    code: 'HBOT_COMPILE',
    ...patch,
  }
}

describe('diagnostic-text', () => {
  it('严重级标签覆盖全部四种（键是全集，漏一个 tsc 会红）', () => {
    expect(SEVERITY_TEXT).toEqual({
      error: '错误',
      warning: '警告',
      info: '信息',
      hint: '提示',
    })
  })

  it('displayLocation：常规条目是 path:line', () => {
    expect(displayLocation(diagnostic())).toBe('src/a.ts:4')
  })

  it('displayLocation：位置未知只给路径，不写 ":0"', () => {
    expect(displayLocation(diagnostic({ line: 0 }))).toBe('src/a.ts')
  })

  it('displayLocation：项目级明说（项目级）', () => {
    expect(displayLocation(diagnostic({ path: '', line: 0 }))).toBe('（项目级）')
  })

  it('agentLocation：有列号时带列号，模型才能精确落到一个 token', () => {
    expect(agentLocation(diagnostic())).toBe('src/a.ts:4:2')
  })

  it('agentLocation：列号未知时不拼 ":0"', () => {
    expect(agentLocation(diagnostic({ column: 0 }))).toBe('src/a.ts:4')
  })

  it('agentLocation：项目级说清"没有具体文件"（不是空白）', () => {
    expect(agentLocation(diagnostic({ path: '', line: 0, column: 0 }))).toBe(
      '项目级（没有具体文件）',
    )
  })
})