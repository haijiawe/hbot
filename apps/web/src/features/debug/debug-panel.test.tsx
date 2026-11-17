// @vitest-environment jsdom
/**
 * 调试面板（`features/debug/debug-panel.tsx`）。
 *
 * 面板只显示核心推来的暂停现场：调用栈 + scopes 变量。两条容易搞错的契约：
 * 1. **`callStack` 的最后一项才是当前帧**（外层在前），所以刚暂停时选中的是最后一项；
 * 2. 点某一帧跳转用的是**被点那一帧**的 file/line，不是暂停点本身的。
 *
 * 断言的是 DOM（可见文案、aria-selected）与 `openAt` 的对外效果（activePath + reveal），
 * 不去读 store 的内部字段。
 */

import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DebugScope, StackFrame } from '../../generated'
import type { Command, CoreEvent } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useDebugStore } from '../../stores/debug-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { DebugPanel } from './debug-panel'

const BOT_ID = 'bot-1'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  // 点帧会走 `openAt` → `fs.read`；其余命令回 null。
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'fs.read') {
      return { path: command.path, content: 'const a = 1\n', version: 1 }
    }
    return null
  })
  return { ...actual, dispatch: stub }
})

function frame(name: string, file: string, line: number, column = 1): StackFrame {
  return { name, file, line, column }
}

function scope(name: string, variables: Record<string, unknown>): DebugScope {
  return { name, variables }
}

const STACK: StackFrame[] = [
  frame('handleMessage', 'src/main.ts', 12),
  frame('dispatch', 'src/lib/reply.ts', 30),
  frame('reply', 'src/handlers/message.ts', 7),
]

function pauseEvent(
  options: {
    botId?: string
    file?: string
    line?: number
    condition?: string | null
    scopes?: DebugScope[]
    callStack?: StackFrame[]
  } = {},
): CoreEvent {
  return {
    type: 'debug.paused',
    botId: options.botId ?? BOT_ID,
    file: options.file ?? 'src/handlers/message.ts',
    line: options.line ?? 7,
    condition: options.condition ?? null,
    scopes: options.scopes ?? [],
    callStack: options.callStack ?? STACK,
  }
}

function ingest(event: CoreEvent): void {
  act(() => useDebugStore.getState().ingestEvent(event))
}

