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
export type BottomPanelTab = 'logs' | 'problems'

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

  loadTree: () => Promise<void>
  openFile: (path: string) => Promise<void>
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