/**
 * 日志面：把 runStore 的环形缓冲铺成一页可筛选的日志流。
 *
 * 与底部「日志」页签的区别：页签跟着当前 bot 走、只占底部一条；这个面是整页，
 * 用来"对着日志读一遍"。数据来源同一个 store，不另发命令（日志只有事件与 logs.tail 两条来路）。
 *
 * **窗口化**：环形缓冲上限 10000 行，全量渲染一次滚动要重排一万个节点。
 * 这里复用 `lib/log-window.ts` 的 `visibleWindow()`，只渲染可视区 + 上下各
 * `PAGE_OVERSCAN` 行，行高固定 `PAGE_ROW_HEIGHT`，用绝对定位摆位。
 * 代价与底部页签相同：单行不换行、超长内容行内截断（全文挂在 `title`）。
 *
 * **最新在上**：`visible` 是反转后的数组，窗口按显示顺序算，取行时用
 * `newestFirstIndex()` 翻回源下标 —— 直接拿显示下标去读源数组会读到最旧那几行。
 * 新行插在最前面，所以滚动位置 0 就是最新，不需要「跳到最新」按钮。
 */

import { type CSSProperties, useCallback, useLayoutEffect, useMemo, useRef, useState } from "react"
import { clockOf } from '../../lib/job-board'
import type { LogLine, LogLevel } from '../../lib/ipc'
import { logLineKey, visibleWindow } from '../../lib/log-window'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 四档级别（与日志面板同口径：contract 的 trace 已在 store 折成 debug）。 */
const LEVELS: readonly LogLevel[] = ['error', 'warn', 'info', 'debug']

const LEVEL_LABELS: Readonly<Record<LogLevel, string>> = {
  error: '错误',
  warn: '警告',
  info: '信息',
  debug: '调试',
}

/** 整页日志的固定行高（px）：11px 等宽字体 + 上下各 1px 内边距，取整到 20。 */
const PAGE_ROW_HEIGHT = 20

/** 可视区上下各多渲染几行。 */
const PAGE_OVERSCAN = 12

const SCROLLER_STYLE: CSSProperties = {
  flex: '1 1 auto',
  minHeight: 0,
  overflowY: 'auto',
}

const LIST_STYLE: CSSProperties = {
  position: 'relative',
  margin: 0,
  padding: 0,
}

/** 每一行的绝对定位：行高固定，所以 top 就是「显示序号 × 行高」。 */
function rowStyle(index: number): CSSProperties {
  return {
    position: 'absolute',
    top: index * PAGE_ROW_HEIGHT,
    left: 0,
    right: 0,
    height: PAGE_ROW_HEIGHT,
    lineHeight: PAGE_ROW_HEIGHT + "px",
  }
}

/** 过滤：级别开关 + 子串（大小写不敏感）。空查询表示不过滤。 */
export function filterLogs(
  lines: readonly LogLine[],
  levels: readonly LogLevel[],
  query: string,
): LogLine[] {
  const needle = query.trim().toLowerCase()
  return lines.filter((line) => {
    if (levels.length > 0 && !levels.includes(line.level)) return false
    if (needle.length === 0) return true
    return line.message.toLowerCase().includes(needle)
  })
}

function countsOf(lines: readonly LogLine[]): Record<LogLevel, number> {
  const counts: Record<LogLevel, number> = { error: 0, warn: 0, info: 0, debug: 0 }
  for (const line of lines) counts[line.level] += 1
  return counts
}

/** 侧栏：级别计数 + 清空。 */
export function LogsSide() {
  const logs = useRunStore((state) => state.logs)
  const clearLogs = useRunStore((state) => state.clearLogs)
  const counts = countsOf(logs)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">LOGS</div>
        <h2 className="wb-title">开发日志</h2>
        <span className="wb-pill">{logs.length} 行</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => clearLogs()} disabled={logs.length === 0}>
            清空
          </button>
        </div>
      </header>
      <ul className="wb-list" aria-label="日志级别统计">
        {LEVELS.map((level) => (
          <li key={level} className="wb-item">
            <div className="wb-item__main">
              <span className="wb-item__name">{LEVEL_LABELS[level]}</span>
              <span className="wb-item__meta">{level}</span>
            </div>
            <span className="wb-tag">{counts[level]}</span>
          </li>
        ))}
      </ul>
      <p className="wb-note">环形缓冲最多 10000 行；更早的用底部「日志」页签的补读。</p>
    </div>
  )
}

