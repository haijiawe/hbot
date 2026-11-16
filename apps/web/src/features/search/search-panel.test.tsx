// @vitest-environment jsdom
/**
 * 搜索面板（`features/search/*`）。
 *
 * 三条真契约（`crates/hbot-rpc` 的 `search_start` / `file.search`）：
 * 1. `search.start` 只回 `data: null`，命中靠 `file.search` 事件推；
 * 2. 同一个 `searchId` 的事件可以有多条，命中必须**追加**（不能覆盖上一次的）；
 * 3. 没有"搜索完成"事件，取消是唯一的收尾动作。
 *
 * 这里断言的是 DOM 可见结果 + 下发命令，不碰 store 内部字段。
 */

import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SearchMatch } from '../../generated'
import type { Command, CoreEvent } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { INITIAL_SEARCH_STATE, useSearchStore } from '../../stores/search-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { SearchPanel } from './search-panel'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  // `fs.read` 是点击结果后 `openAt` 的必经之路；其余命令回 `null` 即可（契约里本来就是 null）。
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'fs.read') {
      return { path: command.path, content: 'const a = 1\n', version: 1 }
    }
    return null
  })
  return { ...actual, dispatch: stub }
})

function match(path: string, line: number, text = 'hello world'): SearchMatch {
  return { path, line, column: 1, text }
}

function fileSearch(searchId: string, matches: SearchMatch[]): CoreEvent {
  return { type: 'file.search', searchId, matches }
}

function ingest(event: CoreEvent): void {
  act(() => useSearchStore.getState().ingestEvent(event))
}

/** 只留某类命令（`type` 之外的字段就是契约的全部）。 */
function commandsOf<T extends Command['type']>(type: T): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

async function submit(user: ReturnType<typeof userEvent.setup>, query: string): Promise<void> {
  const input = screen.getByLabelText('搜索内容')
  await user.clear(input)
  await user.type(input, query)
  await user.keyboard('{Enter}')
}

beforeEach(() => {
  useSearchStore.setState({ ...INITIAL_SEARCH_STATE })
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

describe('SearchPanel', () => {
  it('回车提交：下发 search.start，query / regex / globs 各自对位', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)

    await user.type(screen.getByLabelText('搜索内容'), 'hello')
    await user.click(screen.getByLabelText('按正则表达式搜索'))
    await user.type(screen.getByLabelText('文件过滤（glob，逗号分隔）'), 'src/**/*.ts, !vendor/**')
    await user.keyboard('{Enter}')

    await waitFor(() =>
      expect(commandsOf('search.start')).toEqual([
        { query: 'hello', regex: true, globs: ['src/**/*.ts', '!vendor/**'] },
      ]),
    )
    // 提交后请求已在路上但还没有第一条事件：界面必须说"正在搜索"，不能显示"没有匹配"。
    expect(screen.getByRole('status').textContent).toBe('正在搜索…')
  })

  it('空查询不往返核心：只报错，不发命令', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)

    // 空输入框里回车 = 提交一次空查询（隐式提交由 user-event 模拟）。
    await user.click(screen.getByLabelText('搜索内容'))
    await user.keyboard('{Enter}')

    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('请输入搜索内容。'))
    expect(commandsOf('search.start')).toEqual([])
  })

  it('结果按文件分组、同一 searchId 的多条事件逐条追加', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'hello')

    ingest(fileSearch('search-1', [match('src/a.ts', 3), match('src/a.ts', 9)]))
    expect(screen.getByText('2 处匹配 · 1 个文件')).toBeTruthy()

    // 第二条事件：同一文件新增一行 + 另一个文件 —— 追加而不是覆盖。
    ingest(fileSearch('search-1', [match('src/a.ts', 12), match('src/b.ts', 4)]))
    expect(screen.getByText('4 处匹配 · 2 个文件')).toBeTruthy()
    expect(screen.getByLabelText('3 处匹配')).toBeTruthy()
    expect(screen.getByLabelText('1 处匹配')).toBeTruthy()

    // 每一行都还在（3 条 a.ts + 1 条 b.ts = 4 个可点的命中行）。
    expect(screen.getAllByTitle(/^src\/[ab]\.ts:\d+$/)).toHaveLength(4)
  })

  it('点击一条命中：打开文件并把光标跳到那一行', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'hello')
    ingest(fileSearch('search-1', [match('src/a.ts', 33)]))

    await user.click(screen.getByTitle('src/a.ts:33'))

    await waitFor(() => {
      const workspace = useWorkspaceStore.getState()
      expect(workspace.activePath).toBe('src/a.ts')
      expect(workspace.reveal).toMatchObject({ path: 'src/a.ts', line: 33 })
    })
    // 打开文件必须真的走核心（fs.read），不能拿事件里的路径当内容。
    expect(commandsOf('fs.read')).toEqual([{ path: 'src/a.ts' }])
  })

  it('取消：拿到 searchId 之后点取消，核心收到 search.cancel', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'hello')
    ingest(fileSearch('search-1', [match('src/a.ts', 1)]))

    const cancel = screen.getByRole('button', { name: '取消' })
    expect((cancel as HTMLButtonElement).disabled).toBe(false)
    await user.click(cancel)

    await waitFor(() => expect(commandsOf('search.cancel')).toEqual([{ searchId: 'search-1' }]))
  })

  it('取消：还不知道 searchId 时只本地停收，不编一个假 ID 发出去', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'hello')

    // 命令刚发出、第一条事件还没到 —— 此时 UI 没有 ID 可用。
    const cancel = screen.getByRole('button', { name: '取消' })
    expect((cancel as HTMLButtonElement).disabled).toBe(false)
    await user.click(cancel)

    expect(commandsOf('search.cancel')).toEqual([])
    expect(screen.getByRole('status').textContent).toBe('输入关键词后按回车开始搜索。')
  })

  it('取消后到达的事件不再污染结果区（stale searchId 一律丢弃）', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'hello')
    ingest(fileSearch('search-1', [match('src/a.ts', 1)]))
    await user.click(screen.getByRole('button', { name: '取消' }))

    ingest(fileSearch('search-1', [match('src/a.ts', 2)]))

    // 取消时已经拿到的命中保留（不能凭空清空用户看得见的东西），但不能再多一行。
    expect(screen.getByText('1 处匹配 · 1 个文件')).toBeTruthy()
    expect(screen.getAllByTitle(/^src\/a\.ts:\d+$/)).toHaveLength(1)
    expect(screen.queryByTitle('src/a.ts:2')).toBeNull()
  })

  it('没有匹配时文案引用的是**已提交**的查询，不是输入框里正在改的词', async () => {
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'foo')
    ingest(fileSearch('search-1', []))

    expect(screen.getByRole('status').textContent).toBe('没有匹配「foo」。')

    // 用户开始为下一次搜索改词，但还没回车：上一次的结论不该被改写。
    await user.type(screen.getByLabelText('搜索内容'), 'bar')
    expect(screen.getByRole('status').textContent).toBe('没有匹配「foo」。')
  })

  it('错误只走一行 role=alert，结果区不显示假数据', async () => {
    vi.mocked(dispatch).mockImplementationOnce(async () => {
      throw new Error('非法正则')
    })
    const user = userEvent.setup()
    render(<SearchPanel />)
    await submit(user, 'bad-regex')

    await waitFor(() =>
      expect(screen.getByRole('alert').textContent).toBe('搜索失败：非法正则'),
    )
    expect(screen.getByRole('status').textContent).toBe('输入关键词后按回车开始搜索。')
  })
})
