/**
 * 运行日志窗口化的纯计算（不碰 React、不碰 DOM），可在 node 环境直接单测。
 *
 * 为什么单独一个文件：窗口边界、贴底判定、稳定 key 这三件事都是纯函数，
 * 混在组件里只能靠 jsdom 间接验证；抽出来以后 `run-log-virtual.test.ts` 用
 * 默认的 node 环境就能逐条钉死（jsdom 里 `scrollHeight`/`clientHeight` 恒为 0，
 * 真布局这件事本来也验不了）。
 *
 * 行高为什么是**固定**常量：固定行高才能把「第 i 行在哪儿」算成 O(1) 的
 * `index * LOG_ROW_HEIGHT`，不必测量每一行。代价写在 `run-log.tsx` 的头注释里
 * （超长消息行内截断，全文挂在 `title`）。
 *
 * 数值为什么放在这里而不是 `lib/layout.ts`：layout.ts 是**面板级**布局的唯一真源
 * （侧栏/Agent 栏/底部面板的宽高），日志行高是组件内部的排版度量，不是面板布局；
 * 且本单元只被允许改 `features/run/` 下的文件。
 */

import type { LogLine } from '../../lib/ipc'

/** 每一行的固定高度（px）。字号 12px、行高 1.4 左右再加 1px 内边距，取整到 18。 */
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

/** 距底部多少 px 以内仍算「贴底」（给亚像素取整留余量）。 */
export const LOG_FOLLOW_THRESHOLD = 4

/** 列表左右内边距（px）。窗口化后行是绝对定位的，左右留白要自己补。 */
export const LOG_LIST_GUTTER = 8

/** 一次渲染要展示的窗口：`[start, end)` 半开区间 + 撑出滚动条的总高度。 */
export interface LogWindow {
  /** 切片起点（含）。 */
  start: number
  /** 切片终点（不含）。 */
  end: number
  /** `<ol>` 的占位总高度：`行数 × 行高`。窗口化靠它撑出滚动条，而不是塞占位空节点。 */
  totalHeight: number
}

/**
 * 由「总行数 / 当前滚动位置 / 可视高度」算出要渲染的窗口。
 *
 * `viewportHeight <= 0` 表示还没测量到（首帧或隐藏页签），此时退化成「末尾若干行」，
 * 与「默认贴底」的语义一致。
 */
export function logWindow(count: number, scrollTop: number, viewportHeight: number): LogWindow {
  const safeCount = Math.max(0, Math.floor(count))
  const totalHeight = safeCount * LOG_ROW_HEIGHT
  if (safeCount === 0) return { start: 0, end: 0, totalHeight: 0 }

  if (viewportHeight <= 0) {
    return { start: Math.max(0, safeCount - LOG_FALLBACK_ROWS), end: safeCount, totalHeight }
  }

  const visibleRows = Math.ceil(viewportHeight / LOG_ROW_HEIGHT)
  // 先夹到合法滚动范围：元素被程序赋了超出范围的值（浏览器会自己夹，jsdom 不会）也照样算对。
  const maxScroll = Math.max(0, totalHeight - viewportHeight)
  const top = Math.min(Math.max(scrollTop, 0), maxScroll)
  const firstVisible = Math.floor(top / LOG_ROW_HEIGHT)

  const start = Math.max(0, firstVisible - LOG_OVERSCAN)
  const end = Math.min(safeCount, Math.max(start, firstVisible + visibleRows + LOG_OVERSCAN))
  return { start, end, totalHeight }
}

/** 当前滚动位置是否贴底（用户在底部时继续自动跟随）。 */
export function isAtBottom(scrollTop: number, scrollHeight: number, clientHeight: number): boolean {
  // 内容比视口还短时必然没有滚动条，算贴底。
  const distance = scrollHeight - scrollTop - clientHeight
  return distance <= LOG_FOLLOW_THRESHOLD
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
  const key = `log-${line.at}-${nextKeySeq}`
  lineKeys.set(line, key)
  return key
}
