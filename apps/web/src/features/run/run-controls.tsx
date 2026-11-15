/**
 * 启停控制：状态只来自 runStore（由 `bot.status` 事件推进），按钮防重复点击。
 *
 * 错误必须显示在这里：`runStore.error` 以前只写进 store、没有任何组件渲染，
 * 于是 `bot.start` 失败（例如核心回 `-32006 尚未实现：bot_start`）在界面上
 * 看起来像"点了没反应"。失败就报错，不许当成成功。
 */

import type { BotStatus } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'

const STATUS_TEXT: Record<BotStatus, string> = {
  stopped: '已停止',
  starting: '启动中',
  running: '运行中',
  stopping: '停止中',
  degraded: '降级运行',
}

export function RunControls() {
  const status = useRunStore((state) => state.status)
  const busy = useRunStore((state) => state.busy)
  const error = useRunStore((state) => state.error)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)
  const dismissError = useRunStore((state) => state.dismissError)
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
      {error ? (
        <div className="inline-error" role="alert">
          <span>{error}</span>
          <button
            type="button"
            className="icon-button"
            onClick={dismissError}
            aria-label="关闭错误提示"
          >
            ×
          </button>
        </div>
      ) : null}
    </div>
  )
}