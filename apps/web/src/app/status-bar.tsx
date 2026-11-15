/** 状态栏：文件、脏标记、bot 状态、运行时后端、错误提示。 */

import { basename } from '../lib/paths'
import { runtimeMode } from '../lib/ipc'
import type { BotStatus } from '../lib/ipc'
import { useRunStore } from '../stores/run-store'
import { useWorkspaceStore } from '../stores/workspace-store'

const BOT_TEXT: Record<BotStatus, string> = {
  stopped: 'bot 已停止',
  starting: 'bot 启动中',
  running: 'bot 运行中',
  stopping: 'bot 停止中',
  degraded: 'bot 降级',
}

interface StatusBarProps {
  notice: string | null
}

export function StatusBar({ notice }: StatusBarProps) {
  const activePath = useWorkspaceStore((state) => state.activePath)
  const activeTab = useWorkspaceStore((state) =>
    state.tabs.find((tab) => tab.path === state.activePath),
  )
  const error = useWorkspaceStore((state) => state.error)
  const dismissError = useWorkspaceStore((state) => state.dismissError)
  const tabCount = useWorkspaceStore((state) => state.tabs.length)
  const botStatus = useRunStore((state) => state.status)
  const botId = useRunStore((state) => state.botId)

  return (
    <footer className="status-bar" aria-label="状态栏">
      <div className="status-bar__left">
        {activePath ? (
          <span className="status-bar__item" title={activePath}>
            {basename(activePath)}
            {activeTab?.dirty ? ' ●' : ''}
          </span>
        ) : (
          <span className="status-bar__item status-bar__item--muted">未打开文件</span>
        )}
        <span className={`status-bar__item status-bar__bot status-bar__bot--${botStatus}`}>
          <span className={`status-dot status-dot--${botStatus}`} aria-hidden="true" />
          {BOT_TEXT[botStatus]}
          {botId.length > 0 ? `（${botId}）` : ''}
        </span>
        {notice ? (
          <span className="status-bar__item status-bar__notice" role="status">
            {notice}
          </span>
        ) : null}
        {error ? (
          <span className="status-bar__item status-bar__error" role="alert">
            {error}
            <button
              type="button"
              className="status-bar__dismiss"
              onClick={dismissError}
              aria-label="关闭错误提示"
            >
              ×
            </button>
          </span>
        ) : null}
      </div>
      <div className="status-bar__right">
        <span className="status-bar__item status-bar__item--muted">{tabCount} 个标签</span>
        <span className="status-bar__item status-bar__item--muted">
          {runtimeMode() === 'tauri' ? 'Tauri' : '浏览器 mock'}
        </span>
      </div>
    </footer>
  )
}