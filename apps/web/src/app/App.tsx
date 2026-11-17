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
import { openSearch } from '../features/search/open-search'
// 行内编辑的桥：编辑器挂载时注册，快捷键这边只问一句"现在能不能开浮层"。
import { requestInlineEdit } from '../lib/inline-edit'
import { SettingsView } from '../features/settings/settings-view'
import { restoreSessionIfSameProject, startSessionPersistence } from '../lib/session-persistence'
import { useRunStore } from '../stores/run-store'
import { useDebugStore } from '../stores/debug-store'
import { useDeviceStore } from '../stores/device-store'
import { useWorkspaceStore } from '../stores/workspace-store'
import { ActivityBar } from './activity-bar'
import { CommandPalette, type PaletteMode } from '../features/command-palette/command-palette'
import { ProjectDialog } from '../features/command-palette/project-dialog'
import { ErrorBoundary } from './error-boundary'
import { Splitter } from '../components/splitter'
import { StatusBar } from './status-bar'
import { useCoreEvents } from './use-core-events'
import { useGlobalKeybindings } from './use-global-keybindings'

// 底部面板包含日志渲染与问题面板，首屏不加载（AGENTS.md §6）。
const BottomPanel = lazy(() => import('../features/run/bottom-panel'))

const NOTICE_DURATION_MS = 4000

export function App() {
  const layout = useWorkspaceStore((state) => state.layout)
  const sidebarView = useWorkspaceStore((state) => state.sidebarView)
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const projectDialog = useWorkspaceStore((state) => state.projectDialog)
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
  const runBotId = useRunStore((state) => state.botId)
  // 调试的单位是 bot 而不是会话，所以 botId 由外壳从 runStore 注入 debugStore
  // （两个 store 之间不互相 import，AGENTS.md §0.11）。
  const setDebugBotId = useDebugStore((state) => state.setBotId)
  // 设备会话同样按 bot 记（docs/09-device-capability.md §3）：botId 由外壳注入，
  // deviceStore 自己不 import runStore（AGENTS.md §0.11）。
  const setDeviceBotId = useDeviceStore((state) => state.setBotId)
  const refreshDevice = useDeviceStore((state) => state.refresh)
  const debugResume = useDebugStore((state) => state.resume)
  const debugStep = useDebugStore((state) => state.step)

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

  // 未保存缓冲区 journal：挂载时接上录制（编辑 / 切标签 / 脏标记变化 → 节流写 IndexedDB）
  // 与 beforeunload 冲刷，随后尝试恢复上次现场。写盘失败进状态栏（不静默吞）。
  // 卸载时解绑并再尽力 flush 一次；StrictMode 的重复挂载由 journal 单例兜住。
  useEffect(() => {
    const stop = startSessionPersistence()
    void restoreSessionIfSameProject()
    return stop
  }, [])

  // 启停 bot 会换掉调试目标；断点与暂停现场都由 debugStore 自己处理这次切换。
  useEffect(() => {
    setDebugBotId(runBotId)
  }, [runBotId, setDebugBotId])

  // 设备会话也是按 bot 的：换 bot 就把设备页签的现场换成新 bot 的（`setBotId` 内部会清），
  // 有 bot 时顺手读一次状态 —— 否则用户打开设备页签看到的是"还没有读到设备状态"。
  // 之后一律以 `device.status` 事件为准（IDE 不拥有运行时真相）。
  useEffect(() => {
    setDeviceBotId(runBotId)
    if (runBotId.length > 0) void refreshDevice(runBotId)
  }, [runBotId, setDeviceBotId, refreshDevice])

  // 核心事件（bot.status / run.log / agent.*）与 bot 列表播种都在这个 hook 里。
  useCoreEvents()

  useGlobalKeybindings({
    openPalette: () => setPaletteMode('commands'),
    openQuickOpen: () => setPaletteMode('files'),
    save: () => void saveActive(),
    saveAll: () => void saveAll(),
    // 没有打开的编辑器（主编辑区懒加载未完成 / 没开文件）时快捷键按下去必须有回音，
    // 否则用户以为快捷键坏了。这就是取消/失败的那条可见反馈。
    inlineEdit: () => {
      if (!requestInlineEdit()) notify('没有可编辑的选区：先打开一个文件，把光标放到要改的那一行。')
    },
    run: () => void start(),
    stop: () => void stop(),
    toggleAgent: () => toggleAgent(),
    toggleBottomPanel: () => toggleBottomPanel(),
    showProblems: () => setBottomPanelTab('problems'),
    showSearch: () => openSearch(),
    debugContinue: () => void debugResume(),
    debugStepOver: () => void debugStep('over'),
    debugStepInto: () => void debugStep('into'),
    debugStepOut: () => void debugStep('out'),
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
          {projectRoot === null ? (
            // 还没有项目：显示一句能照着做的提示，而不是一块空白画布。
            // 真桌面壳启动时 `projectRoot` 是 null（要等用户打开/新建项目）；
            // mock 模式由 `startSessionPersistence` 先定成 `mock://project`，所以这条
            // 空状态在浏览器 dev 里不会挡住演示。
            <section className="welcome" role="status" aria-label="还没有打开项目">
              <h2 className="welcome__title">还没有打开项目</h2>
              <p className="welcome__hint">
                按 <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>Shift</kbd> + <kbd>P</kbd> 打开命令面板，
                选「打开项目…」，粘贴项目根的绝对路径。
              </p>
              <p className="welcome__note">
                想从空目录开始？同一面板里的「新建项目…」会交给核心 scaffold。
              </p>
            </section>
          ) : (
            <ErrorBoundary name="编辑器">
              <EditorArea />
            </ErrorBoundary>
          )}

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

      {projectDialog !== null ? <ProjectDialog onNotice={notify} /> : null}
    </div>
  )
}