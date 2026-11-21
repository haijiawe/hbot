/**
 * Bot 面：这个项目的全部 bot，一个一行（状态、平台、启停）。
 *
 * 与底部「试跑」页签的区别：那边是"发一条消息看回复"，这里是"有几台、谁在跑"。
 */

import { useEffect } from 'react'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { BOT_STATUS_LABEL, statusDotClass } from './bot-status'

export function BotsView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const bots = useRunStore((state) => state.bots)
  const botId = useRunStore((state) => state.botId)
  const busy = useRunStore((state) => state.busy)
  const refresh = useRunStore((state) => state.refresh)
  const selectBot = useRunStore((state) => state.selectBot)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)

  useEffect(() => {
    if (projectRoot === null) return
    void refresh()
  }, [projectRoot, refresh])

  const running = bots.filter((bot) => bot.status === 'running' || bot.status === 'starting').length

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">ENTRIES</div>
        <h2 className="wb-title">Bot</h2>
        <span className="wb-pill">{running} / {bots.length} 在跑</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void refresh()} disabled={projectRoot === null}>
            刷新
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : bots.length === 0 ? (
        <p className="wb-empty">
          这个项目没有声明 bot。在项目根的 bots.json 里加一条（platform 写 local 就能本地试跑）。
        </p>
      ) : (
        <ul className="wb-list">
          {bots.map((bot) => {
            const active = bot.botId === botId
            const isRunning = bot.status === 'running' || bot.status === 'starting'
            return (
              <li key={bot.botId} className={active ? 'wb-item wb-item--active' : 'wb-item'}>
                <button className="wb-item__main" type="button" onClick={() => selectBot(bot.botId)}>
                  <span className={statusDotClass(bot.status)} aria-hidden="true" />
                  <span className="wb-item__name">{bot.name}</span>
                  <span className="wb-item__meta">
                    {BOT_STATUS_LABEL[bot.status]} · {bot.platform}
                  </span>
                </button>
                <button
                  className={isRunning ? 'wb-btn' : 'wb-btn wb-btn--primary'}
                  type="button"
                  disabled={busy}
                  onClick={() => {
                    selectBot(bot.botId)
                    if (isRunning) void stop()
                    else void start()
                  }}
                >
                  {isRunning ? '停止' : '启动'}
                </button>
              </li>
            )
          })}
        </ul>
      )}
    </div>
  )
}
