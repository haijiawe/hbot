/** 运行日志：只读列表，aria-live 播报，级别同时用文字与颜色表达。 */

import { useEffect, useRef } from 'react'
import { formatClock, logLevelLabel } from '../../lib/format'
import { useRunStore } from '../../stores/run-store'

export function RunLog() {
  const logs = useRunStore((state) => state.logs)
  const listRef = useRef<HTMLOListElement | null>(null)

  useEffect(() => {
    const list = listRef.current
    if (!list) return
    list.scrollTop = list.scrollHeight
  }, [logs])

  if (logs.length === 0) {
    return (
      <p className="panel-empty" role="status">
        尚未启动 bot，暂无日志。
      </p>
    )
  }

  return (
    <ol className="logs" ref={listRef} aria-live="polite" aria-label="运行日志">
      {logs.map((line, index) => (
        <li key={`${line.at}-${index}`} className={`log log--${line.level}`}>
          <time className="log__time" dateTime={new Date(line.at).toISOString()}>
            {formatClock(line.at)}
          </time>
          <span className="log__level">{logLevelLabel(line.level)}</span>
          <span className="log__source">{line.source}</span>
          <span className="log__message">{line.message}</span>
        </li>
      ))}
    </ol>
  )
}