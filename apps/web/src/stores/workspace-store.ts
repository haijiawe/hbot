/**
 * workspaceStore：文件树、打开的 tab、脏标记、布局。
 *
 * 铁律：store 之间不互相 import / set；跨域只走 `dispatch(command)` 或 `lib/events.ts`。
 * 也不直接读写磁盘：所有文件 IO 都经 `dispatch()` 到核心。
 */

import { create } from 'zustand'
import { errorMessage } from '../lib/format'
import type { CoreEvent, Diagnostic, FileNode } from '../lib/ipc'
import { DIAGNOSTIC_SEVERITY_ORDER, dispatch } from '../lib/ipc'
import {
  clampAgentWidth,
  clampBottomPanelHeight,
  clampSidebarWidth,
  type BottomPanelTab,
  type LayoutState,
  type SidebarView,
} from '../lib/layout'
import { readPrefs, writePrefs, type WorkspacePrefs } from '../lib/layout-prefs'
import type { SessionSnapshot } from '../lib/session-journal'

/**
 * 底部面板页签。搜索结果、问题、调试暂停现场共用这一块区域
 * （布局数值只来自 `lib/layout.ts`）。
 *
 * 类型本身住在 `lib/layout.ts`（与布局一起持久化），这里转出去是为了保住既有
 * `from '../stores/workspace-store'` 的 import 路径。
 */
export type { BottomPanelTab, SidebarView }

/** 光标位置（1-based）。切项目/重开时按 path 恢复。 */
export interface CursorPosition {
  line: number
  column: number
}

/**
 * "跳转到某文件的某一行"的意图。
 *
 * 为什么不直接从 `features/editor/editor-surface.tsx` 导出 `openAt`：那个文件静态
 * `import * as monaco`，从任何组件静态 import 它都会把 Monaco 拉进入口 chunk
 * （首屏体积 ≈ 266 kB → 1.5 MB）。所以跳行意图放 store，由编辑器自己消费 ——
 * `seq` 让"同一行连点两次"也能再次触发（state 引用变化才会重跑 effect）。
 */
export interface RevealRequest {
  path: string
  /** 1-based 行号（核心的 `SearchMatch.line` 就是 1-based）。 */
  line: number
  seq: number
}

export interface EditorTab {
  path: string
  dirty: boolean
  /** 最后一次读/写成功的磁盘版本，用于 M1 的外部改动检测。 */
  version: number
}

export interface WorkspaceState {
  rootName: string
  files: FileNode[]
  treeLoaded: boolean
  /** 打开缓冲区的当前文本，按 path 索引。 */
  contents: Record<string, string>
  /** 最后一次读/写成功的磁盘文本，用于计算 dirty。 */
  saved: Record<string, string>
  tabs: EditorTab[]
  activePath: string | null
  /** 每个打开文件的光标位置（编辑器上报，恢复会话时写回）。 */
  cursors: Record<string, CursorPosition>
  layout: LayoutState
  sidebarView: SidebarView
  bottomPanelTab: BottomPanelTab
  error: string | null
  /** 待执行的"跳行"意图；编辑器消费后不清空（`seq` 保证重复点击也生效）。 */
  reveal: RevealRequest | null
  /**
   * 当前诊断（问题面板的数据源）。
   *
   * 一个扁平列表而不是按文件分桶：面板本来就是"全部问题"一栏，分组只影响展示顺序，
   * 排序在 `sortDiagnostics` 里做一次（严重级 → 路径 → 行号）。空数组 = 没有诊断，
   * 与"还没查过"是同一件事（面板文案一样，不需要第三种状态）。
   */
  diagnostics: Diagnostic[]

