/**
 * searchStore：全局搜索（`search_start` 命令 + `file.search` 事件）。
 *
 * 契约在 `crates/hbot-rpc/src/dto/`（生成物 `src/generated/`），三条硬事实决定了这里的形状：
 *
 * 1. `search_start { query, regex, globs }` 的返回是 `data: null`：命中**不**走返回值，
 *    而是 `file.search { searchId, matches }` 事件。所以开始搜索时本 store 里**没有 ID**，
 *    它是随第一条事件到达的。
 * 2. 事件可能多条（契约允许增量），同一 `searchId` 的命中必须**追加**而不是覆盖。
 * 3. 事件里没有 `query` / `hits` / `truncated` / 列号（`column` 恒为 `1`），所以分组与
 *    计数只能自己从 `matches` 累加；"结果被截断"这类信息 UI 拿不到，不能假装有。
 *
 * 最容易出 bug 的地方（也是本文件的重点）：**旧搜索的迟到事件**。开始新搜索后
 * `searchId` 还是 `null`，单看"是不是当前 ID"分不出这条事件属于新搜索还是上一条，
 * 于是被取代 / 被取消的 ID 记进 `staleSearchIds`（有界），它们的迟到事件一律丢弃。
 *
 * 取消语义（best-effort，与核心一致）：`search_cancel` 需要 `searchId`，而 ID 只随
 * 事件到达；因此在第一条事件之前点"取消"**无法**通知核心，只能本地停止接收。
 * 取消后核心不再推事件、也不会补一条"假结果"。
 *
 * 最后一条要注意：契约里**没有**"搜索完成"事件。所以 `running` 的含义是
 * "还在接收这次搜索的事件"，它会一直为 true 直到 `cancel()` 或被新 `start()` 取代 ——
 * 面板据此把状态分成"等第一条事件"与"结果持续追加"两种文案，不编造"已完成"。
 */

import { create } from 'zustand'
import type { Event as CoreEvent, SearchMatch } from '../generated'
import { errorMessage } from '../lib/format'
import { dispatch } from '../lib/ipc'

/** 记住多少个被取代的搜索 ID；超出后丢最旧的。 */
const MAX_STALE_SEARCH_IDS = 16

/** `search_start` 的入参（与 wire 契约同名同义）。 */
export interface SearchParams {
  query: string
  regex: boolean
  /** gitignore 语义的 glob（由 `parseGlobs` 从输入框拆出来）。 */
  globs: string[]
}

export interface SearchState {
  /** 输入框里的查询串（用户可能正在改，**不代表**当前结果属于它）。 */
  query: string
  /**
   * 最近一次**真的提交给核心**的查询串。
   *
   * 与 `query` 分开的理由：`query` 随每次按键变，而"当前这批结果"只属于上一次提交。
   * 空结果时面板要说「没有匹配「X」」，X 必须是**提交过**的那个串 —— 拿 `query`
   * 会在用户还没按回车时就把上一次搜索的结论改写成新词（界面撒谎）。
   */
  submittedQuery: string
  regex: boolean
  /** glob 的原始输入文本（逗号 / 空白分隔），提交时经 `parseGlobs` 拆成数组。 */
  globs: string
  /** 是否还在接收本次搜索的事件（没有"完成"事件，见文件头）。 */
  running: boolean
  /** 当前搜索的 ID；第一条事件到达前是 `null`。 */
  searchId: string | null
  /** 命中按文件分组；顺序由 `fileOrder` 决定，不依赖对象键序。 */
  resultsByFile: Record<string, SearchMatch[]>
  /** 文件首次出现的顺序（= 后端事件里的出现顺序，必须原样保留）。 */
  fileOrder: string[]
  totalMatches: number
  error: string | null
  /** 已被取代 / 取消的搜索 ID：它们的事件一律丢弃。 */
  staleSearchIds: string[]
  /** 自增计数：置位后搜索面板把焦点放回输入框（跨组件不新增 store 依赖）。 */
  focusSeq: number

  setQuery: (query: string) => void
  setRegex: (regex: boolean) => void
  setGlobs: (globs: string) => void
  start: (params: SearchParams) => Promise<void>
  cancel: () => Promise<void>
  ingestEvent: (event: CoreEvent) => void
  /** 只清本地结果并停止接收；已开始的搜索不会因此被核心取消。 */
  clear: () => void
  focusInput: () => void
}

/** `"*.ts, !vendor/**"` → `["*.ts", "!vendor/**"]`（逗号或空白分隔）。 */
export function parseGlobs(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((glob) => glob.trim())
    .filter((glob) => glob.length > 0)
}

/** `parseGlobs` 的反向：回填输入框时用同一种分隔符。 */
export function formatGlobs(globs: readonly string[]): string {
  return globs.join(', ')
}

