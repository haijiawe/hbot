/**
 * 窗口化计算（`features/run/run-log-virtual.ts`）的纯函数单测。
 *
 * 这里不碰 DOM：窗口边界、贴底判定、稳定 key 都是纯计算，用默认的 node 环境跑
 * （jsdom 里 `scrollHeight`/`clientHeight` 恒为 0，真布局在那里验不了）。
 */

import { describe, expect, it } from 'vitest'
import type { LogLine } from '../../lib/ipc'
import {
  LOG_FALLBACK_ROWS,
  LOG_OVERSCAN,
  LOG_ROW_HEIGHT,
  isAtBottom,
  logLineKey,
  logWindow,
} from './run-log-virtual'

function line(at: number, message = 'm'): LogLine {
  return { level: 'info', source: 'bot', message, at }
}

describe('logWindow', () => {
  it('可视区渲染「可视行数 + 上下缓冲」，totalHeight 是行数 × 行高', () => {
    const viewport = LOG_ROW_HEIGHT * 10
    const window_ = logWindow(10_000, 5000 * LOG_ROW_HEIGHT, viewport)

    expect(window_.totalHeight).toBe(10_000 * LOG_ROW_HEIGHT)
    expect(window_.start).toBe(5000 - LOG_OVERSCAN)
    expect(window_.end).toBe(5000 + 10 + LOG_OVERSCAN)
    expect(window_.end - window_.start).toBe(10 + LOG_OVERSCAN * 2)
  })

  it('顶部的窗口不会出现负下标，底部的窗口不会越过行数', () => {
    const viewport = LOG_ROW_HEIGHT * 10
    expect(logWindow(1000, 0, viewport).start).toBe(0)
    // 滚到最底部（滚动值可能被程序写成超范围）：窗口仍以最后一行为界。
    const bottom = logWindow(1000, 999_999, viewport)
    expect(bottom.end).toBe(1000)
    expect(bottom.start).toBeGreaterThanOrEqual(0)
  })

  it('还没测量到可视高度时退化成末尾若干行（默认贴底的语义）', () => {
    const window_ = logWindow(1000, 0, 0)
    expect(window_.end).toBe(1000)
    expect(window_.start).toBe(1000 - LOG_FALLBACK_ROWS)
    expect(logWindow(3, 0, 0)).toEqual({
      start: 0,
      end: 3,
      totalHeight: 3 * LOG_ROW_HEIGHT,
    })
  })

  it('零行没有窗口，也不给占位高度', () => {
    expect(logWindow(0, 0, 0)).toEqual({ start: 0, end: 0, totalHeight: 0 })
    expect(logWindow(-5, 100, 300)).toEqual({ start: 0, end: 0, totalHeight: 0 })
  })

  it('窗口永远不会比「可视行数 + 上下缓冲」更宽', () => {
    const viewport = LOG_ROW_HEIGHT * 20
    for (const scrollTop of [0, 1, 17, 18, 999, 100_000]) {
      const window_ = logWindow(5000, scrollTop, viewport)
      expect(window_.end - window_.start).toBeLessThanOrEqual(20 + LOG_OVERSCAN * 2)
    }
  })
})

describe('isAtBottom', () => {
  it('贴底（含亚像素余量）才算跟随', () => {
    expect(isAtBottom(1000 - 300, 1000, 300)).toBe(true)
    expect(isAtBottom(1000 - 300 - 3, 1000, 300)).toBe(true)
    expect(isAtBottom(1000 - 300 - 50, 1000, 300)).toBe(false)
  })

  it('内容比视口短时必然贴底', () => {
    expect(isAtBottom(0, 100, 300)).toBe(true)
  })
})

describe('logLineKey', () => {
  it('同一对象的键稳定，不同对象即使 at 相同也不同键', () => {
    const first = line(1)
    expect(logLineKey(first)).toBe(logLineKey(first))

    const second = line(1)
    expect(logLineKey(second)).not.toBe(logLineKey(first))
  })

  it('不依赖数组下标：整份数组重新拼接后既有行的键不变', () => {
    const first = line(1, 'a')
    const second = line(2, 'b')
    const before = logLineKey(second)
    const rebuilt = [first, second, line(3, 'c')]
    expect(logLineKey(rebuilt[1] as LogLine)).toBe(before)
  })
})
