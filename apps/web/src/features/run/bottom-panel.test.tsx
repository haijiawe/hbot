// @vitest-environment jsdom
/**
 * 底部面板的页签壳（`features/run/bottom-panel.tsx`）。
 *
 * 这里只钉"壳"的三件事，各页签内容由各自的测试负责：
 * 1. 问题页签上的徽标显示**诊断条数**（之前 problems/debug 都是不显示计数，
 *    问题页签等于没有反馈 —— 用户不知道有没有问题）；
 * 2. 切到问题页签时真的渲染 ProblemsPanel（而不是空壳）；
 * 3. 暂停时调试页签挂一个"已暂停"标记 —— 别的页签在看着的时候也能看见调试停住了。
 */

import { act, cleanup, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, Diagnostic } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useDebugStore } from '../../stores/debug-store'
import { useRunStore } from '../../stores/run-store'
import { INITIAL_SEARCH_STATE, useSearchStore } from '../../stores/search-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import BottomPanel from './bottom-panel'

/** 核心对 `diagnostics` 查询的回答（面板一挂载就会问一次，回答会覆盖 store 里的旧值）。 */
let diagnosticsResponse: Diagnostic[] = []

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'fs.read') {
      return { path: command.path, content: 'const a = 1\n', version: 1 }
    }
    if (command.type === 'diagnostics') return diagnosticsResponse
    // 作业面板一挂载就会问 bot 清单与两张快照；这里给空结果，
    // 它的内容由 `jobs-panel.test.tsx` 负责。
    if (command.type === 'bot.list') return []
    if (command.type === 'bot_jobs' || command.type === 'bot_waits') return []
    return null
  })
  return { ...actual, dispatch: stub }
})

function diagnostic(message: string): Diagnostic {
  return { path: 'src/main.ts', line: 4, column: 1, severity: 'error', message, code: 'TS2322' }
}

beforeEach(() => {
  useWorkspaceStore.setState({
    files: [],
    treeLoaded: true,
    contents: {},
    saved: {},
    tabs: [],
    activePath: null,
    reveal: null,
    error: null,
    diagnostics: [],
    bottomPanelTab: 'logs',
  })
  useRunStore.setState({ botId: '', status: 'stopped', logs: [], busy: false, error: null })
  useDebugStore.setState({
    botId: '',
    breakpoints: {},
    paused: null,
    selectedFrame: 0,
    busy: false,
    error: null,
  })
  // 搜索面板一挂载就会读这些字段，重置回初始值免得跨用例串味。
  useSearchStore.setState({ ...INITIAL_SEARCH_STATE })
  diagnosticsResponse = []
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('BottomPanel 页签', () => {
  it('问题页签的徽标是诊断条数', () => {
    useWorkspaceStore.setState({ diagnostics: [diagnostic('一'), diagnostic('二')] })
    render(<BottomPanel />)

    const tab = screen.getByRole('tab', { name: '问题' })
    expect(tab.querySelector('.panel-tab__count')?.textContent).toBe('2')
    expect(tab.querySelector('.panel-tab__count')?.getAttribute('aria-label')).toBe('2 项')
  })

  it('切到问题页签渲染 ProblemsPanel 并列出诊断', async () => {
    const user = userEvent.setup()
    diagnosticsResponse = [diagnostic('类型不匹配')]
    useWorkspaceStore.setState({ diagnostics: [diagnostic('类型不匹配')] })
    render(<BottomPanel />)

    await user.click(screen.getByRole('tab', { name: '问题' }))

    expect(useWorkspaceStore.getState().bottomPanelTab).toBe('problems')
    expect(await screen.findByRole('button', { name: /错误：类型不匹配/ })).toBeTruthy()
  })

  it('切到作业页签渲染作业与等待面板', async () => {
    const user = userEvent.setup()
    render(<BottomPanel />)

    await user.click(screen.getByRole('tab', { name: '作业' }))

    expect(useWorkspaceStore.getState().bottomPanelTab).toBe('jobs')
    // 面板是真的挂上了（空态文案来自它自己），不是空壳。
    expect(await screen.findByLabelText('作业与等待')).toBeTruthy()
    expect(await screen.findByText(/这个 bot 还没有作业记录/)).toBeTruthy()
  })

  it('暂停时调试页签挂"已暂停"标记，即使当前在看别的页签', () => {
    useDebugStore.setState({
      botId: 'bot-1',
      paused: {
        file: 'src/main.ts',
        line: 3,
        condition: null,
        scopes: [],
        callStack: [{ name: 'main', file: 'src/main.ts', line: 3, column: 1 }],
      },
    })
    render(<BottomPanel />)

    expect(screen.getByRole('tab', { name: '调试' }).querySelector('.panel-tab__dot')).toBeTruthy()
    expect(screen.getAllByLabelText('已暂停')).toHaveLength(1)
  })

  it('没有诊断时问题页签显示 0 项（与日志的「0 行」、搜索的「0 处匹配」同一口径）', () => {
    render(<BottomPanel />)

    const tab = screen.getByRole('tab', { name: '问题' })
    expect(tab.querySelector('.panel-tab__count')?.textContent).toBe('0')
    expect(tab.querySelector('.panel-tab__count')?.getAttribute('aria-label')).toBe('0 项')
  })

  it('调试页签不挂计数（它的信息是"已暂停"圆点，不是条数）', () => {
    render(<BottomPanel />)

    const tab = screen.getByRole('tab', { name: '调试' })
    expect(tab.querySelector('.panel-tab__count')).toBeNull()
  })
})

describe('BottomPanel 页签切换（state → aria）', () => {
  it('当前页签 aria-selected 为 true，面板 aria-labelledby 跟着切', async () => {
    const user = userEvent.setup()
    render(<BottomPanel />)

    expect(screen.getByRole('tab', { name: '日志' }).getAttribute('aria-selected')).toBe('true')

    await user.click(screen.getByRole('tab', { name: '搜索' }))

    expect(screen.getByRole('tab', { name: '搜索' }).getAttribute('aria-selected')).toBe('true')
    expect(screen.getByRole('tab', { name: '日志' }).getAttribute('aria-selected')).toBe('false')
    expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe('bottom-tab-search')

    // 搜索面板确实挂上了（它的表单一出现就 focus 输入框）。
    expect(screen.getByLabelText('搜索内容')).toBeTruthy()
    act(() => {})
  })
})
