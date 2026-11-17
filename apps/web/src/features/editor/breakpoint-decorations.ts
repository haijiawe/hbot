/**
 * 断点 → glyph margin 装饰的纯逻辑（不 import monaco，便于 vitest 直接测）。
 *
 * 为什么单独一层：装饰本身要 `monaco.Range`，而"哪一行画成什么样"是纯函数。
 * 把它留在 `editor-surface.tsx` 里就只能靠肉眼验（jsdom 里没有 Monaco），
 * 抽出来之后"有条件 = 空心圆点 + 问号角标"这条能被断言住。
 */

import type { DebugBreakpoint } from '../../generated'

/** 普通断点（实心圆点）。 */
export const BREAKPOINT_CLASS = 'debug-breakpoint'

/** 有条件断点：同一个圆点加一个修饰类（CSS 里画成空心圆点 + 问号角标）。 */
export const CONDITIONAL_BREAKPOINT_CLASS = 'debug-breakpoint debug-breakpoint--conditional'

/** 一行断点该画成什么样（行号已夹在有效范围内）。 */
export interface BreakpointGlyph {
  /** 1-based 行号。 */
  line: number
  /** 给 `glyphMarginClassName` 用。 */
  className: string
  /** 悬停提示：让用户不点开也知道这行有条件。 */
  hoverMessage: string
}

/** 某一行断点的悬停文案（行号 + 条件原文）。 */
export function breakpointHover(line: number, condition: string | null): string {
  return condition === null ? `第 ${line} 行：无条件断点` : `第 ${line} 行：条件 ${condition}`
}

/**
 * 断点条目 → 装饰描述（只给 1..lineCount 范围内的行）。
 *
 * 越界的行号要丢掉而不是夹取：文件改短之后旧断点不该在别的行上"复活"，
 * 它们在行数恢复时会自己回来（断点存在 store 里，不依赖当前文本）。
 */
export function breakpointGlyphs(
  entries: readonly DebugBreakpoint[] | undefined,
  lineCount: number,
): BreakpointGlyph[] {
  return (entries ?? [])
    .filter((entry) => entry.line >= 1 && entry.line <= lineCount)
    .map((entry) => ({
      line: entry.line,
      className: entry.condition === null ? BREAKPOINT_CLASS : CONDITIONAL_BREAKPOINT_CLASS,
      hoverMessage: breakpointHover(entry.line, entry.condition),
    }))
}