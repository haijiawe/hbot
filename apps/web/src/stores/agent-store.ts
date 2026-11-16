/**
 * agentStore：Agent 会话、流式输出、待审阅 Diff、审批。
 *
 * 输入全部来自核心事件（`agent.delta` / `agent.step` / `agent.tool` /
 * `approval.request` / `agent.diff`），用户动作全部经 `dispatch`：
 * - 会话 ID：第一条消息带 `sessionId: null`（= 新建），把核心回的 ID 存下来续用；
 * - 审批：`agent_approve`（`approvalId` + `decision` + `always`）；
 * - 编辑：`edit_apply`（`editId` + `acceptHunks`）/ `edit_reject`。
 *
 * 不引用 workspaceStore / runStore；也不直接改文件——写入必须由核心执行
 * （AGENTS.md §0.10：Agent 写入必须走 Diff，逐块接受/拒绝）。
 */

import { create } from 'zustand'
import type { EditFile, RiskLevel, ToolCall } from '../generated'
import type { EditApplyHooks } from '../lib/edit-apply'
import { errorMessage } from '../lib/format'
import type { CoreEvent } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

export type AgentRole = 'user' | 'agent' | 'system'

export interface AgentMessage {
  id: string
  role: AgentRole
  text: string
  at: number
}

export type AgentDiffStatus = 'pending' | 'accepted' | 'rejected'

/** 待审阅的一次编辑（`agent.diff` 事件）：可能跨多个文件，按块接受。 */
export interface PendingEdit {
  editId: string
  files: EditFile[]
  status: AgentDiffStatus
}

/** 待答复的审批（`approval.request` 事件）：问是非，不是问内容。 */
export interface PendingApproval {
  approvalId: string
  tool: string
  risk: RiskLevel
  /** 工具参数原样展示（拓扑未知，渲染时再 stringify）。 */
  args: unknown
  /** 正在回传答复（防重复点击）。 */
  busy: boolean
}

export type ApprovalDecisionInput = 'approve' | 'deny'

export interface AgentState {
  /** 核心分配的会话 ID；`null` = 还没发过消息（下一条会新建会话）。 */
  sessionId: string | null
  messages: AgentMessage[]
  draft: string
  /** 消息命令在途（只用于禁用按钮；回合结束契约里没有事件，见文件头注释）。 */
  sending: boolean
  /** 最近一次 `agent.step` 的步号（0 = 还没有）。 */
  step: number
  /** 最近一次 `agent.tool` 的工具调用（含状态与结果）。 */
  tool: ToolCall | null
  edit: PendingEdit | null
  approval: PendingApproval | null
  error: string | null
  /** 正在追加的流式消息 id：`agent.delta` 往这条上拼。 */
  streamId: string | null

  setDraft: (draft: string) => void
  send: (text: string) => Promise<void>
  ingestEvent: (event: CoreEvent) => void
  answerApproval: (decision: ApprovalDecisionInput, always: boolean) => Promise<void>
  acceptEdit: (hooks?: EditApplyHooks) => Promise<void>
  rejectEdit: () => Promise<void>
  dismissError: () => void
}

