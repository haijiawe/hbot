/**
 * 日志窗口化的纯计算（不碰 React、不碰 DOM），node 环境可直接单测。
 *
 * 为什么放 `lib/`：底部「日志」页签（`features/run/run-log.tsx`）与整页面
 * 「开发日志」（`features/workbench/logs-view.tsx`）都要窗口化，而 feature 之间
 * 不允许互相 import 内部模块 —— 数学共用一份，只能放在这里。
 *
 * 两处差异只在「渲染顺序」与「行高」：页签按时间正序、行高 18、贴底跟随；
 * 整页面最新在上、行高 20、停在顶部。窗口计算与顺序无关 ——
 * 最新在上的列表把窗口套在**反转后的数组**上（`newestFirstIndex()` 是那层映射）。
 */

import type { LogLine } from './ipc'

/** 底部日志页签的固定行高（px）。字号 12px、行高 1.4 再加 1px 内边距，取整到 18。 */
export const LOG_ROW_HEIGHT = 18

/** 可视区上下各多渲染几行：滚动时留出缓冲，快速拖滚动条不至于露出空白。 */
export const LOG_OVERSCAN = 8

/**
 * 还没测量出可视高度时（首帧、jsdom、隐藏中的页签）先渲染末尾多少行。
 *
 * 不能渲染 0 行：容器高度为 0 在真实浏览器里只会出现一帧，但在「默认贴底」的语义下
 * 那一帧必须是**最新的日志**而不是顶部，否则用户会看到闪一下最旧的几行。
 */
export const LOG_FALLBACK_ROWS = 20

/** 距边界多少 px 以内仍算「贴底 / 在顶」（给亚像素取整留余量）。 */
export const LOG_FOLLOW_THRESHOLD = 4

/** 一次渲染要展示的窗口：`[start, end)` 半开区间 + 撑出滚动条的总高度。 */
export interface LogWindow {
  /** 切片起点（含）。 */
  start: number
  /** 切片终点（不含）。 */
  end: number
  /** 列表的占位总高度：`行数 × 行高`。窗口化靠它撑出滚动条，而不是塞占位空节点。 */
  totalHeight: number
}

/** 行高与缓冲的行数可以逐处覆盖（两个日志面的行高不同）。 */
export interface WindowMetrics {
  rowHeight?: number
  overscan?: number
  fallbackRows?: number
  /**
   * 还没测量到视口高度时，退化窗口贴哪一头。
   *
   * `end`（默认）给「按时间正序、贴底跟随」的页签用；`start` 给「最新在上」的
   * 整页面用 —— 那一面滚动位置 0 就是最新，退化到末尾会先闪出最旧的几行。
   */
  fallbackAnchor?: 'start' | 'end'
}

/**
 * 由「总行数 / 当前滚动位置 / 可视高度」算出要渲染的窗口。
 *
 * `viewportHeight <= 0` 表示还没测量到（首帧或隐藏页签），此时退化成「末尾若干行」，
 * 与「默认贴底」的语义一致。
 */
export function visibleWindow(
  count: number,
  scrollTop: number,
  viewportHeight: number,
  metrics: WindowMetrics = {},
): LogWindow {
  const rowHeight = metrics.rowHeight ?? LOG_ROW_HEIGHT
  const overscan = metrics.overscan ?? LOG_OVERSCAN
  const fallbackRows = metrics.fallbackRows ?? LOG_FALLBACK_ROWS
  const safeCount = Math.max(0, Math.floor(count))
  const totalHeight = safeCount * rowHeight
  if (safeCount === 0) return { start: 0, end: 0, totalHeight: 0 }

  if (viewportHeight <= 0) {
    if (metrics.fallbackAnchor === 'start') {
      return { start: 0, end: Math.min(safeCount, fallbackRows), totalHeight }
    }
    return { start: Math.max(0, safeCount - fallbackRows), end: safeCount, totalHeight }
  }

  const visibleRows = Math.ceil(viewportHeight / rowHeight)
  // 先夹到合法滚动范围：元素被程序赋了超出范围的值（浏览器会自己夹，jsdom 不会）也照样算对。
  const maxScroll = Math.max(0, totalHeight - viewportHeight)
  const top = Math.min(Math.max(scrollTop, 0), maxScroll)
  const firstVisible = Math.floor(top / rowHeight)

  const start = Math.max(0, firstVisible - overscan)
  const end = Math.min(safeCount, Math.max(start, firstVisible + visibleRows + overscan))
  return { start, end, totalHeight }
}

/** 底部日志页签用的默认窗口（行高 18 / 缓冲 8 / 退化 20 行）。 */
export function logWindow(count: number, scrollTop: number, viewportHeight: number): LogWindow {
  return visibleWindow(count, scrollTop, viewportHeight)
}

/** 当前滚动位置是否贴底（用户在底部时继续自动跟随）。 */
export function isAtBottom(scrollTop: number, scrollHeight: number, clientHeight: number): boolean {
  // 内容比视口还短时必然没有滚动条，算贴底。
  const distance = scrollHeight - scrollTop - clientHeight
  return distance <= LOG_FOLLOW_THRESHOLD
}

/** 当前滚动位置是否在顶部（最新在上的列表用这个判「是否还跟着最新」）。 */
export function isAtTop(scrollTop: number): boolean {
  return scrollTop <= LOG_FOLLOW_THRESHOLD
}

/**
 * 最新在上的列表里，显示序号 → 源数组下标（源数组按时间正序）。
 *
 * 单独成函数是因为「窗口下标」与「源下标」是两个方向：窗口按显示顺序算，
 * 取数据时要翻过来，否则会拿最新窗口去读最旧那几行。
 */
export function newestFirstIndex(count: number, displayIndex: number): number {
  return count - 1 - displayIndex
}

/**
 * 稳定行键：按**行的对象身份**发号，不依赖数组下标。
 *
 * 之前是 `${line.at}-${index}`，而窗口化后 `index` 变成切片内下标 —— 滚动一次
 * 同一行的下标就变了，React 会把旧 DOM 复用到别的行上（颜色/文本错位）。
 * `run-store` 追加日志时是 `[...existing, line]`，既有行的对象身份不变，
 * 所以 WeakMap 记住的号也不会变；整批替换（`loadLogs`）会换对象，重新发号正是期望行为。
 *
 * 为什么不是 `at + message` 拼字符串：同一毫秒里两条相同内容的日志会撞键，
 * React 会报重复 key 并把两行合并成一行展示。
 */
const lineKeys = new WeakMap<LogLine, string>()
let nextKeySeq = 0

export function logLineKey(line: LogLine): string {
  const existing = lineKeys.get(line)
  if (existing !== undefined) return existing
  nextKeySeq += 1
  const key = "log-" + line.at + "-" + nextKeySeq
  lineKeys.set(line, key)
  return key
}
