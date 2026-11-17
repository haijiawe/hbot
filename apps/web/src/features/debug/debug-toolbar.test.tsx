// @vitest-environment jsdom
/**
 * 调试工具条（`features/debug/debug-toolbar.tsx`）。
 *
 * 工具条不自己做事，只把点击翻译成命令。这里钉三件容易回归的：
 * 1. 没有暂停现场时五个按钮全禁用（避免对着空气发 resume）；
 * 2. 继续 / 单步三种各自 dispatch 什么；
 * 3. **停止的顺序**：先逐文件 `set-breakpoints{breakpoints:[]}`，再 `debug.resume`。
 *    反过来的话 worker 会在断点清掉之前撞上下一个断点，白停一次（见 debug-store 注释）。
 *
 * 注意：`useDebugStore.setState(...)` 必须在 `render` **之前**（或包在 `act` 里）——
 * zustand 的更新走 useSyncExternalStore，render 之后再裸调 setState 不会同步刷新 DOM。
 */

import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useDebugStore } from '../../stores/debug-store'
import { DebugToolbar } from './debug-toolbar'

const BOT_ID = 'bot-1'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  return { ...actual, dispatch: vi.fn(async () => null) }
})

function sent(): Record<string, unknown>[] {
  return vi.mocked(dispatch).mock.calls.map(([command]: [Command]) => ({ ...command }))
}

const PAUSED = {
  file: 'src/handlers/message.ts',
  line: 7,
  condition: null,
  scopes: [],
  callStack: [{ name: 'reply', file: 'src/handlers/message.ts', line: 7, column: 1 }],
}

const BUTTONS = ['继续', '单步跳过', '单步进入', '单步跳出', '停止'] as const

beforeEach(() => {
  useDebugStore.setState({
    botId: BOT_ID,
    breakpoints: {},
    paused: null,
    selectedFrame: 0,
    busy: false,
    error: null,
  })
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('DebugToolbar', () => {
  it('未在调试时五个按钮全禁用，状态写"未在调试"', () => {
    render(<DebugToolbar />)

    for (const name of BUTTONS) {
      expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled).toBe(true)
    }
    expect(screen.getByRole('status').textContent).toBe('未在调试')
  })

  it('暂停时状态显示 file:line，按钮可用', () => {
    useDebugStore.setState({ paused: PAUSED, selectedFrame: 0 })
    render(<DebugToolbar />)

    expect(screen.getByRole('status').textContent).toBe('src/handlers/message.ts:7')
    for (const name of BUTTONS) {
      expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled).toBe(false)
    }
  })

  it('忙碌时按钮禁用（一次只发一条调试命令）', () => {
    useDebugStore.setState({ paused: PAUSED, selectedFrame: 0, busy: true })
    render(<DebugToolbar />)

    for (const name of BUTTONS) {
      expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled).toBe(true)
    }
  })

  it('继续 → debug.resume{botId}', async () => {
    useDebugStore.setState({ paused: PAUSED, selectedFrame: 0 })
    const user = userEvent.setup()
    render(<DebugToolbar />)

    await user.click(screen.getByRole('button', { name: '继续' }))

    await waitFor(() => expect(sent()).toEqual([{ type: 'debug.resume', botId: BOT_ID }]))
    // 继续之后暂停现场结束：工具条回到"未在调试"，不会再发第二条 resume。
    expect(screen.getByRole('status').textContent).toBe('未在调试')
  })

  it.each([
    ['单步跳过', 'over'],
    ['单步进入', 'into'],
    ['单步跳出', 'out'],
  ])('%s → debug.step{kind:%s}', async (label, kind) => {
    useDebugStore.setState({ paused: PAUSED, selectedFrame: 0 })
    const user = userEvent.setup()
    render(<DebugToolbar />)

    await user.click(screen.getByRole('button', { name: label }))

    await waitFor(() => expect(sent()).toEqual([{ type: 'debug.step', botId: BOT_ID, kind }]))
    // 单步保留暂停现场（高亮停在原行），等核心推新事件。
    expect(screen.getByRole('status').textContent).toBe('src/handlers/message.ts:7')
  })

  it('停止：先逐文件清断点，再 resume（顺序不可反）', async () => {
    useDebugStore.setState({
      paused: PAUSED,
      selectedFrame: 0,
      breakpoints: {
        'src/a.ts': [
          { line: 3, condition: null },
          { line: 9, condition: 'count > 3' },
        ],
        'src/b.ts': [{ line: 5, condition: null }],
      },
    })
    const user = userEvent.setup()
    render(<DebugToolbar />)

    await user.click(screen.getByRole('button', { name: '停止' }))

    await waitFor(() =>
      expect(sent()).toEqual([
        { type: 'debug.set-breakpoints', botId: BOT_ID, file: 'src/a.ts', breakpoints: [] },
        { type: 'debug.set-breakpoints', botId: BOT_ID, file: 'src/b.ts', breakpoints: [] },
        { type: 'debug.resume', botId: BOT_ID },
      ]),
    )
    expect(useDebugStore.getState().breakpoints).toEqual({})
  })

  it('按钮 title 里带真实快捷键（唯一真源是 lib/keybindings.ts）', () => {
    render(<DebugToolbar />)

    expect(screen.getByRole('button', { name: '继续' }).title).toMatch(/^继续（.+）$/)
    expect(screen.getByRole('button', { name: '单步跳过' }).title).toMatch(/^单步跳过（.+）$/)
    expect(screen.getByRole('button', { name: '停止' }).title).toContain('清除断点')
  })
})