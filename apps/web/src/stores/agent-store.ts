/**
 * agentStore：Agent 会话、待审阅 Diff、审批状态。
 *
 * 不引用 workspaceStore / runStore；也不直接改文件——Diff 应用必须由核心执行
 * （AGENTS.md §0.10：Agent 写入必须走 Diff，逐块接受/拒绝）。
 */

import { create } from 'zustand'
import { errorMessage } from '../lib/format'
import { dispatch } from '../lib/ipc'

export type AgentRole = 'user' | 'agent' | 'system'

export interface AgentMessage {
  id: string
  role: AgentRole
  text: string
  at: number
}

export type AgentDiffStatus = 'pending' | 'accepted' | 'rejected'

/** M0 的 Diff 卡片只展示摘要；真正的分块审阅需要 M1 的 `agent.diff` 事件负载。 */
export interface AgentDiff {
  id: string
  path: string
  summary: string
  added: number
  removed: number
  status: AgentDiffStatus
}

export type ApprovalState = 'idle' | 'awaiting'

export interface AgentState {
  messages: AgentMessage[]
  draft: string
  sending: boolean
  diff: AgentDiff | null
  approval: ApprovalState
  error: string | null

  setDraft: (draft: string) => void
  send: (text: string) => Promise<void>
  previewDiff: () => void
  resolveDiff: (status: Exclude<AgentDiffStatus, 'pending'>) => void
  dismissError: () => void
}

function nextId(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`
}

export const useAgentStore = create<AgentState>()((set, get) => ({
  messages: [
    {
      id: 'welcome',
      role: 'system',
      text: 'M0 壳演示：这里的回复来自 ipc mock。真实 Agent 循环在 crates/hbot-agent（Rust），M1 接入。',
      at: Date.now(),
    },
  ],
  draft: '',
  sending: false,
  diff: null,
  approval: 'idle',
  error: null,

  setDraft(draft) {
    set({ draft })
  },

  async send(text) {
    const trimmed = text.trim()
    if (trimmed.length === 0 || get().sending) return

    set((state) => ({
      messages: [
        ...state.messages,
        { id: nextId('user'), role: 'user', text: trimmed, at: Date.now() },
      ],
      draft: '',
      sending: true,
      error: null,
    }))

    try {
      const reply = await dispatch({ type: 'agent.send', text: trimmed })
      set((state) => ({
        messages: [
          ...state.messages,
          { id: reply.messageId, role: 'agent', text: reply.reply, at: Date.now() },
        ],
        sending: false,
      }))
    } catch (error) {
      set({ sending: false, error: `发送失败：${errorMessage(error)}` })
    }
  },

  previewDiff() {
    set({
      diff: {
        id: nextId('diff'),
        path: 'src/lib/reply.ts',
        summary: '把回复前缀从「收到：」改成「已收到：」',
        added: 1,
        removed: 1,
        status: 'pending',
      },
      approval: 'awaiting',
    })
  },

  resolveDiff(status) {
    const diff = get().diff
    if (!diff) return
    set({ diff: { ...diff, status }, approval: 'idle' })
  },

  dismissError() {
    set({ error: null })
  },
}))