beforeEach(() => {
  useDebugStore.setState({
    botId: BOT_ID,
    breakpoints: {},
    paused: null,
    selectedFrame: 0,
    busy: false,
    error: null,
  })
  useWorkspaceStore.setState({
    contents: {},
    saved: {},
    tabs: [],
    activePath: null,
    reveal: null,
    error: null,
    diagnostics: [],
  })
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('DebugPanel', () => {
  it('未暂停时是空状态，不是"空调用栈"', () => {
    render(<DebugPanel />)

    const section = screen.getByRole('region', { name: '调试' })
    expect(section.textContent).toContain('未在调试。')
    expect(section.textContent).not.toContain('调用栈为空。')
    expect(screen.queryByRole('listbox', { name: '调用栈' })).toBeNull()
  })

  it('暂停后渲染整条调用栈，默认选中**最后一项**（当前帧）', () => {
    render(<DebugPanel />)
    ingest(pauseEvent())

    const frames = screen.getAllByRole('option')
    expect(frames).toHaveLength(3)
    // 顺序就是事件给的顺序（外层在前），最后一项才是当前帧。
    expect(frames.map((item) => item.textContent)).toEqual([
      'handleMessagesrc/main.ts:12',
      'dispatchsrc/lib/reply.ts:30',
      'replysrc/handlers/message.ts:7',
    ])
    expect(frames.map((item) => item.getAttribute('aria-selected'))).toEqual([
      'false',
      'false',
      'true',
    ])
  })

  it('点外层帧：选中它，并把编辑器跳到**那一帧**的文件行', async () => {
    const user = userEvent.setup()
    render(<DebugPanel />)
    ingest(pauseEvent())

    await user.click(screen.getByText('handleMessage').closest('button') as HTMLButtonElement)

    // 选中态跟着点击走 —— 但跳转目标不受"当前帧"影响，用的是被点那一帧。
    await waitFor(() => {
      const frames = screen.getAllByRole('option')
      expect(frames[0]?.getAttribute('aria-selected')).toBe('true')
      expect(frames[2]?.getAttribute('aria-selected')).toBe('false')
    })

    await waitFor(() => {
      const workspace = useWorkspaceStore.getState()
      expect(workspace.activePath).toBe('src/main.ts')
      expect(workspace.reveal).toMatchObject({ path: 'src/main.ts', line: 12 })
    })
  })

  it('scopes 变量按 JSON 渲染，序列化不了的显示 <unserializable>', () => {
    const circular: Record<string, unknown> = {}
    circular.self = circular

    render(<DebugPanel />)
    ingest(
      pauseEvent({
        scopes: [
          scope('Local', { count: 3, ready: true, text: 'hi', missing: null }),
          scope('Broken', { circular }),
          scope('Empty', {}),
        ],
      }),
    )

    expect(screen.getByRole('region', { name: 'Local' }).textContent).toContain('count3')
    // 字符串按 JSON 显示（带引号）——这是内核推来的值的原样表示。
    expect(screen.getByText('"hi"')).toBeTruthy()
    expect(screen.getByText('null')).toBeTruthy()
    expect(screen.getByRole('region', { name: 'Broken' }).textContent).toContain(
      '<unserializable>',
    )
    expect(screen.getByRole('region', { name: 'Empty' }).textContent).toContain('（空）')
  })

  it('没有可显示的变量给一句提示，而不是空白面板', () => {
    render(<DebugPanel />)
    ingest(pauseEvent({ scopes: [] }))

    expect(screen.getByText('没有可显示的局部变量。')).toBeTruthy()
  })

  it('调用栈为空时说明"调用栈为空"，而不是假装暂停现场完整', () => {
    render(<DebugPanel />)
    ingest(pauseEvent({ callStack: [] }))

    expect(screen.getByText('调用栈为空。')).toBeTruthy()
  })

  it('面板里说明 <unserializable> 的含义与不支持 eval/watch', () => {
    render(<DebugPanel />)
    ingest(pauseEvent())

    const note = screen.getByText(/源码插桩执行/)
    expect(note.textContent).toContain('<unserializable>')
    expect(note.textContent).toContain('eval')
  })

  it('别的 bot 的暂停事件不改当前现场', () => {
    render(<DebugPanel />)
    ingest(pauseEvent({ file: 'src/a.ts', line: 3 }))
    ingest(pauseEvent({ botId: 'other-bot', file: 'src/other.ts', line: 99 }))

    const frames = screen.getAllByRole('option')
    expect(frames).toHaveLength(3)
    expect(screen.getByRole('region', { name: '调试' }).textContent).toContain('src/a.ts')
    expect(screen.getByRole('region', { name: '调试' }).textContent).not.toContain('src/other.ts')
  })

  it('暂停时显示命中的条件原文（条件断点 vs 无条件断点的区别就在这）', () => {
    render(<DebugPanel />)
    ingest(pauseEvent({ condition: 'count > 3 && state.ready' }))

    const panel = screen.getByRole('region', { name: '调试' })
    expect(panel.textContent).toContain('命中的条件')
    expect(panel.textContent).toContain('count > 3 && state.ready')
  })

  it('无条件断点 / 单步暂停不显示命中的条件（condition 是 null）', () => {
    render(<DebugPanel />)
    ingest(pauseEvent({ condition: null }))

    expect(screen.getByRole('region', { name: '调试' }).textContent).not.toContain('命中的条件')
  })

  it('断点清单显示行号与条件原文，没有条件的行显示「无条件」', () => {
    useDebugStore.setState({
      breakpoints: {
        'src/main.ts': [
          { line: 3, condition: null },
          { line: 12, condition: 'count > 3' },
        ],
      },
    })
    render(<DebugPanel />)

    const list = screen.getByRole('region', { name: '断点' })
    expect(list.textContent).toContain('src/main.ts')
    expect(list.textContent).toContain('第 3 行')
    expect(list.textContent).toContain('第 12 行')
    expect(list.textContent).toContain('无条件')
    expect(list.textContent).toContain('count > 3')
  })

  it('点断点清单里的一行：编辑器跳到那个文件行', async () => {
    const user = userEvent.setup()
    useDebugStore.setState({ breakpoints: { 'src/main.ts': [{ line: 12, condition: null }] } })
    render(<DebugPanel />)

    await user.click(screen.getByText('第 12 行').closest('button') as HTMLButtonElement)

    await waitFor(() => {
      const workspace = useWorkspaceStore.getState()
      expect(workspace.activePath).toBe('src/main.ts')
      expect(workspace.reveal).toMatchObject({ path: 'src/main.ts', line: 12 })
    })
  })
})
