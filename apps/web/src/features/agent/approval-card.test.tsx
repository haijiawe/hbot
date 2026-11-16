// @vitest-environment jsdom
/**
 * 审批卡片（`features/agent/approval-card.tsx`）。
 *
 * 断言的是**下发到核心的命令形状**，不是"回调被调用过"：审批的 payload 是
 * `agent.approve { approvalId, decision, always }`，任何一个字段错位都会让核心
 * 对**另一个审批**或**另一个动作**放行（docs/06-ide.md §5.1）。
 *
 * 「always」是复选框而不是第三个按钮 —— 它必须把 `always: true` 带进同一条
 * `approve` / `deny` 命令里，所以三种"动作"在这里是三组 payload：
 * 批准（always=false）、拒绝（always=false）、带放行的批准/拒绝（always=true）。
 */

import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { CoreEvent } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useAgentStore } from '../../stores/agent-store'
import { AgentPanel } from './agent-panel'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  return { ...actual, dispatch: vi.fn(async () => null) }
})

const APPROVE = { type: 'agent.approve' } as const

function approvalRequest(
  approvalId: string,
  tool: string,
  args: unknown = { path: 'src/a.ts', content: 'hello' },
): CoreEvent {
  return { type: 'approval.request', approvalId, tool, args, risk: 'high' }
}

function ingest(event: CoreEvent): void {
  // 事件由核心推来（不在 React 的事件回调里），必须包一层 act 才会同步重渲染。
  act(() => useAgentStore.getState().ingestEvent(event))
}

/** 只取 `agent.approve` 的命令体（`type` 之外的字段就是契约的全部）。 */
function approveCommands(): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === APPROVE.type)
    .map(({ type: _type, ...rest }) => rest)
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
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('ApprovalCard', () => {
  it('批准：下发 approve + always=false，带的是这条审批的 approvalId', async () => {
    const user = userEvent.setup()
    render(<AgentPanel />)
    ingest(approvalRequest('approval-1', 'write_file'))

    expect(screen.getByLabelText('审批请求：write_file')).toBeTruthy()
    await user.click(screen.getByLabelText('批准 write_file'))

    await waitFor(() => expect(approveCommands()).toEqual([
      { approvalId: 'approval-1', decision: 'approve', always: false },
    ]))
    // 成功之后卡片必须消失，否则用户会怀疑"到底批没批"。
    await waitFor(() => expect(screen.queryByLabelText('审批请求：write_file')).toBeNull())
  })

  it('拒绝：下发 deny + always=false', async () => {
    const user = userEvent.setup()
    render(<AgentPanel />)
    ingest(approvalRequest('approval-2', 'run_shell'))

    await user.click(screen.getByLabelText('拒绝 run_shell'))

    await waitFor(() => expect(approveCommands()).toEqual([
      { approvalId: 'approval-2', decision: 'deny', always: false },
    ]))
  })

  it('勾「以后都按这次的动作放行」后批准：同一条命令带 always=true', async () => {
    const user = userEvent.setup()
    render(<AgentPanel />)
    ingest(approvalRequest('approval-3', 'write_file'))

    await user.click(screen.getByRole('checkbox', { name: '以后都按这次的动作放行' }))
    await user.click(screen.getByLabelText('批准 write_file'))

    await waitFor(() => expect(approveCommands()).toEqual([
      { approvalId: 'approval-3', decision: 'approve', always: true },
    ]))
  })

  it('勾了放行再拒绝：拒绝也带 always=true（放行的是"这次动作"，不是"这次批准"）', async () => {
    const user = userEvent.setup()
    render(<AgentPanel />)
    ingest(approvalRequest('approval-4', 'write_file'))

    await user.click(screen.getByRole('checkbox', { name: '以后都按这次的动作放行' }))
    await user.click(screen.getByLabelText('拒绝 write_file'))

    await waitFor(() => expect(approveCommands()).toEqual([
      { approvalId: 'approval-4', decision: 'deny', always: true },
    ]))
  })

  it('换一条审批时勾选态归零：不会把上一条的 always 带给另一个工具', async () => {
    const user = userEvent.setup()
    render(<AgentPanel />)

    ingest(approvalRequest('approval-5', 'write_file'))
    await user.click(screen.getByRole('checkbox', { name: '以后都按这次的动作放行' }))

    // 同一轮里核心又拦下另一个工具：卡片按 approvalId 换成新的一条。
    ingest(approvalRequest('approval-6', 'run_shell'))

    const checkbox = screen.getByRole('checkbox', { name: '以后都按这次的动作放行' })
    expect((checkbox as HTMLInputElement).checked).toBe(false)

    await user.click(screen.getByLabelText('批准 run_shell'))
    await waitFor(() => expect(approveCommands()).toEqual([
      { approvalId: 'approval-6', decision: 'approve', always: false },
    ]))
  })

  it('busy 时两个决策按钮与复选框全部禁用（重复点击不能重复下发）', () => {
    render(<AgentPanel />)
    ingest(approvalRequest('approval-7', 'write_file'))
    act(() =>
      useAgentStore.setState((state) => ({
        approval: state.approval === null ? null : { ...state.approval, busy: true },
      })),
    )

    expect((screen.getByLabelText('批准 write_file') as HTMLButtonElement).disabled).toBe(true)
    expect((screen.getByLabelText('拒绝 write_file') as HTMLButtonElement).disabled).toBe(true)
    expect(
      (screen.getByRole('checkbox', { name: '以后都按这次的动作放行' }) as HTMLInputElement).disabled,
    ).toBe(true)
  })

  it('参数按 JSON 展示；序列化失败也要说清楚是哪一步没渲染出来', () => {
    const circular: Record<string, unknown> = { path: 'src/a.ts' }
    circular.self = circular

    render(<AgentPanel />)
    ingest(approvalRequest('approval-8', 'write_file', circular))

    expect(screen.getByLabelText('工具参数').textContent).toContain('参数无法序列化')
  })

  it('风险等级与工具名直接可见（用户不用点开参数才知道自己在批什么）', () => {
    render(<AgentPanel />)
    ingest(approvalRequest('approval-9', 'write_file'))

    expect(screen.getByText('高风险')).toBeTruthy()
    expect(screen.getByLabelText('工具参数').textContent).toContain('"path": "src/a.ts"')
  })
})