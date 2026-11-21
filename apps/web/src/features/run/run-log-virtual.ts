/**
 * 兼容层：窗口化数学已搬到 `lib/log-window.ts`（整页面「开发日志」也要用同一份）。
 *
 * 这里只留**组件内部的排版度量**（左右留白），并把数学按原名再导出，
 * 这样 `run-log.tsx` 与它自己的单测都不用改 import。
 */

export {
  LOG_FALLBACK_ROWS,
  LOG_FOLLOW_THRESHOLD,
  LOG_OVERSCAN,
  LOG_ROW_HEIGHT,
  isAtBottom,
  isAtTop,
  logLineKey,
  logWindow,
  newestFirstIndex,
  visibleWindow,
  type LogWindow,
  type WindowMetrics,
} from "../../lib/log-window"

/** 列表左右内边距（px）。窗口化后行是绝对定位的，左右留白要自己补。 */
export const LOG_LIST_GUTTER = 8