/** 主区域：筛选条 + 窗口化的日志流（新的在最上）。 */
export function LogsView() {
  const logs = useRunStore((state) => state.logs)
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const [query, setQuery] = useState('')
  const [levels, setLevels] = useState<LogLevel[]>([])
  const scrollerRef = useRef<HTMLDivElement | null>(null)
  const [viewportHeight, setViewportHeight] = useState(0)
  const [scrollTop, setScrollTop] = useState(0)
  const counts = useMemo(() => countsOf(logs), [logs])
  const visible = useMemo(() => filterLogs(logs, levels, query).slice().reverse(), [logs, levels, query])

  const window_ = useMemo(
    () =>
      visibleWindow(visible.length, scrollTop, viewportHeight, {
        rowHeight: PAGE_ROW_HEIGHT,
        overscan: PAGE_OVERSCAN,
        fallbackAnchor: 'start',
      }),
    [visible.length, scrollTop, viewportHeight],
  )

  // 容器高度变化要跟上：有 ResizeObserver 就观察它，没有就退化成 window.resize。
  useLayoutEffect(() => {
    const scroller = scrollerRef.current
    if (!scroller) return
    const measure = () => {
      const height = scroller.clientHeight
      setViewportHeight((previous) => (previous === height ? previous : height))
    }
    measure()
    const Observer = typeof ResizeObserver === "undefined" ? null : ResizeObserver
    if (Observer === null) {
      window.addEventListener('resize', measure)
      return () => window.removeEventListener('resize', measure)
    }
    const observer = new Observer(measure)
    observer.observe(scroller)
    return () => observer.disconnect()
  }, [])

  const handleScroll = useCallback(() => {
    const scroller = scrollerRef.current
    if (!scroller) return
    setScrollTop(scroller.scrollTop)
  }, [])

  const rows = useMemo(() => {
    const slice: LogLine[] = []
    for (let display = window_.start; display < window_.end; display += 1) {
      const line = visible[display]
      if (line === undefined) continue
      slice.push(line)
    }
    return slice
  }, [visible, window_.start, window_.end])

  const toggle = (level: LogLevel) => {
    setLevels((current) =>
      current.includes(level) ? current.filter((item) => item !== level) : [...current, level],
    )
  }

  return (
    <div className="wb-page wb-page--editor">
      <header className="wb-head">
        <div className="wb-eyebrow">LOGS</div>
        <h2 className="wb-title">开发日志</h2>
        <span className="wb-pill">
          {visible.length} / {logs.length} 行
        </span>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : (
        <>
          <div className="wb-form">
            <input
              className="wb-input"
              placeholder="过滤日志内容，例如 HBOT_ 或 reply"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              aria-label="过滤日志"
            />
            {LEVELS.map((level) => (
              <button
                key={level}
                type="button"
                className={'wb-btn' + (levels.includes(level) ? ' wb-btn--primary' : '')}
                aria-pressed={levels.includes(level)}
                onClick={() => toggle(level)}
              >
                {LEVEL_LABELS[level]} {counts[level]}
              </button>
            ))}
          </div>
          {visible.length === 0 ? (
            <p className="wb-empty">
              {logs.length === 0 ? '还没有日志：启动 bot 并给它发一条消息。' : '没有匹配的行。'}
            </p>
          ) : (
            <div className="wb-log-scroll" ref={scrollerRef} style={SCROLLER_STYLE} onScroll={handleScroll}>
              <ol className="wb-log" style={{ ...LIST_STYLE, height: window_.totalHeight }} aria-label="日志流">
                {rows.map((line, index) => (
                  <li
                    key={logLineKey(line)}
                    className={'wb-log__line wb-log__line--' + line.level}
                    style={rowStyle(window_.start + index)}
                    title={line.message}
                    aria-setsize={visible.length}
                    aria-posinset={window_.start + index + 1}
                  >
                    <span className="wb-log__time">{clockOf(line.at)}</span>
                    <span className="wb-log__level">{line.level}</span>
                    <span className="wb-log__text">{line.message}</span>
                  </li>
                ))}
              </ol>
            </div>
          )}
        </>
      )}
    </div>
  )
}
