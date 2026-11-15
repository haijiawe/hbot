/**
 * Agent 栏：消息列表 + 实时进度 + 审批卡片 + Diff 卡片 + 输入框。
 *
 * 所有内容都来自核心事件（见 `stores/agent-store.ts`），这里不编任何状态：
 * 没有会话就没有"待审阅改动"占位按钮 —— 空就是空。
 */

import { useEffect, useRef } from 'react'
import { formatClock } from '../../lib/format'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { ApprovalCard } from './approval-card'
import { DiffCard } from './diff-card'

export function AgentPanel() {
  const sessionId = useAgentStore((state) => state.sessionId)
  const messages = useAgentStore((state) => state.messages)
  const draft = useAgentStore((state) => state.draft)
  const sending = useAgentStore((state) => state.sending)
  const step = useAgentStore((state) => state.step)
  const tool = useAgentStore((state) => state.tool)
  const edit = useAgentStore((state) => state.edit)
  const approval = useAgentStore((state) => state.approval)
  const error = useAgentStore((state) => state.error)
  const setDraft = useAgentStore((state) => state.setDraft)
  const send = useAgentStore((state) => state.send)
  const answerApproval = useAgentStore((state) => state.answerApproval)
  const acceptEdit = useAgentStore((state) => state.acceptEdit)
  const rejectEdit = useAgentStore((state) => state.rejectEdit)
  const dismissError = useAgentStore((state) => state.dismissError)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)

  const listRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    const list = listRef.current
    if (list) list.scrollTop = list.scrollHeight
  }, [messages, sending, approval, edit])

  return (
    <section className="agent" aria-label="Agent">
      <header className="panel-header">
        <h2 className="panel-header__title">Agent</h2>
        <span className="panel-header__hint" title={sessionId ?? '还没有会话'}>
          {sessionId ? `会话 ${sessionId.slice(0, 8)}` : '新会话'}
        </span>
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

      {approval ? <ApprovalCard approval={approval} onAnswer={(decision, always) => void answerApproval(decision, always)} /> : null}

      {edit ? (
        <DiffCard edit={edit} onAccept={() => void acceptEdit()} onReject={() => void rejectEdit()} />
      ) : null}

      {step > 0 || tool ? (
        <p className="agent__status" role="status">
          {step > 0 ? `第 ${step} 步` : ''}
          {step > 0 && tool ? ' · ' : ''}
          {tool ? `工具 ${tool.name}（${tool.status}）` : ''}
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