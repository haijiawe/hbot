// @vitest-environment jsdom
/**
 * 问题面板的两个动作（`features/problems/problem-actions.ts` + `problems-panel.tsx`）。
 *
 * 覆盖的是**跨模块契约**，不是样式：
 * - Fix 只对机械可修的 code 出现（未知 code 一律没有按钮）；
 * - Fix 真的把缓冲区改对，但**绝不写盘**（没有 `fs.write`），文件因此正确变脏；
 * - 请求 Agent 修复的载荷里必须同时有 file / line / code / message；
 * - 批量请求只在"当次全部是 error"时可点；
 * - 有未保存改动时面板上有可见提示，请求文本里也带着它。
 */

import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, Diagnostic } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { ProblemsPanel } from './problems-panel'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'agent.send') return { sessionId: 'session-1' }
    if (command.type === 'diagnostics') return []
    if (command.type === 'fs.read') {
      // 这里的字面量不能引用外部常量：`vi.mock` 的工厂在模块求值前就被提升执行。
      return {
        path: command.path,
        content: ["import { log } from 'hbot'", "import { util } from 'lib/util'", '', 'log(util)'].join(
          '\n',
        ),
        version: 1,
      }
    }
    return null
  })
  return { ...actual, dispatch: stub }
})

const BARE_IMPORT_MESSAGE =
  "bare module specifier 'lib/util' is not allowed; use a relative import ('./x') or the built-in 'hbot' module"

const SOURCE = ["import { log } from 'hbot'", "import { util } from 'lib/util'", '', 'log(util)'].join(
  '\n',
)

function bareImport(patch: Partial<Diagnostic> = {}): Diagnostic {
  return {
    path: 'src/plugins/ping.ts',
    line: 2,
    column: 1,
    severity: 'error',
    message: BARE_IMPORT_MESSAGE,
    code: 'HBOT_BARE_IMPORT',
    ...patch,
  }
}

function diagnostic(patch: Partial<Diagnostic> = {}): Diagnostic {
  return {
    path: 'src/main.ts',
    line: 4,
    column: 1,
    severity: 'error',
    message: '类型不匹配',
    code: 'TS2322',
    ...patch,
  }
}

function ingest(path: string, items: Diagnostic[]): void {
  act(() => useWorkspaceStore.getState().ingestEvent({ type: 'diagnostics', path, items }))
}

function commandsOf<T extends Command['type']>(type: T): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

function sentTexts(): string[] {
  return commandsOf('agent.send').map((command) => String(command.text))
}

async function renderPanel(): Promise<void> {
  render(<ProblemsPanel />)
  await waitFor(() => expect(commandsOf('diagnostics')).toEqual([{ path: null }]))
  await act(async () => {})
}

beforeEach(() => {
  useWorkspaceStore.setState({
    // 挂载时的第一次 diagnostics 要求项目已打开（没有项目时面板不发，见 problems-panel）。
    projectRoot: '/proj',
    contents: {},
    saved: {},
    tabs: [],
    activePath: null,
    reveal: null,
    error: null,
    diagnostics: [],
  })
  useAgentStore.setState({
    sessionId: null,
    messages: [],
    draft: '',
    sending: false,
    step: 0,
    tool: null,
    edit: null,
    approval: null,
    error: null,
    streamId: null,
  })
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('Fix：只对机械可修的 code 出现', () => {
  it('HBOT_BARE_IMPORT 那条有 Fix 按钮', async () => {
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    expect(screen.getByRole('button', { name: '机械修复：src/plugins/ping.ts:2' })).toBeTruthy()
  })

  it('未知 code（TS2322）没有 Fix 按钮（判据是穷举白名单，不是"看起来能改"）', async () => {
    await renderPanel()
    ingest('src/main.ts', [diagnostic()])

    expect(screen.queryByRole('button', { name: /^机械修复/ })).toBeNull()
    // 但「请求 Agent 修复」仍然在：让模型自己判断怎么改。
    expect(screen.getByRole('button', { name: '请求 Agent 修复：src/main.ts:4' })).toBeTruthy()
  })
})

describe('Fix：改缓冲区，不写盘', () => {
  it('把裸 import 改成相对导入，saved 不动、文件变脏、没有 fs.write', async () => {
    const user = userEvent.setup()
    useWorkspaceStore.setState({
      contents: { 'src/plugins/ping.ts': SOURCE },
      saved: { 'src/plugins/ping.ts': SOURCE },
      tabs: [{ path: 'src/plugins/ping.ts', dirty: false, version: 1 }],
      activePath: 'src/plugins/ping.ts',
    })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '机械修复：src/plugins/ping.ts:2' }))

    await waitFor(() => {
      const store = useWorkspaceStore.getState()
      expect(store.contents['src/plugins/ping.ts']).toContain("from './lib/util'")
      // 磁盘基线没动 = 真正的"只改缓冲区"。
      expect(store.saved['src/plugins/ping.ts']).toBe(SOURCE)
      expect(store.tabs[0]?.dirty).toBe(true)
    })
    // 铁律：机械修复不经核心，绝不写盘。
    expect(commandsOf('fs.write')).toEqual([])
  })

  it('修好后给出 role=status 回执，说明只改了缓冲区', async () => {
    const user = userEvent.setup()
    useWorkspaceStore.setState({
      contents: { 'src/plugins/ping.ts': SOURCE },
      saved: { 'src/plugins/ping.ts': SOURCE },
      tabs: [{ path: 'src/plugins/ping.ts', dirty: false, version: 1 }],
    })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '机械修复：src/plugins/ping.ts:2' }))

    await waitFor(() => expect(screen.getByRole('status').textContent).toContain('已修正'))
    expect(screen.getByRole('status').textContent).toContain('缓冲区')
  })

  it('修不了时（报错行里没有那个裸名）走 role=alert，不静默改写', async () => {
    const user = userEvent.setup()
    const unrelated = "const x = 1\nimport { util } from 'lib/other'"
    useWorkspaceStore.setState({
      contents: { 'src/plugins/ping.ts': unrelated },
      saved: { 'src/plugins/ping.ts': unrelated },
      tabs: [{ path: 'src/plugins/ping.ts', dirty: false, version: 1 }],
    })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '机械修复：src/plugins/ping.ts:2' }))

    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('请求 Agent 修复'))
    expect(useWorkspaceStore.getState().contents['src/plugins/ping.ts']).toBe(unrelated)
  })
})

