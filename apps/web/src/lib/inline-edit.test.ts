/**
 * 行内编辑的选区快照与"打开器"桥（`lib/inline-edit.ts`）。
 *
 * 选区唯一真源是 Monaco 的 editor 实例；这里只把它变成**一次性纯数据**。两条要求：
 * 行号夹进有效范围（快捷键可能命中一个刚被写短的文件），以及没有编辑器时
 * `requestInlineEdit()` 必须返回 false 让调用方去给可见反馈。
 */

import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  registerInlineEditOpener,
  requestInlineEdit,
  selectionSnapshot,
} from './inline-edit'

const LINES = ['line one', 'line two', 'line three']

afterEach(() => {
  // 每个用例都把桥清掉：模块级单例，留着会串到后面的用例。
  registerInlineEditOpener(() => false)()
})

describe('selectionSnapshot', () => {
  it('多行选区按 1-based 闭区间切片，行间用 \\n 连接', () => {
    expect(selectionSnapshot('src/a.ts', LINES, 1, 2)).toEqual({
      path: 'src/a.ts',
      startLine: 1,
      endLine: 2,
      text: 'line one\nline two',
    })
  })

  it('空选区（光标所在行）也是合法快照', () => {
    expect(selectionSnapshot('src/a.ts', LINES, 2, 2)?.text).toBe('line two')
  })

  it('起止颠倒时自动摆正（反向选择的选区）', () => {
    expect(selectionSnapshot('src/a.ts', LINES, 3, 2)).toEqual({
      path: 'src/a.ts',
      startLine: 2,
      endLine: 3,
      text: 'line two\nline three',
    })
  })

  it('行号夹进 [1, 行数]：文件刚被写短时不能把 undefined 拼进请求', () => {
    expect(selectionSnapshot('src/a.ts', LINES, 7, 9)).toEqual({
      path: 'src/a.ts',
      startLine: 3,
      endLine: 3,
      text: 'line three',
    })
  })

  it('空文件 / 空路径返回 null', () => {
    expect(selectionSnapshot('src/a.ts', [], 1, 1)).toBeNull()
    expect(selectionSnapshot('', LINES, 1, 1)).toBeNull()
  })
})

describe('行内编辑的打开器桥', () => {
  it('没有注册时 requestInlineEdit 返回 false（调用方据此给 toast）', () => {
    registerInlineEditOpener(() => false)()
    expect(requestInlineEdit()).toBe(false)
  })

  it('注册后请求会转给编辑器，并把返回值透出来', () => {
    const opener = vi.fn(() => true)
    registerInlineEditOpener(opener)
    expect(requestInlineEdit()).toBe(true)
    expect(opener).toHaveBeenCalledTimes(1)
  })

  it('注销函数只清掉自己那一个（后注册的不会被旧注销函数顺手丢掉）', () => {
    const disposeFirst = registerInlineEditOpener(() => true)
    registerInlineEditOpener(() => false)
    disposeFirst()
    expect(requestInlineEdit()).toBe(false)
  })
})