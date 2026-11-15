/**
 * 底部面板（lazy chunk）：日志 / 问题 / 搜索 / 调试四个页签 + 启停控制。
 *
 * 四个页签共用同一块区域，高度只来自 `lib/layout.ts` 的 `--bottom-panel-height`，
 * 不另起第二套布局常量。默认导出是 React.lazy 的要求。
 */

import { formatKeybinding, keybindingById } from '../../lib/keybindings'
import { useDebugStore } from '../../stores/debug-store'
import { useRunStore } from '../../stores/run-store'
import { useSearchStore } from '../../stores/search-store'
import { useWorkspaceStore, type BottomPanelTab } from '../../stores/workspace-store'
import { DebugPanel } from '../debug/debug-panel'
import { ProblemsPanel } from '../problems/problems-panel'
import { SearchPanel } from '../search/search-panel'
import { RunControls } from '../run/run-controls'
import { RunLog } from '../run/run-log'

const TABS: readonly BottomPanelTab[] = ['logs', 'problems', 'search', 'debug']
const TAB_LABEL: Record<BottomPanelTab, string> = {
  logs: '日志',
  problems: '问题',
  search: '搜索',
  debug: '调试',
}
const TAB_ID: Record<BottomPanelTab, string> = {
  logs: 'bottom-tab-logs',
  problems: 'bottom-tab-problems',
  search: 'bottom-tab-search',
  debug: 'bottom-tab-debug',
}

/** 页签上的计数（`null` = 不显示徽标）。 */
function tabCount(tab: BottomPanelTab, logCount: number, searchCount: number): number | null {
  switch (tab) {
    case 'logs':
      return logCount
    case 'search':
      return searchCount
    case 'problems':
    case 'debug':
      return null
    default: {
      const exhaustive: never = tab
      return exhaustive
    }
  }
}

function countLabel(tab: BottomPanelTab, count: number): string {
  if (tab === 'logs') return `${count} 行`
  if (tab === 'search') return `${count} 处匹配`
  return `${count} 项`
}

function panelBody(tab: BottomPanelTab) {
  switch (tab) {
    case 'logs':
      return <RunLog />
    case 'problems':
      return <ProblemsPanel />
    case 'search':
      return <SearchPanel />
    case 'debug':
      return <DebugPanel />
    default: {
      const exhaustive: never = tab
      return exhaustive
    }
  }
}

export default function BottomPanel() {
  const tab = useWorkspaceStore((state) => state.bottomPanelTab)
  const setTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const toggleBottomPanel = useWorkspaceStore((state) => state.toggleBottomPanel)
  const logCount = useRunStore((state) => state.logs.length)
  const clearLogs = useRunStore((state) => state.clearLogs)
  const searchCount = useSearchStore((state) => state.totalMatches)
  // 暂停时页签上挂一个点点提示（不改变计数语义，只是让"别的页签里有暂停"看得见）。
  const paused = useDebugStore((state) => state.paused !== null)

  return (
    <section className="bottom-panel" aria-label="底部面板">
      <header className="bottom-panel__header">
        <div className="bottom-panel__tabs" role="tablist" aria-label="底部面板页签">
          {TABS.map((item) => {
            const count = tabCount(item, logCount, searchCount)
            return (
              <button
                key={item}
                type="button"
                role="tab"
                id={TAB_ID[item]}
                aria-label={TAB_LABEL[item]}
                aria-selected={tab === item}
                aria-controls="bottom-panel-body"
                className={`panel-tab${tab === item ? ' panel-tab--active' : ''}`}
                onClick={() => setTab(item)}
              >
                {TAB_LABEL[item]}
                {item === 'debug' && paused ? (
                  <span className="panel-tab__dot" aria-label="已暂停" />
                ) : null}
                {count === null ? null : (
                  <span className="panel-tab__count" aria-label={countLabel(item, count)}>
                    {count}
                  </span>
                )}
              </button>
            )
          })}
        </div>

        <div className="bottom-panel__actions">
          <RunControls />
          {tab === 'logs' ? (
            <button
              type="button"
              className="icon-button"
              onClick={clearLogs}
              aria-label="清空日志"
              title="清空日志"
            >
              清
            </button>
          ) : null}
          <button
            type="button"
            className="icon-button"
            onClick={() => toggleBottomPanel(true)}
            aria-label="隐藏底部面板"
            title={`隐藏底部面板（${formatKeybinding(keybindingById('toggle-bottom-panel'))}）`}
          >
            ▾
          </button>
        </div>
      </header>

      <div
        className="bottom-panel__body"
        id="bottom-panel-body"
        role="tabpanel"
        aria-labelledby={TAB_ID[tab]}
      >
        {panelBody(tab)}
      </div>
    </section>
  )
}