  loadTree: () => Promise<void>
  openFile: (path: string) => Promise<void>
  /** 打开文件并把光标跳到 `line`（搜索结果点击用）。 */
  openAt: (path: string, line: number) => Promise<void>
  /** 拉一次诊断（问题面板挂载 / 切到该页签时调用）。 */
  refreshDiagnostics: () => Promise<void>
  /** 核心推来 `diagnostics` 事件时并入当前列表。 */
  ingestEvent: (event: CoreEvent) => void
  closeTab: (path: string) => void
  activateTab: (path: string) => void
  updateContent: (path: string, content: string) => void
  /** 编辑器光标移动后调用（节流由调用方负责，这里只存）。 */
  setCursor: (path: string, line: number, column: number) => void
  /**
   * 把 Agent 应用后的磁盘内容记为新的基线（`edit.apply` 成功后调用）。
   *
   * 冲突拒绝时磁盘**没有**变，所以只有真正应用成功的文件才调这个 —— 否则会把
   * 用户没保存的改动误判成"已存盘"，下次保存就丢了。
   */
  markDiskContent: (path: string, content: string) => void
  /**
   * 把一条/几条问题加进问题面板（Agent 应用被拒时用）。
   *
   * 走 `diagnostics` 而不是另开一个"应用错误"列表：用户已经知道去哪儿看问题。
   */
  reportProblems: (problems: readonly Diagnostic[]) => void
  /**
   * 恢复上次的会话现场（journal → 标签顺序/光标/活动标签/未保存缓冲区）。
   *
   * 每个标签都重新 `fs.read` 一次以拿到当前磁盘内容：journal 里只存脏标签的缓冲区，
   * 干净标签必须以磁盘为准（否则会把昨天的旧文本当成今天的文件内容）。
   */
  restoreSession: (snapshot: SessionSnapshot) => Promise<void>
  saveActive: () => Promise<void>
  saveAll: () => Promise<void>
  setSidebarView: (view: SidebarView) => void
  setBottomPanelTab: (tab: BottomPanelTab) => void
  setSidebarWidth: (width: number) => void
  setAgentWidth: (width: number) => void
  setBottomPanelHeight: (height: number) => void
  toggleSidebar: (collapsed?: boolean) => void
  toggleAgent: (collapsed?: boolean) => void
  toggleBottomPanel: (collapsed?: boolean) => void
  dismissError: () => void
}

function isDirty(content: string | undefined, saved: string | undefined): boolean {
  return content !== undefined && saved !== undefined && content !== saved
}

function severityRank(severity: Diagnostic['severity']): number {
  const index = DIAGNOSTIC_SEVERITY_ORDER.indexOf(severity)
  return index === -1 ? DIAGNOSTIC_SEVERITY_ORDER.length : index
}

/**
 * 收到过几条 `diagnostics` 事件。
 *
 * `refreshDiagnostics()` 是"发请求 → 等响应"，而核心在响应之前就会推一条同样的
 * `diagnostics` 事件（`crates/hbot-rpc` 的 `diagnostics` 命令必定推事件）。面板挂载时
 * 这两条路会撞车：事件先到（新），响应后到（旧快照），直接 `set` 会把刚到的诊断打回去。
 * 所以请求发出时记下这个计数器，响应回来发现它变了就让事件赢。
 */
let diagnosticsEventSeq = 0

/**
 * 稳定排序：严重级 → 路径 → 行号 → 列号。
 *
 * 面板按这个顺序渲染，用户不用自己找"哪条最要紧"；`Array.prototype.sort` 在 V8 上
 * 是稳定排序，所以同一严重级、同一位置的条目保持核心给的先后顺序。
 */
export function sortDiagnostics(items: readonly Diagnostic[]): Diagnostic[] {
  return [...items].sort(
    (left, right) =>
      severityRank(left.severity) - severityRank(right.severity) ||
      left.path.localeCompare(right.path) ||
      left.line - right.line ||
      left.column - right.column,
  )
}

/**
 * 并入一条 `diagnostics` 事件。
 *
 * 事件只带**一个 path 的条目**（`path === ''` 表示项目级/整包），所以按 path 覆盖：
 * 同一文件的新一批诊断要整体替换旧的，否则改了错的行号之后旧条目会留在面板上。
 */
export function mergeDiagnostics(
  current: readonly Diagnostic[],
  path: string,
  items: readonly Diagnostic[],
): Diagnostic[] {
  if (path === '') return sortDiagnostics(items)
  return sortDiagnostics([...current.filter((item) => item.path !== path), ...items])
}

/**
 * 布局 / 侧栏 / 底部页签一起持久化（`lib/layout-prefs.ts`）。
 *
 * 失败返回一句人话交给 `error`：布局存不下来不是灭顶之灾（用户在界面上仍能拖动），
 * 但绝不静默 —— 状态栏会把这句话显示出来。
 */
function savePrefsError(next: WorkspacePrefs): string | null {
  try {
    writePrefs(next)
    return null
  } catch (error) {
    return `布局没能保存：${errorMessage(error)}`
  }
}

/** 处理"改了布局的一部分，顺手落盘"的公共壳子。 */
function withPrefs(
  state: WorkspaceState,
  patch: Partial<Pick<WorkspaceState, 'layout' | 'sidebarView' | 'bottomPanelTab'>>,
): Partial<WorkspaceState> {
  const error = savePrefsError({
    layout: patch.layout ?? state.layout,
    sidebarView: patch.sidebarView ?? state.sidebarView,
    bottomPanelTab: patch.bottomPanelTab ?? state.bottomPanelTab,
  })
  return error === null ? patch : { ...patch, error }
}

/** 启动时先读回来的布局快照（同步读，首帧就是上次离开时的样子）。 */
const initialPrefs = readPrefs()

