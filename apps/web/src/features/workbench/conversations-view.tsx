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
      setError('读取会话失败：' + errorMessage(cause))
    }
  }, [botId])

  useEffect(() => {
    if (projectRoot === null) return
    if (bots.length === 0) void refresh()
    void load()
  }, [projectRoot, bots.length, refresh, load])

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">CONVERSATIONS</div>
        <h2 className="wb-title">会话</h2>
        <span className="wb-pill">{items.length} 个已登记</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void load()} disabled={botId.length === 0}>
            刷新
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : error !== null ? (
        <p className="wb-error">{error}</p>
      ) : items.length === 0 ? (
        <p className="wb-empty">
          还没有会话。启动 bot 后，平台上的第一条消息会在这里登记。本地平台在「试跑」里发一条就会建出 local:default。
        </p>
      ) : (
        <ul className="wb-list">
          {items.map((item) => (
            <li key={item.conversationId} className="wb-item">
              <div className="wb-item__main">
                <span className="wb-item__name">{item.externalId}</span>
                <span className="wb-item__meta">{item.kind}</span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
