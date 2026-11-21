// @vitest-environment jsdom
/**
 * 运行日志面板（`features/run/run-log.tsx`）：窗口化 / 跟随可撤销 / 稳定 key / 空状态。
 *
 * jsdom 没有布局：`clientHeight` / `scrollHeight` 恒为 0，`scrollTop` 随便赋也不会被夹。
 * 所以这里在 `HTMLElement.prototype` 上按类名伪造这两个读数：滚动容器
 * `.logs-scroll` 的可视高度取自 `fakeViewport`，内容高度取自它子节点 `<ol>` 的
 * 行内 `height`（组件自己写的就是「行数 × 行高」）。断言的是"组件算出来的窗口与
 * 滚动位置"，不是浏览器的真实排版 —— 真排版在 jsdom 里本来也不存在。
 *
 * 每次追加/滚动都用 `act` 包住：组件在 `useLayoutEffect` 里贴底，不等 `act` 冲刷
 * 就断言会读到上一帧的 DOM。
 */

import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest'
import type { LogLine } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { RunLog } from './run-log'
import { LOG_OVERSCAN, LOG_ROW_HEIGHT } from './run-log-virtual'

/** 伪造的可视高度：默认 10 行。 */
let fakeViewport = LOG_ROW_HEIGHT * 10

/** 组件里那个滚动容器的类名（与 `run-log.tsx` 一致）。 */
const SCROLLER_CLASS = 'logs-scroll'

function isScroller(node: Element): boolean {
  return node.classList.contains(SCROLLER_CLASS)
}

function scroller(): HTMLElement {
  const node = document.querySelector('.' + SCROLLER_CLASS)
  if (!(node instanceof HTMLElement)) throw new Error('找不到日志滚动容器 .' + SCROLLER_CLASS)
  return node
}

beforeAll(() => {
  // 只有滚动容器有可视高度。
  Object.defineProperty(HTMLElement.prototype, 'clientHeight', {
    configurable: true,
    get(this: HTMLElement) {
      return isScroller(this) ? fakeViewport : 0
    },
  })
  // 内容高度 = 组件写在 `<ol>` 上的行内 height（滚动范围由它撑出）。
  Object.defineProperty(HTMLElement.prototype, 'scrollHeight', {
    configurable: true,
    get(this: HTMLElement) {
      if (!isScroller(this)) return 0
      const list = this.firstElementChild
      if (!(list instanceof HTMLElement)) return 0
      const height = Number.parseFloat(list.style.height)
      return Number.isFinite(height) ? Math.max(height, fakeViewport) : 0
    },
  })
})

afterAll(() => {
  Reflect.deleteProperty(HTMLElement.prototype, 'clientHeight')
  Reflect.deleteProperty(HTMLElement.prototype, 'scrollHeight')
})

function makeLog(index: number): LogLine {
  return {
    level: index % 7 === 0 ? 'error' : 'info',
    source: 'bot-alpha',
    message: '第 ' + index + ' 条日志',
    at: 1_700_000_000_000 + index,
  }
}

function seedLogs(count: number): LogLine[] {
  const lines: LogLine[] = []
  for (let index = 0; index < count; index += 1) lines.push(makeLog(index))
  return lines
}

function resetStore(logs: LogLine[]): void {
  useRunStore.setState({
    botId: '',
    status: 'stopped',
    logs,
    busy: false,
    error: null,
    hydrated: true,
    live: false,
  })
}

/** 渲染出来的行里包含该文本的那一个（找不到就抛，避免断言在 undefined 上"通过"）。 */
function rowOf(message: string): HTMLLIElement {
  const rows = Array.from(document.querySelectorAll('li'))
  const found = rows.find((row) => (row.textContent ?? '').includes(message))
  if (!found) throw new Error('没有渲染这一行：' + message)
  return found
}

/** 真实用户滚动：改 scrollTop + 派发 scroll 事件（jsdom 不会自己发）。 */
function scrollTo(top: number): void {
  const node = scroller()
  node.scrollTop = top
  fireEvent.scroll(node)
}

beforeEach(() => {
  fakeViewport = LOG_ROW_HEIGHT * 10
  resetStore([])
})

afterEach(cleanup)

