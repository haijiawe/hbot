// @vitest-environment jsdom
/**
 * 整页面「开发日志」的窗口化回归（组件级，jsdom）。
 *
 * 为什么必须有一条这样的测试：`visibleWindow()` 的数学有单测，但「组件有没有真的
 * 用它」是另一件事 —— 把渲染改回 `visible.map(...)` 不会让任何单测变红，
 * 只会让一万行日志重新变成一万个 DOM 节点（滚一下卡住，用户以为应用挂了）。
 *
 * 断言两条：① 渲染出的 `<li>` 数量是窗口级的（远小于 10000）；
 * ② 第一行是最新那条（最新在上，退化窗口贴 start）。
 */

import { render } from '@testing-library/react'
import { beforeEach, describe, expect, it } from 'vitest'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { LogsView } from './logs-view'

const TOTAL = 10_000

function seed() {
  const logs = Array.from({ length: TOTAL }, (_, index) => ({
    level: 'info' as const,
    source: 'bot',
    message: 'line-' + index,
    at: 1_700_000_000_000 + index,
  }))
  useRunStore.setState({ logs })
  useWorkspaceStore.setState({ projectRoot: 'E:/demo' })
}

describe('LogsView 窗口化', () => {
  beforeEach(seed)

  it('一万行日志只渲染窗口内的行，且首行是最新那条', () => {
    const { container } = render(<LogsView />)
    const rows = container.querySelectorAll("li.wb-log__line")

    expect(rows.length).toBeGreaterThan(0)
    expect(rows.length).toBeLessThanOrEqual(40)
    expect(rows.length).toBeLessThan(TOTAL / 100)

    expect(rows[0]?.textContent).toContain("line-" + (TOTAL - 1))
  })
})
