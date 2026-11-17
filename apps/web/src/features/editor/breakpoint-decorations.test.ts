/**
 * 断点 glyph 的纯逻辑（`features/editor/breakpoint-decorations.ts`）。
 *
 * jsdom 里没有 Monaco，所以"gutter 上画成什么样"只能在这一层钉住：
 * 有条件的断点必须能被一眼认出来（空心圆点 + 问号角标），否则用户写了条件
 * 却看到和普通断点一样的实心点，就会以为条件没生效。
 */

import { describe, expect, it } from 'vitest'
import {
  BREAKPOINT_CLASS,
  CONDITIONAL_BREAKPOINT_CLASS,
  breakpointGlyphs,
  breakpointHover,
} from './breakpoint-decorations'

describe('breakpointGlyphs', () => {
  it('无条件断点用实心圆点', () => {
    expect(breakpointGlyphs([{ line: 3, condition: null }], 10)).toEqual([
      { line: 3, className: BREAKPOINT_CLASS, hoverMessage: '第 3 行：无条件断点' },
    ])
  })

  it('有条件断点用空心圆点 + 问号角标（与普通断点区分开）', () => {
    expect(breakpointGlyphs([{ line: 12, condition: 'count > 3' }], 20)).toEqual([
      { line: 12, className: CONDITIONAL_BREAKPOINT_CLASS, hoverMessage: '第 12 行：条件 count > 3' },
    ])
    // 修饰类是在基础类之上叠加：基础类丢了 CSS 就整块不显示。
    expect(CONDITIONAL_BREAKPOINT_CLASS.split(' ')).toContain(BREAKPOINT_CLASS)
  })

  it('越界的行号被丢掉（文件改短后旧断点不在别的行复活）', () => {
    const glyphs = breakpointGlyphs(
      [
        { line: 0, condition: null },
        { line: 2, condition: null },
        { line: 99, condition: 'x > 0' },
      ],
      5,
    )

    expect(glyphs.map((glyph) => glyph.line)).toEqual([2])
  })

  it('没有条目 / 传 undefined 时什么都不画', () => {
    expect(breakpointGlyphs([], 10)).toEqual([])
    expect(breakpointGlyphs(undefined, 10)).toEqual([])
  })
})

describe('breakpointHover', () => {
  it('条件原文出现在悬停提示里（截断的文本也要能看全）', () => {
    expect(breakpointHover(7, 'state.ready && count > 3')).toContain('state.ready && count > 3')
  })
})