/**
 * workspaceStore：文件树、打开的 tab、脏标记、布局。
 *
 * 铁律：store 之间不互相 import / set；跨域只走 `dispatch(command)` 或 `lib/events.ts`。
 * 也不直接读写磁盘：所有文件 IO 都经 `dispatch()` 到核心。
 */

import { create } from 'zustand'
import { errorMessage } from '../lib/format'
import type { FileNode } from '../lib/ipc'
import { dispatch } from '../lib/ipc'
import {
  DEFAULT_LAYOUT,
  clampAgentWidth,
  clampBottomPanelHeight,
  clampSidebarWidth,
  type LayoutState,
} from '../lib/layout'

export type SidebarView = 'explorer' | 'settings'
/**
 * 底部面板页签。搜索结果、问题、调试暂停现场共用这一块区域
 * （布局数值只来自 `lib/layout.ts`）。
 */
export type BottomPanelTab = 'logs' | 'problems' | 'search' | 'debug'

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
  layout: LayoutState
  sidebarView: SidebarView
  bottomPanelTab: BottomPanelTab
  error: string | null
  /** 待执行的"跳行"意图；编辑器消费后不清空（`seq` 保证重复点击也生效）。 */
  reveal: RevealRequest | null

  loadTree: () => Promise<void>
  openFile: (path: string) => Promise<void>
  /** 打开文件并把光标跳到 `line`（搜索结果点击用）。 */
  openAt: (path: string, line: number) => Promise<void>
  closeTab: (path: string) => void
  activateTab: (path: string) => void
  updateContent: (path: string, content: string) => void
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

export const useWorkspaceStore = create<WorkspaceState>()((set, get) => ({
  rootName: 'hbot-project',
  files: [],
  treeLoaded: false,
  contents: {},
  saved: {},
  tabs: [],
  activePath: null,
  layout: DEFAULT_LAYOUT,
  sidebarView: 'explorer',
  bottomPanelTab: 'logs',
  error: null,
  reveal: null,

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

  closeTab(path) {
    set((state) => {
      const index = state.tabs.findIndex((tab) => tab.path === path)
      if (index === -1) return state
      const tabs = state.tabs.filter((tab) => tab.path !== path)
      const contents = { ...state.contents }
      delete contents[path]
      const saved = { ...state.saved }
      delete saved[path]
      let activePath = state.activePath
      if (activePath === path) {
        const neighbour = tabs[index] ?? tabs[index - 1] ?? null
        activePath = neighbour ? neighbour.path : null
      }
      return { ...state, tabs, contents, saved, activePath }
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
    set((state) => ({
      sidebarView: view,
      layout: { ...state.layout, sidebarCollapsed: false },
    }))
  },

  setBottomPanelTab(tab) {
    set((state) => ({
      bottomPanelTab: tab,
      layout: { ...state.layout, bottomPanelCollapsed: false },
    }))
  },

  setSidebarWidth(width) {
    set((state) => ({ layout: { ...state.layout, sidebarWidth: clampSidebarWidth(width) } }))
  },

  setAgentWidth(width) {
    set((state) => ({ layout: { ...state.layout, agentWidth: clampAgentWidth(width) } }))
  },

  setBottomPanelHeight(height) {
    set((state) => ({
      layout: { ...state.layout, bottomPanelHeight: clampBottomPanelHeight(height) },
    }))
  },

  toggleSidebar(collapsed) {
    set((state) => ({
      layout: {
        ...state.layout,
        sidebarCollapsed: collapsed ?? !state.layout.sidebarCollapsed,
      },
    }))
  },

  toggleAgent(collapsed) {
    set((state) => ({
      layout: { ...state.layout, agentCollapsed: collapsed ?? !state.layout.agentCollapsed },
    }))
  },

  toggleBottomPanel(collapsed) {
    set((state) => ({
      layout: {
        ...state.layout,
        bottomPanelCollapsed: collapsed ?? !state.layout.bottomPanelCollapsed,
      },
    }))
  },

  dismissError() {
    set({ error: null })
  },
}))