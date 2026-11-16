// @vitest-environment jsdom
/**
 * 设备面板的界面契约（docs/09-device-capability.md §7 / AGENTS.md 的 UI 约定）。
 *
 * 这组测试守的是四条**安全相关**的显示规则，而不是像素：
 * 1. 空数组 `allowedApps` 必须写成"不允许任何应用"，绝不能写成"无限制"（空 = 全拒）。
 * 2. `device.request` 的 `reason` 是不可信文本：原样当文本渲染，注入的 `<b>` 必须被转义。
 * 3. 未知的 `state` / `sub` 原样显示，界面不能因为后端加了新值就崩。
 * 4. 撤销按钮是显眼的破坏性按钮（`button--danger` + 固定 `aria-label`），点它只调 store。
 */

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DeviceStatus } from '../../generated'
import { INITIAL_DEVICE_STATE, useDeviceStore } from '../../stores/device-store'
import { DevicePanel } from './device-panel'

const dispatchMock = vi.hoisted(() =>
  vi.fn(async (_command: unknown): Promise<unknown> => null),
)

vi.mock('../../lib/ipc', () => ({ dispatch: dispatchMock }))

/** 一个合法的 `DeviceStatus`（字段与 generated/DeviceStatus.ts 一致）。 */
function statusOf(overrides: Partial<DeviceStatus> = {}): DeviceStatus {
  return {
    botId: 'bot-1',
    sessionId: 'sess-1',
    state: 'active',
    sub: ['screen'],
    allowedApps: ['PathOfExile.exe'],
    grantedAt: 1_700_000_000_000,
    deadline: 1_700_000_300_000,
    remainingMs: 300_000,
    ...overrides,
  }
}

beforeEach(() => {
  useDeviceStore.setState({ ...INITIAL_DEVICE_STATE })
  dispatchMock.mockReset()
  dispatchMock.mockImplementation(async () => null)
})

afterEach(cleanup)

describe('DevicePanel', () => {
  it('① 没有选中 bot 时显示"先启动一个 bot"', () => {
    render(<DevicePanel />)

    expect(screen.getByText(/先启动一个 bot/)).toBeTruthy()
  })

  it('② active 时显示中文状态、倒计时与"立即撤销"按钮', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'active', remainingMs: 125_000 }),
    })

    render(<DevicePanel />)

    expect(screen.getByText('进行中')).toBeTruthy()
    expect(screen.getByText(/剩余 2:05/)).toBeTruthy()
    const revoke = screen.getByLabelText('立即撤销设备会话')
    expect(revoke.className).toContain('button--danger')
  })

  it('③ allowedApps 是空数组时显式写"不允许任何应用"，不写成无限制', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'active', allowedApps: [] }),
    })

    const { container } = render(<DevicePanel />)

    expect(container.textContent).toContain('不允许任何应用')
    expect(container.textContent).not.toContain('无限制')
  })

  it('④ pending 存在时显示理由，且理由里的 <b> 被当文本转义', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'none', sub: [], allowedApps: [] }),
      pending: {
        requestId: 'req-1',
        sub: ['screen'],
        reason: '<b>x</b> 请把游戏窗口里的确认框点掉',
      },
    })

    const { container } = render(<DevicePanel />)

    expect(container.textContent).toContain('<b>x</b>')
    // 没被解释成标签：没有任何 <b> 元素被插进 DOM。
    expect(container.querySelector('b')).toBeNull()
    expect(screen.getByText(/请把游戏窗口里的确认框点掉/)).toBeTruthy()
  })

  it('⑤ 未知的能力值原样显示且不崩（后端将来会加 audio）', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'active', sub: ['screen', 'audio'] }),
    })

    const { container } = render(<DevicePanel />)

    expect(container.textContent).toContain('audio')
    expect(container.textContent).toContain('未知能力（按原样显示）')
    // 认识的照旧给中文说明。
    expect(container.textContent).toContain('截屏')
  })

  it('⑥ 点「立即撤销」调用 store 的 revoke', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'active' }),
    })
    const revokeSpy = vi.spyOn(useDeviceStore.getState(), 'revoke').mockResolvedValue(undefined)

    render(<DevicePanel />)
    fireEvent.click(screen.getByLabelText('立即撤销设备会话'))

    expect(revokeSpy).toHaveBeenCalledTimes(1)
  })

  it('⑦ 点「开始会话」打开对话框：aria-modal，子能力默认一个都不勾', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'none', sub: [], allowedApps: [] }),
    })

    render(<DevicePanel />)
    fireEvent.click(screen.getByRole('button', { name: '开始会话' }))

    const dialog = screen.getByRole('dialog')
    expect(dialog.getAttribute('aria-modal')).toBe('true')
    expect(dialog.textContent).toContain('只读档仍然允许设备控制')
    const boxes = within(dialog).getAllByRole('checkbox') as HTMLInputElement[]
    expect(boxes).toHaveLength(5)
    expect(boxes.every((box) => !box.checked)).toBe(true)
  })

  it('⑧ 待处理请求点「允许」时，对话框预勾请求里带的子能力', () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      status: statusOf({ state: 'none', sub: [], allowedApps: [] }),
      pending: { requestId: 'req-1', sub: ['screen'], reason: '帮我截一张图' },
    })

    render(<DevicePanel />)
    fireEvent.click(screen.getByRole('button', { name: '允许…' }))

    const dialog = screen.getByRole('dialog')
    const boxes = within(dialog).getAllByRole('checkbox') as HTMLInputElement[]
    expect(boxes[0]?.checked).toBe(true) // screen
    expect(boxes[1]?.checked).toBe(false) // ocr
  })
})