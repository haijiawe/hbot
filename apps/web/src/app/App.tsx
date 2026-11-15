/**
 * IDE 外壳：活动栏 + 侧栏 + 编辑区 + Agent 栏 + 底部面板 + 状态栏。
 *
 * 所有尺寸来自 `lib/layout.ts`（含 clamp），组件不写死 px；三块可折叠区由
 * workspaceStore.layout 驱动，分隔条拖拽/键盘都走 store 的 setter。
 */

import { lazy, Suspense, useCallback, useEffect, useRef, useState } from 'react'
import {
  AGENT_MAX_WIDTH,
  AGENT_MIN_WIDTH,
  BOTTOM_PANEL_MAX_HEIGHT,
  BOTTOM_PANEL_MIN_HEIGHT,
  SIDEBAR_MAX_WIDTH,
  SIDEBAR_MIN_WIDTH,
  layoutStyleVars,
  resolveLayout,
} from '../lib/layout'
import { AgentPanel } from '../features/agent/agent-panel'
import { EditorArea } from '../features/editor/editor-area'
import { ExplorerView } from '../features/explorer/explorer-view'
import { SettingsView } from '../features/settings/settings-view'
import { useRunStore } from '../stores/run-store'
import { useWorkspaceStore } from '../stores/workspace-store'
import { ActivityBar } from './activity-bar'
import { CommandPalette, type PaletteMode } from '../features/command-palette/command-palette'
import { ErrorBoundary } from './error-boundary'
import { Splitter } from '../components/splitter'
import { StatusBar } from './status-bar'
import { useGlobalKeybindings } from './use-global-keybindings'

// 底部面板包含日志渲染与问题面板，首屏不加载（AGENTS.md §6）。
const BottomPanel = lazy(() => import('../features/run/bottom-panel'))

const NOTICE_DURATION_MS = 4000

export function App() {
  const layout = useWorkspaceStore((state) => state.layout)
  const sidebarView = useWorkspaceStore((state) => state.sidebarView)
  const loadTree = useWorkspaceStore((state) => state.loadTree)
  const setSidebarWidth = useWorkspaceStore((state) => state.setSidebarWidth)
  const setAgentWidth = useWorkspaceStore((state) => state.setAgentWidth)
  const setBottomPanelHeight = useWorkspaceStore((state) => state.setBottomPanelHeight)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)
  const toggleBottomPanel = useWorkspaceStore((state) => state.toggleBottomPanel)
  const setBottomPanelTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const saveActive = useWorkspaceStore((state) => state.saveActive)
  const saveAll = useWorkspaceStore((state) => state.saveAll)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)

  const [paletteMode, setPaletteMode] = useState<PaletteMode | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const noticeTimer = useRef<number | null>(null)

  const resolved = resolveLayout(layout)
  const styleVars = layoutStyleVars(resolved)

  const notify = useCallback((message: string) => {
    setNotice(message)
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current)
    noticeTimer.current = window.setTimeout(() => setNotice(null), NOTICE_DURATION_MS)
  }, [])

  useEffect(() => {
    return () => {
      if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current)
    }
  }, [])

  useEffect(() => {
    void loadTree()
  }, [loadTree])

  useGlobalKeybindings({
    openPalette: () => setPaletteMode('commands'),
    openQuickOpen: () => setPaletteMode('files'),
    save: () => void saveActive(),
    saveAll: () => void saveAll(),
    run: () => void start(),
    stop: () => void stop(),
    toggleAgent: () => toggleAgent(),
    toggleBottomPanel: () => toggleBottomPanel(),
    showProblems: () => setBottomPanelTab('problems'),
    notify,
  })

  return (
    <div className="ide" style={styleVars}>
      <ActivityBar />

      <div className="ide__body">
        {resolved.sidebarVisible ? (
          <>
            <aside className="sidebar" aria-label="侧栏">
              <ErrorBoundary name="侧栏">
                {sidebarView === 'explorer' ? <ExplorerView /> : <SettingsView />}
              </ErrorBoundary>
            </aside>
            <Splitter
              orientation="vertical"
              value={resolved.sidebarWidth}
              min={SIDEBAR_MIN_WIDTH}
              max={SIDEBAR_MAX_WIDTH}
              label="调整侧栏宽度"
              onChange={setSidebarWidth}
            />
          </>
        ) : null}

        <div className="workspace-column">
          <ErrorBoundary name="编辑器">
            <EditorArea />
          </ErrorBoundary>

          {resolved.bottomPanelVisible ? (
            <>
              <Splitter
                orientation="horizontal"
                value={resolved.bottomPanelHeight}
                min={BOTTOM_PANEL_MIN_HEIGHT}
                max={BOTTOM_PANEL_MAX_HEIGHT}
                direction={-1}
                label="调整底部面板高度"
                onChange={setBottomPanelHeight}
              />
              <ErrorBoundary name="底部面板">
                <Suspense
                  fallback={
                    <p className="panel-empty" role="status">
                      正在加载底部面板…
                    </p>
                  }
                >
                  <BottomPanel />
                </Suspense>
              </ErrorBoundary>
            </>
          ) : null}
        </div>

        {resolved.agentVisible ? (
          <>
            <Splitter
              orientation="vertical"
              value={resolved.agentWidth}
              min={AGENT_MIN_WIDTH}
              max={AGENT_MAX_WIDTH}
              direction={-1}
              label="调整 Agent 栏宽度"
              onChange={setAgentWidth}
            />
            <aside className="agent-bar" aria-label="Agent 栏">
              <ErrorBoundary name="Agent">
                <AgentPanel />
              </ErrorBoundary>
            </aside>
          </>
        ) : null}
      </div>

      <StatusBar notice={notice} />

      {paletteMode ? (
        <CommandPalette
          mode={paletteMode}
          onClose={() => setPaletteMode(null)}
          onNotice={notify}
        />
      ) : null}
    </div>
  )
}