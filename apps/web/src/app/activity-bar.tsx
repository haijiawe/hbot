/**
 * 活动栏：切换侧栏视图、开关 Agent 栏。
 *
 * **只有「面」才在这里**（一个面 = 一块能独立回答问题的界面）。底部面板的页签不在这里
 * 再放一份镜像按钮 —— 那会让 16 个图标里有一半点下去只是换底部页签，与页签本身重复
 * （2026-11-21 UI 收敛）。底部页签用 Ctrl+J 展开后直接点，命令面板里也都有。
 */

import { useWorkspaceStore } from '../stores/workspace-store'

interface ActivityItem {
  id: string
  label: string
  icon: string
  active: boolean
  /** true 表示在这一项之前画一条分隔线（分组用，不参与按钮）。 */
  separatorBefore?: boolean
  onClick: () => void
}

export function ActivityBar() {
  const sidebarView = useWorkspaceStore((state) => state.sidebarView)
  const sidebarCollapsed = useWorkspaceStore((state) => state.layout.sidebarCollapsed)
  const agentCollapsed = useWorkspaceStore((state) => state.layout.agentCollapsed)
  const setSidebarView = useWorkspaceStore((state) => state.setSidebarView)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)
  const theme = useWorkspaceStore((state) => state.theme)
  const setTheme = useWorkspaceStore((state) => state.setTheme)

  // 上半是 bot 工作台的面（这个 bot 是什么、在做什么），下半是工具面。
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
      id: 'board',
      label: '作业看板',
      icon: '▦',
      active: sidebarView === 'board' && !sidebarCollapsed,
      onClick: () => setSidebarView('board'),
    },
    {
      id: 'docs',
      label: '设计文档',
      icon: '▧',
      active: sidebarView === 'docs' && !sidebarCollapsed,
      onClick: () => setSidebarView('docs'),
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
      separatorBefore: true,
      active: sidebarView === 'settings' && !sidebarCollapsed,
      onClick: () => setSidebarView('settings'),
    },
  ]

  return (
    <nav className='activity-bar' aria-label='活动栏'>
      {/* 品牌块：Mino 的 .brand-mark。只有装饰作用，所以 aria-hidden。 */}
      <div className='activity-bar__brand' aria-hidden='true'>
        H
      </div>
      {items.map((item) => (
        <div key={item.id} className='activity-bar__slot'>
          {item.separatorBefore === true ? (
            <span className='activity-bar__separator' aria-hidden='true' />
          ) : null}
          <button
            type='button'
            className={'activity-bar__button' + (item.active ? ' activity-bar__button--active' : '')}
            aria-label={item.label}
            aria-pressed={item.active}
            title={item.label}
            onClick={() => item.onClick()}
          >
            <span aria-hidden='true'>{item.icon}</span>
          </button>
        </div>
      ))}
      {/* 主题切换贴在底部：它是「这台机器怎么显示」，不属于任何一个面。 */}
      <div className='activity-bar__footer'>
        <button
          type='button'
          className='activity-bar__button'
          aria-label={theme === 'dark' ? '切换到浅色主题' : '切换到深色主题'}
          title={theme === 'dark' ? '切换到浅色主题' : '切换到深色主题'}
          onClick={() => setTheme(theme === 'dark' ? 'light' : 'dark')}
        >
          <span aria-hidden='true'>{theme === 'dark' ? '☾' : '☀'}</span>
        </button>
      </div>
    </nav>
  )
}
