/**
 * agentStore 的事件归约 + 用户动作（无浏览器：`dispatch` 在 node 下走 `ipc-mock`）。
 *
 * 覆盖两件最容易回归的事：
 * - 流式输出必须拼在**同一条**消息上（每次 delta 新建一条 = 屏幕上刷屏）；
 * - Diff 与审批必须来自核心事件、答复必须真的下发命令（mock 返回 `null` 即视为接受）。
 */

import { beforeEach, describe, expect, it } from 'vitest'
import type { EditFile } from '../generated'
import type { CoreEvent } from '../lib/ipc'
import { editStats, useAgentStore } from './agent-store'

/** 一次跨文件的编辑：1 块删除 1 行、加 2 行。 */
const FILES: EditFile[] = [
  {
    path: 'src/a.ts',
    hunks: [
      {
        index: 0,
        oldStart: 1,
        oldLines: 1,
        newStart: 1,
        newLines: 2,
        lines: ['-旧的一行', '+新的一行', '+又一行'],
      },
    ],
  },
]

function delta(text: string): CoreEvent {
  return { type: 'agent.delta', sessionId: 'session-1', text }
}

function ingest(event: CoreEvent): void {
  useAgentStore.getState().ingestEvent(event)
}

beforeEach(() => {
  useAgentStore.setState({
    sessionId: null,
    messages: [],
    draft: '',
    sending: false,
    step: 0,
    tool: null,
    edit: null,
    approval: null,
    error: null,
    streamId: null,
  })
})

describe('agentStore.ingestEvent', () => {
  it('同一轮的两次 agent.delta 拼在一条消息上', () => {
    ingest(delta('我看了下 AGENTS.md，'))
    ingest(delta('先改 ipc.ts。'))

    const messages = useAgentStore.getState().messages
    expect(messages).toHaveLength(1)
    expect(messages[0]?.role).toBe('agent')
    expect(messages[0]?.text).toBe('我看了下 AGENTS.md，先改 ipc.ts。')
  })

  it('agent.diff 落成待审阅编辑，增删行数可从块里数出来', () => {
    ingest({ type: 'agent.diff', editId: 'edit-1', files: FILES })

    const edit = useAgentStore.getState().edit
    expect(edit?.editId).toBe('edit-1')
    expect(edit?.status).toBe('pending')
    expect(edit?.files).toEqual(FILES)
    expect(editStats(edit?.files ?? [])).toEqual({ added: 2, removed: 1 })
  })

  it('agent.step / agent.tool 反映最新进度', () => {
    ingest({ type: 'agent.step', sessionId: 'session-1', step: 3 })
    expect(useAgentStore.getState().step).toBe(3)

    const call = {
      id: 'call-1',
      sessionId: 'session-1',
      name: 'read_file',
      args: { path: 'AGENTS.md' },
      result: null,
      status: 'running',
      approval: null,
      createdAt: 1_700_000_000_000,
      finishedAt: null,
    } as const
    ingest({ type: 'agent.tool', sessionId: 'session-1', call })

    expect(useAgentStore.getState().tool?.name).toBe('read_file')
  })

  it('approval.request 落成审批卡片，未知事件被忽略', () => {
    ingest({
      type: 'approval.request',
      approvalId: 'approval-1',
      tool: 'write_file',
      args: { path: 'src/a.ts' },
      risk: 'medium',
    })
    ingest({ type: 'project.changed', path: 'src/a.ts', kind: 'updated' })

    const approval = useAgentStore.getState().approval
    expect(approval?.approvalId).toBe('approval-1')
    expect(approval?.tool).toBe('write_file')
    expect(approval?.risk).toBe('medium')
    expect(approval?.busy).toBe(false)
  })
})

describe('agentStore 用户动作', () => {
  it('答复审批后卡片消失（命令经 mock 下发成功）', async () => {
    ingest({
      type: 'approval.request',
      approvalId: 'approval-1',
      tool: 'write_file',
      args: {},
      risk: 'high',
    })

    await useAgentStore.getState().answerApproval('approve', true)

    expect(useAgentStore.getState().approval).toBeNull()
    expect(useAgentStore.getState().error).toBeNull()
  })

  it('接受编辑后状态变 accepted；拒绝后变 rejected', async () => {
    ingest({ type: 'agent.diff', editId: 'edit-1', files: FILES })

    await useAgentStore.getState().acceptEdit()
    expect(useAgentStore.getState().edit?.status).toBe('accepted')

    // 同一张卡片不接受第二次点击（状态已不是 pending）。
    await useAgentStore.getState().acceptEdit()
    expect(useAgentStore.getState().edit?.status).toBe('accepted')

    ingest({ type: 'agent.diff', editId: 'edit-2', files: FILES })
    await useAgentStore.getState().rejectEdit()
    expect(useAgentStore.getState().edit?.status).toBe('rejected')
  })

  it('首条消息带 sessionId: null，并保存核心返回的会话 ID 续用', async () => {
    await useAgentStore.getState().send('看看 AGENTS.md')

    const state = useAgentStore.getState()
    expect(state.sessionId).toBe('mock-session')
    expect(state.sending).toBe(false)
    expect(state.messages[0]?.role).toBe('user')
    expect(state.messages[0]?.text).toBe('看看 AGENTS.md')
  })
})