describe('请求 Agent 修复：载荷', () => {
  it('一条请求里同时有 file / line / code / message', async () => {
    const user = userEvent.setup()
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '请求 Agent 修复：src/plugins/ping.ts:2' }))

    await waitFor(() => expect(sentTexts()).toHaveLength(1))
    const text = sentTexts()[0]
    expect(text).toContain('src/plugins/ping.ts')
    expect(text).toContain('src/plugins/ping.ts:2:1')
    expect(text).toContain('HBOT_BARE_IMPORT')
    expect(text).toContain(BARE_IMPORT_MESSAGE)
    // 走的是既有 agent 通道：user 消息进会话列表，Diff 仍由 diff-card 审阅。
    expect(useAgentStore.getState().messages.at(-1)?.role).toBe('user')
  })

  it('Agent 正在处理上一条时不发（send 会静默丢弃），并在面板上说明', async () => {
    const user = userEvent.setup()
    useAgentStore.setState({ sending: true })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '请求 Agent 修复：src/plugins/ping.ts:2' }))

    await waitFor(() =>
      expect(screen.getByRole('alert').textContent).toContain('正在处理上一条消息'),
    )
    expect(sentTexts()).toEqual([])
  })
})

describe('批量请求 Agent 修复', () => {
  it('全部是 error 时可点，一条请求里列出全部', async () => {
    const user = userEvent.setup()
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])
    ingest('src/main.ts', [diagnostic({ message: '找不到名字' })])

    const batch = screen.getByRole('button', { name: '全部请求 Agent 修复（2 条）' })
    expect((batch as HTMLButtonElement).disabled).toBe(false)

    await user.click(batch)

    await waitFor(() => expect(sentTexts()).toHaveLength(1))
    expect(sentTexts()[0]).toContain('HBOT_BARE_IMPORT')
    expect(sentTexts()[0]).toContain('src/main.ts:4:1')
    expect(sentTexts()[0]).toContain('找不到名字')
  })

  it('混进一条 warning 就禁用（不替用户顺手改掉他没打算改的东西）', async () => {
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])
    ingest('src/main.ts', [
      diagnostic({ severity: 'warning', message: '未使用的变量', code: 'HBOT_COMPILE' }),
    ])

    const batch = screen.getByRole('button', { name: '全部请求 Agent 修复（2 条）' })
    expect((batch as HTMLButtonElement).disabled).toBe(true)
  })

  it('只有 error 时才可点的判据也接受"单条 error"这种平凡情况', async () => {
    await renderPanel()
    ingest('src/main.ts', [diagnostic()])

    const batch = screen.getByRole('button', { name: '全部请求 Agent 修复（1 条）' })
    expect((batch as HTMLButtonElement).disabled).toBe(false)
  })
})

describe('未保存缓冲区：必须说出来', () => {
  it('有脏文件时面板上有 role=note 提示并点名文件', async () => {
    useWorkspaceStore.setState({
      contents: { 'src/plugins/ping.ts': SOURCE },
      saved: { 'src/plugins/ping.ts': 'old' },
      tabs: [{ path: 'src/plugins/ping.ts', dirty: true, version: 2 }],
    })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    const note = screen.getByRole('note')
    expect(note.textContent).toContain('未保存')
    expect(note.textContent).toContain('src/plugins/ping.ts')
  })

  it('请求文本里也带着未保存提示（模型据此以磁盘为基准）', async () => {
    const user = userEvent.setup()
    useWorkspaceStore.setState({
      contents: { 'src/plugins/ping.ts': SOURCE },
      saved: { 'src/plugins/ping.ts': 'old' },
      tabs: [{ path: 'src/plugins/ping.ts', dirty: true, version: 2 }],
    })
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    await user.click(screen.getByRole('button', { name: '请求 Agent 修复：src/plugins/ping.ts:2' }))

    await waitFor(() => expect(sentTexts()).toHaveLength(1))
    expect(sentTexts()[0]).toContain('未保存')
    expect(sentTexts()[0]).toContain('磁盘')
  })

  it('没有脏文件时不显示那条提示（不喊狼来了）', async () => {
    await renderPanel()
    ingest('src/plugins/ping.ts', [bareImport()])

    expect(screen.queryByRole('note')).toBeNull()
  })
})