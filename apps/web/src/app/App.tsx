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
  type SidebarView,
} from '../lib/layout'
import { AgentPanel } from '../features/agent/agent-panel'
import { applyTheme } from '../lib/theme'
import { EditorArea } from '../features/editor/editor-area'
import { ExplorerView } from '../features/explorer/explorer-view'
import { openSearch } from '../features/search/open-search'
// 行内编辑的桥：编辑器挂载时注册，快捷键这边只问一句"现在能不能开浮层"。
import { requestInlineEdit } from '../lib/inline-edit'
import { SettingsView } from '../features/settings/settings-view'
import { BotsView } from '../features/workbench/bots-view'
import { CapabilitiesView } from '../features/workbench/capabilities-view'
import { ChannelsView } from '../features/workbench/channels-view'
import { ConversationsView } from '../features/workbench/conversations-view'
import { DocsSide, DocsView } from '../features/workbench/docs-view'
import { JobsBoardSide, JobsBoardView } from '../features/workbench/jobs-board-view'
import { OverviewView } from '../features/workbench/overview-view'
import { RuntimeView } from '../features/workbench/runtime-view'
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

// 侧栏的面只有一个真源：活动栏改的是 sidebarView，这里决定渲染谁。
// 新增一个面时，layout.ts 的 SidebarView、活动栏的条目、这里的三处要同批改。
function renderSidebar(view: SidebarView) {
  switch (view) {
    case 'overview':
      return <OverviewView />
    case 'bots':
      return <BotsView />
    case 'capabilities':
      return <CapabilitiesView />
    case 'conversations':
      return <ConversationsView />
    case 'runtime':
      return <RuntimeView />
    case 'channels':
      return <ChannelsView />
    case 'board':
      return <JobsBoardSide />
    case 'docs':
      return <DocsSide />
    case 'settings':
      return <SettingsView />
    case 'explorer':
      return <ExplorerView />
  }
}

/**
 * 两个"要整页宽度"的面（看板 / 文档）。返回 null 表示主区域还是编辑器。
 *
 * 为什么不分两套导航：侧栏放清单、主区域放正文是同一个面的两半（与 VSCode 的
 * "视图 + 编辑区"一致）。面一旦确定，主区域就不该再猜——所以这里只认 `SidebarView`。
 */
function renderPage(view: SidebarView) {
  switch (view) {
    case 'board':
      return <JobsBoardView />
    case 'docs':
      return <DocsView />
    default:
      return null
  }
}
export function App() {
  const layout = useWorkspaceStore((state) => state.layout)
  const sidebarView = useWorkspaceStore((state) => state.sidebarView)
  const theme = useWorkspaceStore((state) => state.theme)
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
  const page = renderPage(sidebarView)
  const styleVars = layoutStyleVars(resolved)

  const notify = useCallback((message: string) => {
    setNotice(message)
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current)
    noticeTimer.current = window.setTimeout(() => setNotice(null), NOTICE_DURATION_MS)
  }, [])

  // 主题写到 <html data-theme>：两套调色板都在 styles/theme.css，组件只管用变量。
  useEffect(() => {
    applyTheme(theme)
  }, [theme])

  useEffect(() => {
    return () => {
      if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current)
    }
  }, [])

  useEffect(() => {
    // 没有打开项目时不发 `workspace.tree`：核心回 `-32002`，而首屏状态栏会把这条
    // 显示成红字（用户什么都没做就报错）。项目打开 / 切换时再读。
    if (projectRoot === null) return
    void loadTree()
  }, [projectRoot, loadTree])

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
                {renderSidebar(sidebarView)}
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
          {page !== null ? (
            <ErrorBoundary name="工作台页面">{page}</ErrorBoundary>
          ) : projectRoot === null ? (
            // 还没有项目：显示一句能照着做的提示，而不是一块空白画布。
            // 真桌面壳启动时 `projectRoot` 是 null（要等用户打开/新建项目）；
            // mock 模式由 `startSessionPersistence` 先定成 `mock://project`，所以这条
            // 空状态在浏览器 dev 里不会挡住演示。
            <div className="wb-page wb-page--center" role="status" aria-label="还没有打开项目">
              <div className="wb-hero">
                <span className="wb-eyebrow">BOT WORKBENCH</span>
                <h2 className="wb-title">还没有打开项目</h2>
                <p className="wb-sub">
                  按 Ctrl / Cmd + Shift + P 打开命令面板，选「打开项目…」，粘贴项目根的绝对路径。
                </p>
                <div className="wb-card">
                  <div className="wb-card__title">从这里开始</div>
                  <ul className="wb-keys">
                    <li>
                      <span className="wb-kbd">Ctrl ⇧ P</span>打开命令面板
                    </li>
                    <li>
                      <span className="wb-kbd">打开项目…</span>粘贴项目根的绝对路径
                    </li>
                    <li>
                      <span className="wb-kbd">新建项目…</span>交给核心 scaffold（含 hbot.json / bots.json）
                    </li>
                  </ul>
                </div>
                <p className="wb-note">打开项目后，侧栏会显示这个 bot 的清单、能力、会话与运行态；作业看板与设计文档两面在编辑区整页打开，日志在底部面板（Ctrl+J）。</p>
              </div>
            </div>
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