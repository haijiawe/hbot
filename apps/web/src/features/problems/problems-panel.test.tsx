// @vitest-environment jsdom
/**
 * 问题面板（`features/problems/problems-panel.tsx`）。
 *
 * 数据来源两条，都是核心侧（`crates/hbot-rpc` 的 `diagnostics` 命令 / `diagnostics` 事件）：
 * 1. 面板挂载时发一次 `diagnostics{path:null}`，返回值就是第一批（首帧兜底）；
 * 2. 之后核心每次编译都会推 `diagnostics{path, items}`，**同一 path 是整批覆盖**，
 *    不同 path 才并存 —— 所以 merge 不能简单 push。
 *
 * docs/06-ide.md §3.1 要求"问题面板可点击跳转"，所以这里断言点一下真的打开文件并
 * 把光标移到那一行；两类条目跳不了（项目级 `path === ''`、位置未知 `line <= 0`）必须在
 * 界面上明说，而不是点了没反应。
 */

import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, Diagnostic } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { ProblemsPanel } from './problems-panel'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'fs.read') {
      return { path: command.path, content: 'const a = 1\n', version: 1 }
    }
    if (command.type === 'diagnostics') return []
    return null
  })
  return { ...actual, dispatch: stub }
})

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

/**
 * 渲染并等挂载时那次 `diagnostics` 请求**落地**。
 *
 * 等它落地是必须的：响应回来时会把 `diagnostics` 覆盖成它拿到的那一批，如果测试在
 * 响应之前就 ingest 事件，等于同时验"事件赢"（那是下面单独一条用例的事）。
 */
async function renderPanel(): Promise<void> {
  render(<ProblemsPanel />)
  await waitFor(() => expect(commandsOf('diagnostics')).toEqual([{ path: null }]))
  await act(async () => {})
}