describe('RunLog 窗口化', () => {
  it('10000 条日志时 DOM 里的 <li> 被限制在小常数内，并记下渲染耗时', () => {
    resetStore(seedLogs(10_000))
    const started = performance.now()
    const { container } = render(<RunLog />)
    const elapsedMs = performance.now() - started

    const renderedRows = container.querySelectorAll('li').length
    const renderedNodes = container.querySelectorAll('*').length
    // 上界：可视 10 行 + 上下各 LOG_OVERSCAN 行 + 1 行取整余量。
    const upperBound = 10 + LOG_OVERSCAN * 2 + 1

    expect(renderedRows).toBeGreaterThan(0)
    expect(renderedRows).toBeLessThanOrEqual(80)
    expect(renderedRows).toBeLessThanOrEqual(upperBound)

    // 全量渲染对照（推算）：每行 <li> + <time> + 3 个 <span> = 5 个元素。
    const legacyNodes = 10_000 * 5 + 2
    console.log('[perf] 10000 条日志首屏渲染（含贴底后的第二次提交）耗时 ' + elapsedMs.toFixed(1) + ' ms')
    console.log(
      '[dom] 窗口化后渲染 ' + renderedNodes + ' 个元素（其中 ' + renderedRows + ' 行），全量渲染约 ' +
        legacyNodes + ' 个（推算：每行 5 个元素）',
    )
    expect(renderedNodes * 50).toBeLessThan(legacyNodes)
  })

  it('容器高度变化后可视行数跟着变（ResizeObserver 缺失时走 window.resize）', () => {
    if (typeof ResizeObserver !== 'undefined') {
      // jsdom 目前没有 ResizeObserver；若将来补上，这条要靠 observer 的 fake 驱动，另写。
      return
    }
    resetStore(seedLogs(1000))
    render(<RunLog />)
    const before = document.querySelectorAll('li').length

    fakeViewport = LOG_ROW_HEIGHT * 30
    act(() => {
      window.dispatchEvent(new Event('resize'))
    })

    const after = document.querySelectorAll('li').length
    expect(after).toBeGreaterThan(before)
    expect(after).toBeLessThanOrEqual(30 + LOG_OVERSCAN * 2 + 1)
  })
})

describe('RunLog 跟随最新', () => {
  it('追加新日志时仍贴底', () => {
    resetStore(seedLogs(500))
    render(<RunLog />)
    const node = scroller()
    expect(node.scrollTop).toBe(500 * LOG_ROW_HEIGHT - fakeViewport)

    act(() => {
      useRunStore.setState({ logs: [...useRunStore.getState().logs, makeLog(500)] })
    })

    expect(node.scrollTop).toBe(501 * LOG_ROW_HEIGHT - fakeViewport)
  })

  it('用户向上滚动后停止跟随，「跳到最新」出现并恢复贴底', () => {
    resetStore(seedLogs(1000))
    render(<RunLog />)
    const node = scroller()
    expect(document.querySelector('.logs-jump')).toBeNull()

    // 模拟用户向上滚到顶部。
    scrollTo(0)
    expect(node.scrollTop).toBe(0)
    expect(screen.getByRole('button', { name: '跳到最新日志' })).toBeTruthy()

    // 再来新日志：不许把用户拽回底部。
    act(() => {
      useRunStore.setState({ logs: [...useRunStore.getState().logs, makeLog(1000)] })
    })
    expect(node.scrollTop).toBe(0)
    expect(screen.getByRole('button', { name: '跳到最新日志' })).toBeTruthy()

    // 点「跳到最新」：恢复跟随并滚到底，按钮消失。
    fireEvent.click(screen.getByRole('button', { name: '跳到最新日志' }))
    expect(node.scrollTop).toBe(1001 * LOG_ROW_HEIGHT - fakeViewport)
    expect(document.querySelector('.logs-jump')).toBeNull()

    // 恢复跟随之后再追加：继续贴底。
    act(() => {
      useRunStore.setState({ logs: [...useRunStore.getState().logs, makeLog(1001)] })
    })
    expect(node.scrollTop).toBe(1002 * LOG_ROW_HEIGHT - fakeViewport)
  })

  it('用户自己滚回底部时自动恢复跟随（不必点按钮）', () => {
    resetStore(seedLogs(1000))
    render(<RunLog />)
    const node = scroller()

    scrollTo(0)
    expect(document.querySelector('.logs-jump')).toBeTruthy()

    scrollTo(1000 * LOG_ROW_HEIGHT - fakeViewport)
    expect(node.scrollTop).toBe(1000 * LOG_ROW_HEIGHT - fakeViewport)
    expect(document.querySelector('.logs-jump')).toBeNull()
  })
})

