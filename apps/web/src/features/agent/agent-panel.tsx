/** Agent 栏：消息列表 + 输入框 + Diff 卡片。 */

import { useEffect, useRef } from 'react'
import { formatClock } from '../../lib/format'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { DiffCard } from './diff-card'

export function AgentPanel() {
  const messages = useAgentStore((state) => state.messages)
  const draft = useAgentStore((state) => state.draft)
  const sending = useAgentStore((state) => state.sending)
  const diff = useAgentStore((state) => state.diff)
  const approval = useAgentStore((state) => state.approval)
  const error = useAgentStore((state) => state.error)
  const setDraft = useAgentStore((state) => state.setDraft)
  const send = useAgentStore((state) => state.send)
  const previewDiff = useAgentStore((state) => state.previewDiff)
  const resolveDiff = useAgentStore((state) => state.resolveDiff)
  const dismissError = useAgentStore((state) => state.dismissError)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)

  const listRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    const list = listRef.current
    if (list) list.scrollTop = list.scrollHeight
  }, [messages, sending])

  return (
    <section className="agent" aria-label="Agent">
      <header className="panel-header">
        <h2 className="panel-header__title">Agent</h2>
        <button
          type="button"
          className="icon-button"
          onClick={() => toggleAgent(true)}
          aria-label="隐藏 Agent 栏"
          title="隐藏 Agent 栏（Ctrl+Shift+A）"
        >
          ›
        </button>
      </header>

      <div className="agent__messages" ref={listRef} aria-live="polite" aria-label="Agent 消息">
        {messages.length === 0 ? (
          <p className="panel-empty">还没有对话。输入一句话开始。</p>
        ) : (
          <ul className="messages">
            {messages.map((message) => (
              <li key={message.id} className={`message message--${message.role}`}>
                <header className="message__meta">
                  <span className="message__role">
                    {message.role === 'user' ? '你' : message.role === 'agent' ? 'Agent' : '系统'}
                  </span>
                  <time className="message__time" dateTime={new Date(message.at).toISOString()}>
                    {formatClock(message.at)}
                  </time>
                </header>
                <p className="message__text">{message.text}</p>
              </li>
            ))}
            {sending ? (
              <li className="message message--agent message--pending" role="status">
                <p className="message__text">Agent 正在处理…</p>
              </li>
            ) : null}
          </ul>
        )}
      </div>

      {error ? (
        <div className="inline-error" role="alert">
          <span>{error}</span>
          <button type="button" className="icon-button" onClick={dismissError} aria-label="关闭错误提示">
            ×
          </button>
        </div>
      ) : null}

      {diff ? (
        <DiffCard diff={diff} onResolve={resolveDiff} />
      ) : (
        <div className="agent__diff-placeholder">
          <p className="panel-hint">暂无待审阅改动。</p>
          <button
            type="button"
            className="button"
            onClick={previewDiff}
            aria-label="预览 Diff 卡片（占位）"
          >
            预览 Diff 卡片
          </button>
        </div>
      )}

      {approval === 'awaiting' ? (
        <p className="agent__approval" role="status">
          等待审阅：Agent 的写入被拦在审批这一步，接受后才由核心执行。
        </p>
      ) : null}

      <form
        className="composer"
        onSubmit={(event) => {
          event.preventDefault()
          void send(draft)
        }}
      >
        <textarea
          className="composer__input"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault()
              void send(draft)
            }
          }}
          placeholder="描述你想让 Agent 做的事（Enter 发送，Shift+Enter 换行）"
          aria-label="给 Agent 的消息"
          rows={3}
        />
        <div className="composer__actions">
          <button
            type="submit"
            className="button button--primary"
            disabled={sending || draft.trim().length === 0}
            aria-label="发送消息"
          >
            {sending ? '发送中…' : '发送'}
          </button>
        </div>
      </form>
    </section>
  )
}