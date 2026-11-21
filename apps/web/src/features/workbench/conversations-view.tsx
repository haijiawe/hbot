/**
 * 会话面：当前 bot 已登记的会话（`bot_conversations`）。
 *
 * 这不是聊天窗口：消息内容在「日志」里（`run.log`），这里只回答"有哪些会话、
 * 各自是什么形态"，以及给本地试跑一个入口。
 */

import { useCallback, useEffect, useState } from 'react'
import { errorMessage } from '../../lib/format'
import type { ConversationSummary } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

export function ConversationsView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const botId = useRunStore((state) => state.botId)
  const bots = useRunStore((state) => state.bots)
  const refresh = useRunStore((state) => state.refresh)
  const [items, setItems] = useState<ConversationSummary[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    if (botId.length === 0) {
      setItems([])
      return
    }
    try {
      const rows = await dispatch({ type: 'bot_conversations', botId })
      setItems(rows)
      setError(null)
    } catch (cause) {
      // 会话表是运行时产物：bot 没跑过就可能是空的，也可能还没建表，原文给用户看。
      setError(`读取会话失败：${errorMessage(cause)}`)
    }
  }, [botId])

  useEffect(() => {
    if (projectRoot === null) return
    if (bots.length === 0) void refresh()
    void load()
  }, [projectRoot, bots.length, refresh, load])

  return (
    <div className="wb">
      <header className="wb__header">
        <h2 className="wb__title">会话</h2>
        <button className="button" type="button" onClick={() => void load()} disabled={botId.length === 0}>
          刷新
        </button>
      </header>
      {projectRoot === null ? (
        <p className="panel-hint">先打开一个项目。</p>
      ) : error !== null ? (
        <p className="wb__error">{error}</p>
      ) : items.length === 0 ? (
        <p className="panel-hint">还没有会话。启动 bot 后，平台上的第一条消息会在这里登记。
          本地平台在「试跑」里发一条就会建出 local:default。</p>
      ) : (
        <ul className="wb__list">
          {items.map((item) => (
            <li key={item.conversationId} className="wb__item">
              <div className="wb__item-main wb__item-main--static">
                <span className="wb__value">{item.externalId}</span>
                <span className="wb__meta">{item.kind}</span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
