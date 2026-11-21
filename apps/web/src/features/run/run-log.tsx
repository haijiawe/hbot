/**
 * 运行日志：只读列表，aria-live 播报，级别同时用文字与颜色表达。
 *
 * **日志只有这一个渲染器**：整页「开发日志」面已经删掉（2026-11-21 UI 收敛），
 * 因为它是同一个 runStore.logs 的第二个渲染器。原来那面的筛选能力搬到这里：
 * 级别开关 + 子串过滤，数学在 lib/log-filter.ts（可单独测）。
 *
 * **窗口化（virtualization）**：日志最多 10k 行（run-store 的 MAX_LOG_LINES），
 * 全量渲染时一次滚动要重排 10k 个 li。这里只渲染可视区 + 上下各
 * LOG_OVERSCAN 行，用**固定行高**把「第 i 行在哪」算成 O(1)，窗口边界由
 * run-log-virtual.ts 的纯函数 logWindow() 给出（那边有 node 环境的单测）。
 *
 * 四条纪律：
 * 1. **固定行高**：行高是模块级常量 LOG_ROW_HEIGHT，行用绝对定位摆位；
 *    ol 自身高度写成「行数 × 行高」，滚动范围由这个高度撑出，
 *    既不必赌浏览器会不会让绝对定位子节点撑开 scrollHeight，
 *    也不往 aria-live 容器里塞占位空节点（那里只有真实日志行）。
 *    代价：单行不换行，超长消息在本行内截断（text-overflow: ellipsis），
 *    全文仍在 DOM 文本里（屏幕阅读器读得到），并挂 title 供鼠标查看。
 * 2. **跟随可撤销**：默认贴底；用户一旦向上滚动就停止跟随（follow = false），
 *    并出现「跳到最新」按钮；点它恢复跟随并滚到底。滚回底部则自动恢复跟随。
 * 3. **稳定 key**：logLineKey() 按行对象身份发号，不用切片下标（窗口化后下标会漂）。
 * 4. **尺寸变化要跟上**：外部改变底部面板高度时，可视行数跟着变（ResizeObserver；
 *    环境没有它就退化成 window.resize）。
 */