beforeEach(() => {
  useWorkspaceStore.setState({
    // 挂载时的第一次 diagnostics 要求项目已打开（没有项目时不该发，见 problems-panel）。
    projectRoot: '/proj',
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

describe('ProblemsPanel', () => {
  it('挂载时向核心要一次诊断（否则真机上永远是空面板）', async () => {
    render(<ProblemsPanel />)

    await waitFor(() => expect(commandsOf('diagnostics')).toEqual([{ path: null }]))
  })

  it('没有诊断时是空状态文案', async () => {
    render(<ProblemsPanel />)

    await waitFor(() => expect(screen.getByRole('status').textContent).toBe('暂无问题。'))
    expect(screen.queryByRole('list', { name: '诊断列表' })).toBeNull()
  })

  it('渲染严重级 / 消息 / 位置 / 错误码', async () => {
    await renderPanel()
    ingest('src/main.ts', [diagnostic()])

    const item = screen.getByRole('button', { name: '错误：类型不匹配（src/main.ts:4）' })
    expect(item.textContent).toContain('错误')
    expect(item.textContent).toContain('类型不匹配')
    expect(item.textContent).toContain('src/main.ts:4')
    expect(item.textContent).toContain('TS2322')
    expect(item.title).toBe('类型不匹配')
  })

  it('按严重级排序：错误在警告/提示之前', async () => {
    await renderPanel()
    ingest('', [
      diagnostic({ path: '', line: 0, severity: 'hint', message: '可以更简洁', code: null }),
      diagnostic({ path: 'src/a.ts', line: 9, severity: 'warning', message: '未使用的变量' }),
      diagnostic({ path: 'src/b.ts', line: 2, severity: 'error', message: '找不到名字' }),
    ])

    const items = screen.getAllByRole('listitem')
    expect(items.map((item) => item.textContent?.slice(0, 2))).toEqual(['错误', '警告', '提示'])
  })

  it('点击一条诊断：打开那个文件并跳到那一行', async () => {
    const user = userEvent.setup()
    await renderPanel()
    ingest('src/main.ts', [diagnostic({ path: 'src/main.ts', line: 4 })])

    await user.click(screen.getByRole('button', { name: /错误：类型不匹配/ }))

    await waitFor(() => {
      const workspace = useWorkspaceStore.getState()
      expect(workspace.activePath).toBe('src/main.ts')
      expect(workspace.reveal).toMatchObject({ path: 'src/main.ts', line: 4 })
    })
    await waitFor(() => expect(commandsOf('fs.read')).toEqual([{ path: 'src/main.ts' }]))
  })

  it('项目级诊断（path 为空）标明来源且不可跳转', async () => {
    await renderPanel()
    ingest('', [diagnostic({ path: '', line: 0, message: 'hbot.json 缺少 main', code: null })])

    const item = screen.getByRole('button', { name: /hbot\.json 缺少 main（（项目级））$/ })
    expect((item as HTMLButtonElement).disabled).toBe(true)
    expect(screen.getByText('（项目级）')).toBeTruthy()
  })

  it('位置未知（line 为 0）时只显示路径，不假装知道行号', async () => {
    await renderPanel()
    ingest('src/main.ts', [diagnostic({ path: 'src/main.ts', line: 0 })])

    const item = screen.getByRole('button', { name: /src\/main\.ts）/ })
    expect(item.textContent).not.toContain('src/main.ts:0')
    expect((item as HTMLButtonElement).disabled).toBe(true)
  })

  it('同一文件的第二批诊断整批覆盖，不同文件并存', async () => {
    await renderPanel()

    ingest('src/a.ts', [diagnostic({ path: 'src/a.ts', message: '旧的' })])
    expect(screen.getByText('旧的')).toBeTruthy()

    // 同一 path 的第二次编译结果 = 这个文件的全部诊断（旧的已修好，不再出现）。
    ingest('src/a.ts', [diagnostic({ path: 'src/a.ts', message: '新的' })])
    expect(screen.queryByText('旧的')).toBeNull()
    expect(screen.getByText('新的')).toBeTruthy()

    ingest('src/b.ts', [diagnostic({ path: 'src/b.ts', message: '另一个文件' })])
    expect(screen.getByText('新的')).toBeTruthy()
    expect(screen.getByText('另一个文件')).toBeTruthy()
  })

  it('没有打开项目时不发 diagnostics（否则首屏状态栏会挂一条 -32002 红字）', async () => {
    useWorkspaceStore.setState({ projectRoot: null })
    render(<ProblemsPanel />)
    // 给挂载后的微任务一轮机会：真发了就会出现在 dispatch 的调用里。
    await act(async () => {})
    expect(commandsOf('diagnostics')).toEqual([])
  })

  it('读取失败走 role=alert 一行错误，不把上一批诊断清空（清空等于撒谎）', async () => {
    vi.mocked(dispatch).mockImplementationOnce(async () => {
      throw new Error('核心未就绪')
    })
    render(<ProblemsPanel />)

    await waitFor(() =>
      expect(screen.getByRole('alert').textContent).toContain('读取诊断失败：核心未就绪'),
    )
  })

  it('请求还在路上时先到的事件不会被响应覆盖（旧快照不能把新诊断打回去）', async () => {
    // 核心在响应之前就会推同样的事件，挂载时的"拉一次"和后到的事件会撞车。
    let resolveRefresh: (items: Diagnostic[]) => void = () => {}
    vi.mocked(dispatch).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveRefresh = resolve
        }),
    )

    render(<ProblemsPanel />)
    await waitFor(() => expect(commandsOf('diagnostics')).toEqual([{ path: null }]))

    // 事件先到：这是比响应更新的数据。
    ingest('src/main.ts', [diagnostic({ message: '新诊断' })])
    expect(screen.getByText('新诊断')).toBeTruthy()

    // 响应后到，带的是过时快照（这个文件已经没有诊断了）。
    await act(async () => resolveRefresh([]))

    expect(screen.getByText('新诊断')).toBeTruthy()
  })
})

