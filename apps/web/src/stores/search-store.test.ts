/**
 * searchStore 的契约测试：`file.search` 事件如何落进按文件分组的结果。
 *
 * 环境是 `node`（`apps/web/vitest.config.ts`），所以 `dispatch` 走 `ipc-mock`；
 * 这里只用 mock 的 `search_cancel`（不产出事件）来构造"取消过"的状态，
 * 其余断言全部用直接 `ingestEvent` 造事件，不依赖 mock 的命中内容。
 */

import { beforeEach, describe, expect, it } from 'vitest'
import type { Event as CoreEvent, SearchMatch } from '../generated'
import { INITIAL_SEARCH_STATE, useSearchStore } from './search-store'

function hit(path: string, line: number, text = '示例'): SearchMatch {
  // 契约里 `column` 恒为 1（核心的 SearchHit 没有列信息）。
  return { path, line, column: 1, text }
}

function fileSearch(searchId: string, matches: SearchMatch[]): CoreEvent {
  return { type: 'file.search', searchId, matches }
}

function ingest(event: CoreEvent): void {
  useSearchStore.getState().ingestEvent(event)
}

beforeEach(() => {
  // 默认处在"一次搜索正在进行、第一条事件还没到"的起点：事件只有在接收窗口内才有意义
  // （`running` 就是那个窗口，见 store 文件头）。
  useSearchStore.setState({ ...INITIAL_SEARCH_STATE, running: true })
})

describe('searchStore', () => {
  it('late_events_from_a_previous_search_are_dropped', async () => {
    // 上一条搜索已经采纳了 s1。
    ingest(fileSearch('s1', [hit('src/a.ts', 1)]))
    expect(useSearchStore.getState().searchId).toBe('s1')
    expect(useSearchStore.getState().totalMatches).toBe(1)

    // 取消 s1：ID 进 stale（取消命令在 mock 下不产事件，状态是可预期的）。
    await useSearchStore.getState().cancel()
    expect(useSearchStore.getState().running).toBe(false)

    // 模拟"新搜索已发出、第一条事件还没到"：running 且没有 searchId。
    // 这一段没法用 `start()` 铺，因为 mock 会立刻发自己的事件。
    useSearchStore.setState({
      running: true,
      searchId: null,
      resultsByFile: {},
      fileOrder: [],
      totalMatches: 0,
    })

    // s1 的结果现在才到 —— 必须整条丢弃。
    ingest(fileSearch('s1', [hit('src/a.ts', 2), hit('src/b.ts', 3)]))

    const state = useSearchStore.getState()
    expect(state.searchId).toBeNull()
    expect(state.totalMatches).toBe(0)
    expect(state.fileOrder).toEqual([])
    expect(state.resultsByFile['src/a.ts']).toBeUndefined()
  })

  it('multiple_events_for_one_search_accumulate', () => {
    ingest(fileSearch('s1', [hit('src/a.ts', 1), hit('src/b.ts', 4)]))
    ingest(fileSearch('s1', [hit('src/a.ts', 2), hit('src/a.ts', 9)]))

    const state = useSearchStore.getState()
    expect(state.searchId).toBe('s1')
    expect(state.totalMatches).toBe(4)
    expect(state.resultsByFile['src/a.ts']?.map((match) => match.line)).toEqual([1, 2, 9])
    expect(state.resultsByFile['src/b.ts']?.map((match) => match.line)).toEqual([4])
  })

  it('cancel_stops_accepting_results', async () => {
    ingest(fileSearch('s1', [hit('src/a.ts', 1)]))
    await useSearchStore.getState().cancel()

    expect(useSearchStore.getState().running).toBe(false)
    expect(useSearchStore.getState().searchId).toBeNull()

    // 取消之后，同一条搜索的事件（哪怕 ID 对得上）都不许再改结果。
    ingest(fileSearch('s1', [hit('src/a.ts', 2)]))
    expect(useSearchStore.getState().totalMatches).toBe(1)
    expect(useSearchStore.getState().resultsByFile['src/a.ts']?.map((match) => match.line)).toEqual([
      1,
    ])

    // 别的 ID 更不行。
    ingest(fileSearch('s2', [hit('src/z.ts', 7)]))
    expect(useSearchStore.getState().resultsByFile['src/z.ts']).toBeUndefined()
    expect(useSearchStore.getState().searchId).toBeNull()
  })

  it('results_are_grouped_by_path_preserving_backend_order', () => {
    // 后端顺序：文件按遍历顺序出现，同一文件内按行号递增 —— 前端必须原样保留。
    ingest(fileSearch('s1', [hit('src/lib/reply.ts', 5), hit('src/main.ts', 2)]))
    ingest(fileSearch('s1', [hit('src/lib/reply.ts', 12), hit('README.md', 1)]))

    const state = useSearchStore.getState()
    expect(state.fileOrder).toEqual(['src/lib/reply.ts', 'src/main.ts', 'README.md'])
    expect(state.resultsByFile['src/lib/reply.ts']?.map((match) => match.line)).toEqual([5, 12])
    expect(state.resultsByFile['src/main.ts']?.map((match) => match.line)).toEqual([2])
    expect(state.resultsByFile['README.md']?.map((match) => match.line)).toEqual([1])
    expect(state.totalMatches).toBe(4)
  })
})