import {
  type CSSProperties,
  type ReactElement,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { formatClock, logLevelLabel } from '../../lib/format'
import {
  LOG_LEVELS,
  LOG_LEVEL_LABELS,
  countsOf,
  filterLogs,
  toggleLevel,
} from '../../lib/log-filter'
import type { LogLevel } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import {
  LOG_LIST_GUTTER,
  LOG_ROW_HEIGHT,
  isAtBottom,
  logLineKey,
  logWindow,
} from './run-log-virtual'

/** 滚动容器：唯一负责滚动与尺寸测量的元素。 */
const SCROLLER_STYLE: CSSProperties = {
  flex: '1 1 auto',
  height: '100%',
  minHeight: 0,
  overflowY: 'auto',
}

/** 列表：绝对定位行的包含块，高度就是占位总高度（撑出滚动条）。 */
const LIST_STYLE: CSSProperties = {
  position: 'relative',
  margin: 0,
  // .logs 的 padding 会挪动绝对定位的基准，这里清掉，留白交给每一行的左右 gutter。
  padding: 0,
  listStyle: 'none',
}

/** 外层：给「跳到最新」一个定位父级（按钮不能跟着列表一起滚走）。 */
const WRAP_STYLE: CSSProperties = {
  position: 'relative',
  height: '100%',
  minHeight: 0,
  display: 'flex',
  flexDirection: 'column',
}

const JUMP_STYLE: CSSProperties = {
  position: 'absolute',
  right: LOG_LIST_GUTTER * 2,
  bottom: LOG_LIST_GUTTER * 2,
  background: 'var(--bg-elevated)',
  border: '1px solid var(--border-strong)',
  boxShadow: '0 2px 6px rgba(0, 0, 0, 0.35)',
}

/** 每一行的绝对定位：行高固定，所以 top 就是「行号 × 行高」。 */
function rowStyle(index: number): CSSProperties {
  return {
    position: 'absolute',
    top: index * LOG_ROW_HEIGHT,
    left: LOG_LIST_GUTTER,
    right: LOG_LIST_GUTTER,
    height: LOG_ROW_HEIGHT,
    // 文本行高比行高少 2px，给 .log 的上下 1px 内边距留位（CSS 里没改这条）。
    lineHeight: LOG_ROW_HEIGHT - 2 + 'px',
    overflow: 'hidden',
  }
}

/** 消息单行截断（固定行高的代价），全文由 title 与 DOM 文本保留。 */
const MESSAGE_STYLE: CSSProperties = {
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
}

export function RunLog() {
  const logs = useRunStore((state) => state.logs)
  const scrollerRef = useRef<HTMLDivElement | null>(null)
  const [viewportHeight, setViewportHeight] = useState(0)
  const [scrollTop, setScrollTop] = useState(0)
  // 默认贴底：新日志到达时自动滚到底。
  const [follow, setFollow] = useState(true)
  // 筛选是「看什么」不是「存什么」：只活在组件里，切走页签就清掉。
  const [levels, setLevels] = useState<readonly LogLevel[]>([])
  const [query, setQuery] = useState('')

  const counts = useMemo(() => countsOf(logs), [logs])
  const filtered = useMemo(() => filterLogs(logs, levels, query), [logs, levels, query])

  const count = filtered.length
  const visible = useMemo(
    () => logWindow(count, scrollTop, viewportHeight),
    [count, scrollTop, viewportHeight],
  )

  const scrollToBottom = useCallback(() => {
    const scroller = scrollerRef.current
    if (!scroller) return
    const bottom = Math.max(0, scroller.scrollHeight - scroller.clientHeight)
    scroller.scrollTop = bottom
    setScrollTop(scroller.scrollTop)
  }, [])

  // 新日志到达 / 恢复跟随 / 尺寸变化：跟随态贴底，否则保持用户当前位置。
  //
  // 依赖里放 filtered 而不是它的长度：环形缓冲满 10k 之后是「丢一行、加一行」，
  // 长度不变 —— 只看长度的话，停在底部的用户会被悄悄甩在原地。
  useLayoutEffect(() => {
    if (follow) scrollToBottom()
  }, [follow, filtered, viewportHeight, scrollToBottom])

  // 容器高度变化要跟上：有 ResizeObserver 就观察它，没有就退化成 window.resize。
  useLayoutEffect(() => {
    const scroller = scrollerRef.current
    if (!scroller) return
    const measure = () => {
      const height = scroller.clientHeight
      setViewportHeight((previous) => (previous === height ? previous : height))
    }
    measure()
    const Observer = typeof ResizeObserver === 'undefined' ? null : ResizeObserver
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
    // 用户滚回底部就重新跟随（不必再点按钮）。
    setFollow(isAtBottom(scroller.scrollTop, scroller.scrollHeight, scroller.clientHeight))
  }, [])

  const jumpToLatest = useCallback(() => {
    setFollow(true)
    scrollToBottom()
  }, [scrollToBottom])

  const rows = useMemo(() => {
    const slice: ReactElement[] = []
    for (let index = visible.start; index < visible.end; index += 1) {
      const line = filtered[index]
      if (line === undefined) continue
      slice.push(
        <li
          key={logLineKey(line)}
          className={'log log--' + line.level}
          style={rowStyle(index)}
          title={line.message}
          // 窗口化后 DOM 里只有可视区那几行；告诉屏幕阅读器它在整份列表里的位置。
          aria-setsize={count}
          aria-posinset={index + 1}
        >
          <time className='log__time' dateTime={new Date(line.at).toISOString()}>
            {formatClock(line.at)}
          </time>
          <span className='log__level'>{logLevelLabel(line.level)}</span>
          <span className='log__source'>{line.source}</span>
          <span className='log__message' style={MESSAGE_STYLE}>
            {line.message}
          </span>
        </li>,
      )
    }
    return slice
  }, [filtered, count, visible.start, visible.end])

  // 一条日志都没有：整块让位给空态（工具栏也不显示，没什么可筛的）。
  if (logs.length === 0) {
    return (
      <p className='panel-empty' role='status'>
        尚未启动 bot，暂无日志。
      </p>
    )
  }

  return (
    <div className='logs-wrap' style={WRAP_STYLE}>
      <div className='logs-toolbar' role='toolbar' aria-label='日志筛选'>
        <input
          type='search'
          className='logs-toolbar__query'
          placeholder='筛选日志…'
          aria-label='按内容筛选日志'
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
        {LOG_LEVELS.map((level) => {
          const on = levels.includes(level)
          return (
            <button
              key={level}
              type='button'
              className={'logs-toolbar__level logs-toolbar__level--' + level + (on ? ' is-on' : '')}
              aria-pressed={on}
              title={on ? '取消只看' + LOG_LEVEL_LABELS[level] : '只看' + LOG_LEVEL_LABELS[level]}
              onClick={() => setLevels((previous) => toggleLevel(previous, level))}
            >
              {LOG_LEVEL_LABELS[level]}
              <span className='logs-toolbar__count'>{counts[level]}</span>
            </button>
          )
        })}
        {levels.length > 0 || query.length > 0 ? (
          <button
            type='button'
            className='logs-toolbar__reset'
            onClick={() => {
              setLevels([])
              setQuery('')
            }}
          >
            清除筛选
          </button>
        ) : null}
        <span className='logs-toolbar__meta' role='status'>
          {count === logs.length ? logs.length + ' 行' : count + ' / ' + logs.length + ' 行'}
        </span>
      </div>
      {count === 0 ? (
        <p className='panel-empty' role='status'>
          没有匹配的日志：清掉筛选，或换个关键词。
        </p>
      ) : (
        <div className='logs-scroll' ref={scrollerRef} style={SCROLLER_STYLE} onScroll={handleScroll}>
          <ol
            className='logs'
            style={{ ...LIST_STYLE, height: visible.totalHeight }}
            aria-live='polite'
            aria-label='运行日志'
          >
            {rows}
          </ol>
        </div>
      )}
      {follow || count === 0 ? null : (
        <button
          type='button'
          className='icon-button logs-jump'
          style={JUMP_STYLE}
          onClick={jumpToLatest}
          aria-label='跳到最新日志'
          title='跳到最新日志并恢复跟随'
        >
          ↓ 跳到最新
        </button>
      )}
    </div>
  )
}
