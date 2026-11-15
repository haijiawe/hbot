/**
 * 底部面板（lazy chunk）：日志 / 问题两个页签 + 启停控制。
 *
 * 默认导出是 React.lazy 的要求。
 */

import { useWorkspaceStore } from '../../stores/workspace-store'
import { useRunStore } from '../../stores/run-store'
import { ProblemsPanel } from '../problems/problems-panel'
import { RunControls } from '../run/run-controls'
import { RunLog } from '../run/run-log'

export default function BottomPanel() {
  const tab = useWorkspaceStore((state) => state.bottomPanelTab)
  const setTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const toggleBottomPanel = useWorkspaceStore((state) => state.toggleBottomPanel)
  const logCount = useRunStore((state) => state.logs.length)
  const clearLogs = useRunStore((state) => state.clearLogs)

  return (
    <section className="bottom-panel" aria-label="底部面板">
      <header className="bottom-panel__header">
        <div className="bottom-panel__tabs" role="tablist" aria-label="底部面板页签">
          <button
            type="button"
            role="tab"
            id="bottom-tab-logs"
            aria-label="日志"
            aria-selected={tab === 'logs'}
            aria-controls="bottom-panel-body"
            className={`panel-tab${tab === 'logs' ? ' panel-tab--active' : ''}`}
            onClick={() => setTab('logs')}
          >
            日志
            <span className="panel-tab__count" aria-label={`${logCount} 行`}>
              {logCount}
            </span>
          </button>
          <button
            type="button"
            role="tab"
            id="bottom-tab-problems"
            aria-label="问题"
            aria-selected={tab === 'problems'}
            aria-controls="bottom-panel-body"
            className={`panel-tab${tab === 'problems' ? ' panel-tab--active' : ''}`}
            onClick={() => setTab('problems')}
          >
            问题
          </button>
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
            title="隐藏底部面板（Ctrl+J）"
          >
            ▾
          </button>
        </div>
      </header>

      <div
        className="bottom-panel__body"
        id="bottom-panel-body"
        role="tabpanel"
        aria-labelledby={tab === 'logs' ? 'bottom-tab-logs' : 'bottom-tab-problems'}
      >
        {tab === 'logs' ? <RunLog /> : <ProblemsPanel />}
      </div>
    </section>
  )
}