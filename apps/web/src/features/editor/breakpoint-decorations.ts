/**
 * 断点/日志点 → glyph margin 装饰的纯逻辑（不 import monaco，便于 vitest 直接测）。
 *
 * 为什么单独一层：装饰本身要 `monaco.Range`，而"哪一行画成什么样"是纯函数。
 * 把它留在 `editor-surface.tsx` 里就只能靠肉眼验（jsdom 里没有 Monaco），
 * 抽出来之后"有条件 = 空心圆点 + 问号角标""日志点 = 实心方块"这两条能被断言住。
 *
 * 三种形状是刻意的：实心圆点 = 无条件断点（会停），空心带问号 = 有条件断点（可能停），
 * 实心方块 = 日志点（**绝不停**）。只靠颜色分不出这三种（色弱、以及暂停行本身就是暖色底）。
 */

import type { DebugBreakpoint } from '../../generated'

/** 普通断点（实心圆点）。 */
export const BREAKPOINT_CLASS = 'debug-breakpoint'

/** 有条件断点：同一个圆点加一个修饰类（CSS 里画成空心圆点 + 问号角标）。 */
export const CONDITIONAL_BREAKPOINT_CLASS = 'debug-breakpoint debug-breakpoint--conditional'

/** 日志点（实心方块）：命中只打日志、不暂停。 */
export const LOGPOINT_CLASS = 'debug-logpoint'

/** 一行断点/日志点该画成什么样（行号已夹在有效范围内）。 */
export interface BreakpointGlyph {
  /** 1-based 行号。 */
  line: number
  /** 给 `glyphMarginClassName` 用。 */
  className: string
  /** 悬停提示：让用户不点开也知道这行有条件 / 打什么日志。 */
  hoverMessage: string
}

/** 某一行断点的悬停文案（行号 + 条件原文）。 */
export function breakpointHover(line: number, condition: string | null): string {
  return condition === null ? `第 ${line} 行：无条件断点` : `第 ${line} 行：条件 ${condition}`
}

/** 某一行日志点的悬停文案（行号 + 模板原文）。 */
export function logpointHover(line: number, template: string): string {
  return `第 ${line} 行：日志点 ${template}`
}

/**
 * 条目 → 装饰描述（只给 1..lineCount 范围内的行）。
 *
 * 越界的行号要丢掉而不是夹取：文件改短之后旧断点不该在别的行上"复活"，
 * 它们在行数恢复时会自己回来（断点存在 store 里，不依赖当前文本）。
 *
 * 同一行**同时**有断点与日志点时（store 里是两条）只画断点那个圆点：一个槽位只能
 * 显示一个字形，而"会停下来"比"会打日志"更需要被看见；日志点的存在由悬停文案与
 * 调试面板里的清单交代。
 */
export function breakpointGlyphs(
  entries: readonly DebugBreakpoint[] | undefined,
  lineCount: number,
): BreakpointGlyph[] {
  const glyphs: BreakpointGlyph[] = []
  const seen = new Set<number>()
  for (const entry of entries ?? []) {
    if (entry.line < 1 || entry.line > lineCount) continue
    // 同一行先来的是断点（store 的排序保证），后到的日志点让位。
    if (seen.has(entry.line)) continue
    seen.add(entry.line)
    if (entry.log !== null) {
      glyphs.push({
        line: entry.line,
        className: LOGPOINT_CLASS,
        hoverMessage: logpointHover(entry.line, entry.log),
      })
      continue
    }
    glyphs.push({
      line: entry.line,
      className: entry.condition === null ? BREAKPOINT_CLASS : CONDITIONAL_BREAKPOINT_CLASS,
      hoverMessage: breakpointHover(entry.line, entry.condition),
    })
  }
  return glyphs.sort((left, right) => left.line - right.line)
}