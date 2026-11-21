/**
 * `lib/log-window.ts` 的纯函数单测（额外覆盖两个日志面各自的行高/缓冲）。
 *
 * 默认行高那套语义已由 `features/run/run-log-virtual.test.ts` 钉住（它现在走的是
 * 本文件的 re-export）；这里补的是**整页面**用到的那半边：自定义行高、最新在上的
 * 下标映射、以及「停在顶部算跟随」。
 */

import { describe, expect, it } from 'vitest'
import {
  LOG_ROW_HEIGHT,
  isAtTop,
  logWindow,
  newestFirstIndex,
  visibleWindow,
} from './log-window'

describe('visibleWindow', () => {
  it('自定义行高时窗口按显示顺序算，总高度 = 行数 × 该行高', () => {
    const rowHeight = 20
    const window_ = visibleWindow(500, 400, rowHeight * 5, { rowHeight, overscan: 3 })

    expect(window_.totalHeight).toBe(500 * rowHeight)
    expect(window_.start).toBe(20 - 3)
    expect(window_.end).toBe(20 + 5 + 3)
  })

  it('没有传行高时退回页签的默认值 18', () => {
    expect(visibleWindow(10, 0, LOG_ROW_HEIGHT * 2).totalHeight).toBe(10 * LOG_ROW_HEIGHT)
  })

  it('行数比视口少时窗口就是全部，且不会算出负起点', () => {
    const window_ = visibleWindow(3, 9999, 400, { rowHeight: 20 })
    expect(window_).toEqual({ start: 0, end: 3, totalHeight: 60 })
  })

  it('非法行数（负数/小数/NaN 之外）都夹到 0', () => {
    expect(visibleWindow(-1, 0, 100)).toEqual({ start: 0, end: 0, totalHeight: 0 })
    expect(visibleWindow(2.9, 0, 100, { rowHeight: 20 }).totalHeight).toBe(40)
  })
})

  it('fallbackAnchor 决定退化窗口贴哪一头', () => {
    const start = visibleWindow(1000, 0, 0, { fallbackAnchor: 'start', fallbackRows: 20 })
    expect(start.start).toBe(0)
    expect(start.end).toBe(20)

    const end = visibleWindow(1000, 0, 0, { fallbackRows: 20 })
    expect(end.start).toBe(980)
    expect(end.end).toBe(1000)
  })
describe('newestFirstIndex', () => {
  it('显示序号 0 对应源数组的最后一行', () => {
    expect(newestFirstIndex(5, 0)).toBe(4)
    expect(newestFirstIndex(5, 4)).toBe(0)
  })
})

describe('isAtTop', () => {
  it('顶部 4px 余量以内算在顶（最新在最上时继续跟随）', () => {
    expect(isAtTop(0)).toBe(true)
    expect(isAtTop(3)).toBe(true)
    expect(isAtTop(40)).toBe(false)
  })
})

describe('logWindow 与 visibleWindow 一致', () => {
  it('默认参数下两者给出同一个窗口', () => {
    const a = logWindow(1000, 300, 200)
    const b = visibleWindow(1000, 300, 200)
    expect(a).toEqual(b)
  })
})
