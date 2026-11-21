/** 活动栏：切换侧栏视图、打开底部面板页签、开关 Agent 栏。 */

import { openSearch } from '../features/search/open-search'
import { useWorkspaceStore } from '../stores/workspace-store'

interface ActivityItem {
  id: string
  label: string
  icon: string
  active: boolean
  disabled?: boolean
  /** true 表示在这一项之前画一条分隔线（分组用，不参与按钮）。 */
  separatorBefore?: boolean
  onClick: () => void
}

export function ActivityBar() {
  const sidebarView = useWorkspaceStore((state) => state.sidebarView)
  const sidebarCollapsed = useWorkspaceStore((state) => state.layout.sidebarCollapsed)
  const agentCollapsed = useWorkspaceStore((state) => state.layout.agentCollapsed)
  const bottomPanelCollapsed = useWorkspaceStore((state) => state.layout.bottomPanelCollapsed)
  const bottomPanelTab = useWorkspaceStore((state) => state.bottomPanelTab)
  const setSidebarView = useWorkspaceStore((state) => state.setSidebarView)
  const setBottomPanelTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)
  const toggleBottomPanel = useWorkspaceStore((state) => state.toggleBottomPanel)

  // 上半是 bot 工作台的面（这个 bot 是什么、在做什么），下半是 IDE 视图。
  // 面的顺序与 Mino 侧栏一致：概览在最上，其余按「从身份到运行」排列。
  const items: ActivityItem[] = [
    {
      id: 'overview',
      label: '概览',
      icon: '◱',
      active: sidebarView === 'overview' && !sidebarCollapsed,
      onClick: () => setSidebarView('overview'),
    },
    {
      id: 'bots',
      label: 'Bot',
      icon: '▣',
      active: sidebarView === 'bots' && !sidebarCollapsed,
      onClick: () => setSidebarView('bots'),
    },
    {
      id: 'capabilities',
      label: '能力',
      icon: '⊞',
      active: sidebarView === 'capabilities' && !sidebarCollapsed,
      onClick: () => setSidebarView('capabilities'),
    },
    {
      id: 'conversations',
      label: '会话',
      icon: '✉',
      active: sidebarView === 'conversations' && !sidebarCollapsed,
      onClick: () => setSidebarView('conversations'),
    },
    {
      id: 'runtime',
      label: '运行态',
      icon: '◷',
      active: sidebarView === 'runtime' && !sidebarCollapsed,
      onClick: () => setSidebarView('runtime'),
    },
    {
      id: 'channels',
      label: '渠道',
      icon: '⇄',
      active: sidebarView === 'channels' && !sidebarCollapsed,
      onClick: () => setSidebarView('channels'),
    },
    {
      id: 'explorer',
      label: '资源管理器',
      icon: '▤',
      separatorBefore: true,
      active: sidebarView === 'explorer' && !sidebarCollapsed,
      onClick: () => setSidebarView('explorer'),
    },
    {
      id: 'search',
      label: '全局搜索（Ctrl+Shift+F）',
      icon: '⌕',
      // openSearch() 自己会展开底部面板并抢焦点（与快捷键、命令面板同一入口）。
      active: bottomPanelTab === 'search' && !bottomPanelCollapsed,
      onClick: () => openSearch(),
    },
    {
      id: 'run',
      label: '运行与日志',
      icon: '▶',
      active: bottomPanelTab === 'logs' && !bottomPanelCollapsed,
      onClick: () => setBottomPanelTab('logs'),
    },
    {
      id: 'problems',
      label: '问题',
      icon: '⚠',
      active: bottomPanelTab === 'problems' && !bottomPanelCollapsed,
      onClick: () => setBottomPanelTab('problems'),
    },
    {
      id: 'debug',
      label: '调试',
      icon: '◈',
      active: bottomPanelTab === 'debug' && !bottomPanelCollapsed,
      onClick: () => setBottomPanelTab('debug'),
    },
    {
      id: 'agent',
      label: agentCollapsed ? '显示 Agent 栏（Ctrl+Shift+A）' : '隐藏 Agent 栏（Ctrl+Shift+A）',
      icon: '✧',
      active: !agentCollapsed,
      onClick: () => toggleAgent(),
    },
    {
      id: 'settings',
      label: '设置',
      icon: '⚙',
      active: sidebarView === 'settings' && !sidebarCollapsed,
      onClick: () => setSidebarView('settings'),
    },
  ]

  return (
    <nav className="activity-bar" aria-label="活动栏">
      {items.map((item) => (
        <div key={item.id} className="activity-bar__slot">
          {item.separatorBefore === true ? (
            <span className="activity-bar__separator" aria-hidden="true" />
          ) : null}
          <button
            type="button"
            className={'activity-bar__button' + (item.active ? ' activity-bar__button--active' : '')}
            aria-label={item.label}
            aria-pressed={item.active}
            title={item.label}
            disabled={item.disabled}
            onClick={() => {
              item.onClick()
              // 侧栏展开 / 底部面板展开：被折叠时点活动栏应当重新显示。
              if (item.id === 'run' || item.id === 'problems' || item.id === 'debug') {
                if (bottomPanelCollapsed) toggleBottomPanel(false)
              }
            }}
          >
            <span aria-hidden="true">{item.icon}</span>
          </button>
        </div>
      ))}
    </nav>
  )
}
