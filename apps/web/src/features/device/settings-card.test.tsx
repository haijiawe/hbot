// @vitest-environment jsdom

/**
 * 宿主设备设置卡片的组件测试（挂在 `DevicePanel` 里，所以这里渲染整个面板）。
 *
 * 每条用例只钉一件事：读一次、发给核心的载荷、两条本地拦截、事件刷新。
 * 断言沿用设备面板既有写法（不用 jest-dom matcher）：`toBeTruthy()` / `.toBe()`。
 */

import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DeviceSettings } from '../../generated'
import { INITIAL_DEVICE_STATE, useDeviceStore } from '../../stores/device-store'
import { DevicePanel } from './device-panel'

const dispatchMock = vi.hoisted(() => vi.fn(async (_command: unknown): Promise<unknown> => null))

vi.mock('../../lib/ipc', () => ({ dispatch: dispatchMock }))

const BOT_ID = 'bot-1'

function settingsOf(overrides: Partial<DeviceSettings> = {}): DeviceSettings {
  return {
    enabled: false,
    capabilities: [],
    allowedApps: [],
    defaultMinutes: 5,
    maxMinutes: 30,
    maxActionsPerSession: 10_000,
    ...overrides,
  }
}

function commandType(command: unknown): string {
  return (command as { type: string }).type
}

/**
 * 让 mock 变成一块"内存磁盘"：get 回当前那份，set 原样覆盖并回声。
 * 故意不在这里重复核心的校验 —— 前端的拦截要自己站得住，不能靠 mock 兜。
 */
function serve(initial: DeviceSettings): { stored: DeviceSettings } {
  const box = { stored: initial }
  dispatchMock.mockImplementation(async (command) => {
    const type = commandType(command)
    if (type === 'deviceStatus') return null
    if (type === 'deviceSettingsGet') return box.stored
    if (type === 'deviceSettingsSet') {
      box.stored = (command as { settings: DeviceSettings }).settings
      return box.stored
    }
    return null
  })
  return box
}

/** 最后一次 `device_settings_set` 带的载荷；没发过就是 `null`（测试 ③④ 靠它断言"不发命令"）。 */
function lastSetPayload(): DeviceSettings | null {
  const calls = dispatchMock.mock.calls.filter((call) => commandType(call[0]) === 'deviceSettingsSet')
  const last = calls.at(-1)
  return last === undefined ? null : (last[0] as { settings: DeviceSettings }).settings
}

function checkedOf(label: string): boolean {
  return (screen.getByLabelText(label) as HTMLInputElement).checked
}

function valueOf(label: string): string {
  return (screen.getByLabelText(label) as HTMLInputElement).value
}

beforeEach(() => {
  useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: BOT_ID })
  dispatchMock.mockReset()
  dispatchMock.mockImplementation(async () => null)
})

afterEach(cleanup)