describe('RunLog 稳定 key 与可访问性', () => {
  it('滚动后同一行的 DOM 节点被复用（key 不随切片下标漂移）', () => {
    resetStore(seedLogs(1000))
    render(<RunLog />)
    const message = '第 995 条日志'
    const before = rowOf(message)

    scrollTo(980 * LOG_ROW_HEIGHT)

    expect(rowOf(message)).toBe(before)
  })

  it('保留 aria-live / aria-label / 级别的文字表达', () => {
    resetStore([{ ...makeLog(0), level: 'warn' }])
    render(<RunLog />)

    const list = document.querySelector('ol.logs')
    expect(list?.getAttribute('aria-live')).toBe('polite')
    expect(list?.getAttribute('aria-label')).toBe('运行日志')
    expect(screen.getByText('WARN')).toBeTruthy()
    expect(list?.querySelector('li')?.className).toBe('log log--warn')
  })

  it('空数组仍渲染 panel-empty 空状态', () => {
    resetStore([])
    render(<RunLog />)

    const empty = document.querySelector('.panel-empty')
    expect(empty?.textContent).toContain('尚未启动 bot')
    expect(document.querySelector('ol.logs')).toBeNull()
    expect(document.querySelectorAll('li')).toHaveLength(0)
  })
})
describe('RunLog 来源区分', () => {
  it('Agent 行的 [agent] 前缀与普通 bot 行在视觉上区分开', () => {
    resetStore([
      { level: 'info', source: '[agent]', message: '[session 3f2a…] agent 工具 fs_read 成功（1 ms）', at: 1_700_000_000_000 },
      { level: 'info', source: 'bot-alpha', message: '普通 run log', at: 1_700_000_000_001 },
    ])
    render(<RunLog />)

    expect(screen.getByText('[agent]')).toBeTruthy()
    expect(screen.getByText('[session 3f2a…] agent 工具 fs_read 成功（1 ms）')).toBeTruthy()
    expect(screen.getByText('普通 run log')).toBeTruthy()
  })
})

describe('RunLog 筛选（从已删的整页面日志面搬进来）', () => {
  it('关键词只留匹配行，并给出匹配/总数', () => {
    resetStore([
      { level: 'info', source: 'bot', message: '启动完成', at: 1 },
      { level: 'error', source: 'bot', message: 'HBOT_JOBS_FAILED: 没有 onJob', at: 2 },
    ])
    render(<RunLog />)

    fireEvent.change(screen.getByLabelText('按内容筛选日志'), { target: { value: 'hbot' } })

    expect(document.querySelectorAll('li')).toHaveLength(1)
    expect(document.querySelector('.logs-toolbar__meta')?.textContent).toContain('1 / 2')
  })

  it('级别按钮是并集开关；全部关掉等于不按级别过滤', () => {
    resetStore([
      { level: 'info', source: 'bot', message: '一', at: 1 },
      { level: 'error', source: 'bot', message: '二', at: 2 },
      { level: 'warn', source: 'bot', message: '三', at: 3 },
    ])
    render(<RunLog />)

    fireEvent.click(screen.getByRole('button', { name: /^错误/ }))
    expect(document.querySelectorAll('li')).toHaveLength(1)

    fireEvent.click(screen.getByRole('button', { name: /^警告/ }))
    expect(document.querySelectorAll('li')).toHaveLength(2)

    // 再点一次错误：取消 -> 只剩警告。
    fireEvent.click(screen.getByRole('button', { name: /^错误/ }))
    expect(document.querySelectorAll('li')).toHaveLength(1)

    fireEvent.click(screen.getByRole('button', { name: /^警告/ }))
    expect(document.querySelectorAll('li')).toHaveLength(3)
  })

  it('筛选后没有匹配时显示可执行的空态，而不是空白列表', () => {
    resetStore([{ level: 'info', source: 'bot', message: '一', at: 1 }])
    render(<RunLog />)

    fireEvent.change(screen.getByLabelText('按内容筛选日志'), { target: { value: 'zzz' } })

    expect(document.querySelector('.panel-empty')?.textContent).toContain('没有匹配的日志')
    expect(document.querySelectorAll('li')).toHaveLength(0)
  })
})