/** 记住一个被取代的 ID（去重 + 有界）。 */
function rememberId(ids: readonly string[], id: string | null): string[] {
  if (id === null) return [...ids]
  const next = ids.filter((candidate) => candidate !== id)
  next.push(id)
  return next.length > MAX_STALE_SEARCH_IDS ? next.slice(next.length - MAX_STALE_SEARCH_IDS) : next
}

/** 初始状态（测试与"新开一次搜索"共用，避免两份字面量）。 */
export const INITIAL_SEARCH_STATE = {
  query: '',
  submittedQuery: '',
  regex: false,
  globs: '',
  running: false,
  searchId: null,
  resultsByFile: {},
  fileOrder: [],
  totalMatches: 0,
  error: null,
  staleSearchIds: [],
  focusSeq: 0,
} satisfies Pick<
  SearchState,
  | 'query'
  | 'submittedQuery'
  | 'regex'
  | 'globs'
  | 'running'
  | 'searchId'
  | 'resultsByFile'
  | 'fileOrder'
  | 'totalMatches'
  | 'error'
  | 'staleSearchIds'
  | 'focusSeq'
>

export const useSearchStore = create<SearchState>()((set, get) => ({
  ...INITIAL_SEARCH_STATE,

  setQuery(query) {
    set({ query })
  },

  setRegex(regex) {
    set({ regex })
  },

  setGlobs(globs) {
    set({ globs })
  },

  async start(params) {
    const { query } = params
    if (query.length === 0) {
      // 与核心同样的判据（`hbot-workspace`：`search query is empty`），省一次往返。
      set({ running: false, searchId: null, error: '请输入搜索内容。' })
      return
    }

    const previous = get()
    set({
      query,
      submittedQuery: query,
      regex: params.regex,
      globs: formatGlobs(params.globs),
      running: true,
      searchId: null,
      resultsByFile: {},
      fileOrder: [],
      totalMatches: 0,
      error: null,
      staleSearchIds: rememberId(previous.staleSearchIds, previous.searchId),
    })

    try {
      // 返回是 `data: null`；命中随后由 `file.search` 事件推来（见文件头）。
      await dispatch({ type: 'search.start', query, regex: params.regex, globs: params.globs })
    } catch (error) {
      // 命令自己失败（非法正则 / 非法 glob / 核心拒绝）：不会再有事件，必须收掉 running。
      set({ error: `搜索失败：${errorMessage(error)}`, running: false, searchId: null })
    }
  },

  async cancel() {
    const { running, searchId, staleSearchIds } = get()
    if (!running) return

    // 先本地停止接收：核心侧取消是 best-effort，UI 不能等它。
    set({
      running: false,
      searchId: null,
      staleSearchIds: rememberId(staleSearchIds, searchId),
    })

    // 还没有 ID（第一条事件没到）：`search_cancel` 没有可用的 ID，只能本地停收。
    if (searchId === null) return

    try {
      await dispatch({ type: 'search.cancel', searchId })
    } catch (error) {
      set({ error: `取消搜索失败：${errorMessage(error)}` })
    }
  },

  ingestEvent(event) {
    if (event.type !== 'file.search') return

    const state = get()
    if (!state.running) return

    if (state.searchId !== null) {
      // 只认当前搜索；其余（包括旧搜索的迟到事件）丢弃。
      if (event.searchId !== state.searchId) return
    } else if (state.staleSearchIds.includes(event.searchId)) {
      // 新搜索的首条事件还没到，但这条来自已被取代的搜索：丢弃。
      return
    }

    if (event.matches.length === 0) {
      // 空事件没有命中，但它确认了本次搜索的 ID（取消时要用）。
      set({ searchId: event.searchId })
      return
    }

    const bucket = new Map<string, SearchMatch[]>()
    for (const match of event.matches) {
      const list = bucket.get(match.path)
      if (list === undefined) bucket.set(match.path, [match])
      else list.push(match)
    }

    const resultsByFile: Record<string, SearchMatch[]> = { ...state.resultsByFile }
    const fileOrder = [...state.fileOrder]
    for (const [path, matches] of bucket) {
      const existing = resultsByFile[path]
      if (existing === undefined) {
        fileOrder.push(path)
        resultsByFile[path] = matches
      } else {
        resultsByFile[path] = [...existing, ...matches]
      }
    }

    set({
      searchId: event.searchId,
      resultsByFile,
      fileOrder,
      totalMatches: state.totalMatches + event.matches.length,
    })
  },

  clear() {
    set((state) => ({
      running: false,
      searchId: null,
      resultsByFile: {},
      fileOrder: [],
      totalMatches: 0,
      error: null,
      staleSearchIds: rememberId(state.staleSearchIds, state.searchId),
    }))
  },

  focusInput() {
    set((state) => ({ focusSeq: state.focusSeq + 1 }))
  },
}))