describe('宿主设备设置卡片', () => {
  it('打开面板就读一次设置，并把核心返回的值填进表单', async () => {
    serve(
      settingsOf({
        enabled: true,
        capabilities: ['screen'],
        allowedApps: ['PathOfExile.exe'],
        defaultMinutes: 7,
        maxMinutes: 20,
        maxActionsPerSession: 500,
      }),
    )
    render(<DevicePanel />)

    expect(
      dispatchMock.mock.calls.some((call) => commandType(call[0]) === 'deviceSettingsGet'),
    ).toBe(true)

    await screen.findByLabelText('允许设备控制（总开关）')
    expect(checkedOf('允许设备控制（总开关）')).toBe(true)
    expect(checkedOf('截屏')).toBe(true)
    expect(checkedOf('读屏幕文字')).toBe(false)
    expect((screen.getByLabelText('允许的应用（每行一个，逗号分隔也行）') as HTMLTextAreaElement).value).toBe(
      'PathOfExile.exe',
    )
    expect(valueOf('默认时长（分钟）')).toBe('7')
    expect(valueOf('会话时长上限（分钟）')).toBe('20')
    expect(valueOf('每次会话动作上限')).toBe('500')
  })

  it('关掉总开关旁边写着"所有设备调用都会被拒"，应用列表的空值写着"不设上限"', async () => {
    serve(settingsOf())
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    expect(
      screen.getByText(/关掉之后 Agent 与 bot 代码的所有设备调用都会被拒/),
    ).toBeTruthy()
    // 能力"空 = 全拒"与应用"空 = 不设上限"是**故意相反**的语义，两句都要在界面上。
    expect(screen.getByText(/不设上限/)).toBeTruthy()
    expect(screen.getByText(/全拒/)).toBeTruthy()
  })

  it('勾两个能力后保存，`device_settings_set` 收到的 capabilities 就是这两个', async () => {
    serve(settingsOf())
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    fireEvent.click(screen.getByLabelText('读屏幕文字'))
    fireEvent.click(screen.getByLabelText('点击与输入'))
    fireEvent.click(screen.getByText('保存设置'))

    await waitFor(() => {
      expect(lastSetPayload() === null).toBe(false)
    })
    const payload = lastSetPayload()
    expect(payload === null ? [] : [...payload.capabilities].sort()).toEqual(['input', 'ocr'])
    // 其余字段按表单原样带过去：set 是**全量覆盖**，漏字段会被核心当成默认值。
    expect(payload === null ? null : payload.maxMinutes).toBe(30)
    expect(payload === null ? null : payload.defaultMinutes).toBe(5)
    expect(payload === null ? null : payload.maxActionsPerSession).toBe(10_000)
  })

  it('`maxMinutes=31` 被前端拦住：给出中文原因，且不发命令', async () => {
    serve(settingsOf())
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    fireEvent.change(screen.getByLabelText('会话时长上限（分钟）'), { target: { value: '31' } })
    fireEvent.click(screen.getByText('保存设置'))

    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('30')
    expect(lastSetPayload()).toBeNull()
  })

  it('`defaultMinutes > maxMinutes` 被拦住：原因是"默认时长不能超过上限"，且不发命令', async () => {
    serve(settingsOf())
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    fireEvent.change(screen.getByLabelText('会话时长上限（分钟）'), { target: { value: '10' } })
    fireEvent.change(screen.getByLabelText('默认时长（分钟）'), { target: { value: '20' } })
    fireEvent.click(screen.getByText('保存设置'))

    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('默认时长')
    expect(alert.textContent).toContain('10')
    expect(lastSetPayload()).toBeNull()
  })

  it('收到 `device.settings` 事件后表单跟着更新（别的窗口改了设置）', async () => {
    serve(settingsOf())
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    act(() => {
      useDeviceStore.getState().ingestEvent({
        type: 'device.settings',
        settings: settingsOf({
          enabled: true,
          capabilities: ['clipboard'],
          maxMinutes: 15,
          defaultMinutes: 10,
        }),
      })
    })

    await waitFor(() => {
      expect(checkedOf('剪贴板')).toBe(true)
    })
    expect(checkedOf('允许设备控制（总开关）')).toBe(true)
    expect(valueOf('会话时长上限（分钟）')).toBe('15')
    expect(valueOf('默认时长（分钟）')).toBe('10')
  })

  it('后端拒绝保存时把 `message` 原文显示出来（不吞成"操作失败"）', async () => {
    serve(settingsOf())
    const original = dispatchMock.getMockImplementation()
    dispatchMock.mockImplementation(async (command) => {
      if (commandType(command) === 'deviceSettingsSet') {
        throw new Error('device_settings_set: 不认识的能力（audio，-32002）')
      }
      return original === undefined ? null : original(command)
    })
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    fireEvent.click(screen.getByText('保存设置'))

    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('不认识的能力（audio，-32002）')
  })

  it('不认识的能力按原样显示、提交时原样带回（前端不替核心判合法）', async () => {
    serve(settingsOf({ capabilities: ['screen', 'audio'] }))
    render(<DevicePanel />)
    await screen.findByLabelText('允许设备控制（总开关）')

    expect(checkedOf('audio')).toBe(true)
    fireEvent.click(screen.getByText('保存设置'))

    await waitFor(() => {
      expect(lastSetPayload() === null).toBe(false)
    })
    const payload = lastSetPayload()
    expect(payload === null ? [] : [...payload.capabilities].sort()).toEqual(['audio', 'screen'])
    expect(screen.queryByRole('alert')).toBeNull()
  })
})