function nextId(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`
}

/** 统计一次编辑的增删行数（DiffHunk 的行以 `+`/`-` 开头，与统一 diff 一致）。 */
export function editStats(files: EditFile[]): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const file of files) {
    for (const hunk of file.hunks) {
      for (const line of hunk.lines) {
        if (line.startsWith('+')) added += 1
        else if (line.startsWith('-')) removed += 1
      }
    }
  }
  return { added, removed }
}

/** 全部块序号（"整份接受"用；核心要求显式列出接受的块）。 */
function allHunkIndices(files: EditFile[]): number[] {
  return files.flatMap((file) => file.hunks.map((hunk) => hunk.index))
}

export const useAgentStore = create<AgentState>()((set, get) => ({
  sessionId: null,
  messages: [
    {
      id: 'welcome',
      role: 'system',
      text: 'Agent 循环跑在核心（crates/hbot-agent）：这里显示它的流式输出，它的写入会以 Diff 卡片让你逐块审阅。',
      at: Date.now(),
    },
  ],
  draft: '',
  sending: false,
  step: 0,
  tool: null,
  edit: null,
  approval: null,
  error: null,
  streamId: null,

  setDraft(draft) {
    set({ draft })
  },

  async send(text) {
    const trimmed = text.trim()
    if (trimmed.length === 0 || get().sending) return

    const sessionId = get().sessionId
    set((state) => ({
      messages: [
        ...state.messages,
        { id: nextId('user'), role: 'user', text: trimmed, at: Date.now() },
      ],
      draft: '',
      sending: true,
      error: null,
      // 新一轮：上一次的流式消息不能再被追加（否则两轮的话会粘成一条）。
      streamId: null,
      step: 0,
      tool: null,
    }))

    try {
      const result = await dispatch({ type: 'agent.send', text: trimmed, sessionId })
      set({ sessionId: result.sessionId, sending: false })
    } catch (error) {
      set({ sending: false, error: `发送失败：${errorMessage(error)}` })
    }
  },

  ingestEvent(event) {
    switch (event.type) {
      case 'agent.delta': {
        const state = get()
        const id = state.streamId ?? nextId('agent')
        const messages =
          state.streamId === null
            ? [
                ...state.messages,
                { id, role: 'agent' as const, text: event.text, at: Date.now() },
              ]
            : state.messages.map((message) =>
                message.id === id ? { ...message, text: message.text + event.text } : message,
              )
        set({ streamId: id, messages })
        return
      }

      case 'agent.step':
        set({ step: event.step })
        return

      case 'agent.tool':
        set({ tool: event.call })
        return

      case 'approval.request':
        set({
          approval: {
            approvalId: event.approvalId,
            tool: event.tool,
            risk: event.risk,
            args: event.args,
            busy: false,
          },
        })
        return

      case 'agent.diff':
        set({ edit: { editId: event.editId, files: event.files, status: 'pending' } })
        return

      default:
        return
    }
  },

  async answerApproval(decision, always) {
    const approval = get().approval
    if (!approval || approval.busy) return

    set({ approval: { ...approval, busy: true }, error: null })
    try {
      await dispatch({
        type: 'agent.approve',
        approvalId: approval.approvalId,
        decision,
        always,
      })
      set({ approval: null })
    } catch (error) {
      // 保留卡片让用户重试：审批没送达时工具还卡在那里等。
      set({ approval: { ...approval, busy: false }, error: `审批失败：${errorMessage(error)}` })
    }
  },

  async acceptEdit(hooks) {
    const edit = get().edit
    if (!edit || edit.status !== 'pending') return

    // 落盘前先按**缓冲区**校验（用户有未保存改动时，hunk 必须以屏幕上的文本为准）。
    // precheck 只返回"仍然要接受的块"：对不上缓冲区的文件整份剔掉，其余照常落盘。
    let acceptHunks = allHunkIndices(edit.files)
    if (hooks?.precheck) {
      const planned = hooks.precheck({
        editId: edit.editId,
        files: edit.files,
        acceptHunks,
      })
      acceptHunks = planned.acceptHunks
    }

    if (acceptHunks.length === 0) {
      // 一块都没剩下（全部对不上缓冲区）：**不要**派发空 accept_hunks ——
      // 核心对空数组的语义是"丢弃这次编辑"，那样用户保存后连重试的机会都没有了。
      // 具体原因已经由 precheck 报进问题面板，这里只把卡片留在 pending。
      set({ error: '这次改动没有任何一块能安全落到当前缓冲区，见问题面板。' })
      return
    }

    try {
      await dispatch({
        type: 'edit.apply',
        editId: edit.editId,
        acceptHunks,
      })
      set({ edit: { ...edit, status: 'accepted' }, error: null })
    } catch (error) {
      // 核心拒绝（哈希过期 / 文件已被创建）时保留卡片：重新发起一次编辑即可。
      set({ error: `应用改动失败：${errorMessage(error)}` })
      return
    }

    // 落盘成功后才碰编辑器：一次应用 = 每个受影响的模型一个 undo 组
    // （具体分组在 `applyEditsAsOneUndoGroup`，本 store 不 import monaco）。
    if (hooks?.afterApply) {
      try {
        await hooks.afterApply({ editId: edit.editId, files: edit.files, acceptHunks })
      } catch (error) {
        set({ error: `改动已落盘，但同步到编辑器失败：${errorMessage(error)}` })
      }
    }
  },

  async rejectEdit() {
    const edit = get().edit
    if (!edit || edit.status !== 'pending') return

    try {
      await dispatch({ type: 'edit.reject', editId: edit.editId })
      set({ edit: { ...edit, status: 'rejected' } })
    } catch (error) {
      set({ error: `拒绝改动失败：${errorMessage(error)}` })
    }
  },

  dismissError() {
    set({ error: null })
  },
}))