export const useWorkspaceStore = create<WorkspaceState>()((set, get) => ({
  rootName: 'hbot-project',
  files: [],
  treeLoaded: false,
  contents: {},
  saved: {},
  tabs: [],
  activePath: null,
  cursors: {},
  layout: initialPrefs.layout,
  sidebarView: initialPrefs.sidebarView,
  bottomPanelTab: initialPrefs.bottomPanelTab,
  error: null,
  reveal: null,
  diagnostics: [],

  async loadTree() {
    try {
      const files = await dispatch({ type: 'workspace.tree' })
      set({ files, treeLoaded: true, error: null })
    } catch (error) {
      set({ error: `读取文件树失败：${errorMessage(error)}`, treeLoaded: true })
    }
  },

  async openFile(path) {
    const existing = get().tabs.find((tab) => tab.path === path)
    if (existing) {
      set({ activePath: path })
      return
    }
    try {
      const result = await dispatch({ type: 'fs.read', path })
      set((state) => ({
        contents: { ...state.contents, [path]: result.content },
        saved: { ...state.saved, [path]: result.content },
        tabs: [...state.tabs, { path, dirty: false, version: result.version }],
        activePath: path,
        error: null,
      }))
    } catch (error) {
      set({ error: `打开 ${path} 失败：${errorMessage(error)}` })
    }
  },

  async openAt(path, line) {
    await get().openFile(path)
    // 打开失败时 `openFile` 只设 error、不动 activePath —— 别再往编辑器发跳行意图。
    if (get().activePath !== path) return
    set((state) => ({ reveal: { path, line, seq: (state.reveal?.seq ?? 0) + 1 } }))
  },

  async refreshDiagnostics() {
    // 请求期间如果来了 `diagnostics` 事件，事件比这条响应新（见 `diagnosticsEventSeq`）。
    const seqAtRequest = diagnosticsEventSeq
    try {
      const items = await dispatch({ type: 'diagnostics', path: null })
      if (diagnosticsEventSeq !== seqAtRequest) return
      set({ diagnostics: sortDiagnostics(items), error: null })
    } catch (error) {
      // 失败时保留上一批诊断：清空面板等于告诉用户"问题都没了"，那是撒谎。
      set({ error: `读取诊断失败：${errorMessage(error)}` })
    }
  },

  ingestEvent(event) {
    if (event.type !== 'diagnostics') return
    diagnosticsEventSeq += 1
    set((state) => ({ diagnostics: mergeDiagnostics(state.diagnostics, event.path, event.items) }))
  },

  closeTab(path) {
    set((state) => {
      const index = state.tabs.findIndex((tab) => tab.path === path)
      if (index === -1) return state
      const tabs = state.tabs.filter((tab) => tab.path !== path)
      const contents = { ...state.contents }
      delete contents[path]
      const saved = { ...state.saved }
      delete saved[path]
      const cursors = { ...state.cursors }
      delete cursors[path]
      let activePath = state.activePath
      if (activePath === path) {
        const neighbour = tabs[index] ?? tabs[index - 1] ?? null
        activePath = neighbour ? neighbour.path : null
      }
      return { ...state, tabs, contents, saved, cursors, activePath }
    })
  },

  activateTab(path) {
    set({ activePath: path })
  },

  updateContent(path, content) {
    set((state) => {
      const tabs = state.tabs.map((tab) =>
        tab.path === path ? { ...tab, dirty: isDirty(content, state.saved[path]) } : tab,
      )
      return { ...state, contents: { ...state.contents, [path]: content }, tabs }
    })
  },

  setCursor(path, line, column) {
    set((state) => {
      const current = state.cursors[path]
      // 同一个位置反复上报（编辑器每帧都可能报）不值得换一次 state 引用。
      if (current !== undefined && current.line === line && current.column === column) return state
      return { cursors: { ...state.cursors, [path]: { line, column } } }
    })
  },

  markDiskContent(path, content) {
    set((state) => {
      const exists = state.tabs.some((tab) => tab.path === path)
      if (!exists) return state
      const buffer = state.contents[path]
      return {
        saved: { ...state.saved, [path]: content },
        tabs: state.tabs.map((tab) =>
          tab.path === path ? { ...tab, dirty: isDirty(buffer, content) } : tab,
        ),
      }
    })
  },

  reportProblems(problems) {
    if (problems.length === 0) return
    set((state) => {
      // 同一条（path + message）已经在面板上就别叠第二条：用户连点两次"应用"只该看到一条。
      const known = new Set(state.diagnostics.map((item) => `${item.path}\u0000${item.message}`))
      const fresh = problems.filter((item) => !known.has(`${item.path}\u0000${item.message}`))
      if (fresh.length === 0) return state
      return { diagnostics: sortDiagnostics([...state.diagnostics, ...fresh]) }
    })
  },

  async restoreSession(snapshot) {
    const restored: EditorTab[] = []
    const contents: Record<string, string> = {}
    const saved: Record<string, string> = {}
    const cursors: Record<string, CursorPosition> = {}
    const failures: string[] = []
    // 顺序串行读：标签顺序必须与 journal 一致（并行读会让完成顺序决定 tab 顺序）。
    for (const entry of snapshot.tabs) {
      try {
        const result = await dispatch({ type: 'fs.read', path: entry.path })
        // 只有脏标签带缓冲区；干净标签一律以磁盘为准（journal 里存的是昨天的事）。
        const buffer = entry.content ?? result.content
        contents[entry.path] = buffer
        saved[entry.path] = result.content
        restored.push({ path: entry.path, dirty: buffer !== result.content, version: result.version })
        cursors[entry.path] = entry.cursor
      } catch (error) {
        // 文件可能已被删除/改名：这一个标签恢复不了，其余照常恢复，并把原因说出来。
        failures.push(`恢复 ${entry.path} 失败：${errorMessage(error)}`)
      }
    }
    const wanted = snapshot.activePath
    const activePath =
      wanted !== null && restored.some((tab) => tab.path === wanted)
        ? wanted
        : (restored[0]?.path ?? null)
    set({
      tabs: restored,
      contents,
      saved,
      cursors,
      activePath,
      error: failures.length > 0 ? failures.join('；') : null,
    })
  },

  async saveActive() {
    const { activePath, tabs, contents } = get()
    if (!activePath) return
    const tab = tabs.find((item) => item.path === activePath)
    const content = contents[activePath]
    if (!tab || content === undefined) return
    try {
      const result = await dispatch({ type: 'fs.write', path: activePath, content })
      set((state) => ({
        saved: { ...state.saved, [activePath]: content },
        tabs: state.tabs.map((item) =>
          item.path === activePath ? { ...item, dirty: false, version: result.version } : item,
        ),
        error: null,
      }))
    } catch (error) {
      set({ error: `保存 ${activePath} 失败：${errorMessage(error)}` })
    }
  },

  async saveAll() {
    const { tabs } = get()
    const dirtyPaths = tabs.filter((tab) => tab.dirty).map((tab) => tab.path)
    for (const path of dirtyPaths) {
      const content = get().contents[path]
      if (content === undefined) continue
      try {
        const result = await dispatch({ type: 'fs.write', path, content })
        set((state) => ({
          saved: { ...state.saved, [path]: content },
          tabs: state.tabs.map((tab) =>
            tab.path === path ? { ...tab, dirty: false, version: result.version } : tab,
          ),
          error: null,
        }))
      } catch (error) {
        // 逐个上报，不中断其他文件的保存（docs/06 §4.2：全有或全无由核心事务保证）。
        set({ error: `保存 ${path} 失败：${errorMessage(error)}` })
      }
    }
  },

  setSidebarView(view) {
    set((state) =>
      withPrefs(state, {
        sidebarView: view,
        layout: { ...state.layout, sidebarCollapsed: false },
      }),
    )
  },

  setBottomPanelTab(tab) {
    set((state) =>
      withPrefs(state, {
        bottomPanelTab: tab,
        layout: { ...state.layout, bottomPanelCollapsed: false },
      }),
    )
  },

  setSidebarWidth(width) {
    set((state) =>
      withPrefs(state, { layout: { ...state.layout, sidebarWidth: clampSidebarWidth(width) } }),
    )
  },

  setAgentWidth(width) {
    set((state) =>
      withPrefs(state, { layout: { ...state.layout, agentWidth: clampAgentWidth(width) } }),
    )
  },

  setBottomPanelHeight(height) {
    set((state) =>
      withPrefs(state, {
        layout: { ...state.layout, bottomPanelHeight: clampBottomPanelHeight(height) },
      }),
    )
  },

  toggleSidebar(collapsed) {
    set((state) =>
      withPrefs(state, {
        layout: {
          ...state.layout,
          sidebarCollapsed: collapsed ?? !state.layout.sidebarCollapsed,
        },
      }),
    )
  },

  toggleAgent(collapsed) {
    set((state) =>
      withPrefs(state, {
        layout: { ...state.layout, agentCollapsed: collapsed ?? !state.layout.agentCollapsed },
      }),
    )
  },

  toggleBottomPanel(collapsed) {
    set((state) =>
      withPrefs(state, {
        layout: {
          ...state.layout,
          bottomPanelCollapsed: collapsed ?? !state.layout.bottomPanelCollapsed,
        },
      }),
    )
  },

  dismissError() {
    set({ error: null })
  },
}))