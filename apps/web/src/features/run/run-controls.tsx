/** 启停控制：状态只来自 runStore，按钮防重复点击。 */

import type { BotStatus } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'

const STATUS_TEXT: Record<BotStatus, string> = {
  stopped: '已停止',
  starting: '启动中',
  running: '运行中',
  stopping: '停止中',
  error: '出错',
}

export function RunControls() {
  const status = useRunStore((state) => state.status)
  const busy = useRunStore((state) => state.busy)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)
  const running = status === 'running' || status === 'starting'

  return (
    <div className="run-controls">
      <span className={`status-dot status-dot--${status}`} aria-hidden="true" />
      <span className="run-controls__status">{STATUS_TEXT[status]}</span>
      <button
        type="button"
        className="button button--primary"
        onClick={() => void start()}
        disabled={busy || running}
        aria-label="启动 bot"
      >
        启动
      </button>
      <button
        type="button"
        className="button"
        onClick={() => void stop()}
        disabled={busy || !running}
        aria-label="停止 bot"
      >
        停止
      </button>
    </div>
  )
}