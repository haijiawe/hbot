/**
 * deviceStore：设备会话状态（docs/09-device-capability.md §7 状态机 / §8 契约）。
 *
 * 这一组测试盯的是三条最容易写错的东西：
 * 1. **事件是唯一真相**：撤销之后不许自己编一个 `revoked`（那会让界面显示"已撤销"而核心
 *    其实还在接受调用，反向也一样）。所以断言 `status` 仍是旧对象、直到事件到来。
 * 2. **开放字符串**：`state` / `sub` 是后端可扩展的字符串，未知值必须原样保留而不是抛错。
 * 3. **失败不污染状态**：`grant` 失败只写 `error`，不能把上一次读到的会话冲掉。
 */

import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { DeviceSettings, DeviceStatus } from '../generated'
import { INITIAL_DEVICE_STATE, useDeviceStore } from './device-store'

const dispatchMock = vi.hoisted(() =>
  vi.fn(async (_command: unknown): Promise<unknown> => null),
)

vi.mock('../lib/ipc', () => ({ dispatch: dispatchMock }))

/** 一个合法的 `DeviceStatus`（字段全部来自 generated/DeviceStatus.ts）。 */
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

/** 没有会话时的状态：`state: 'none'` 是正常返回值，不是 `null`。 */
function noneStatus(botId = 'bot-1'): DeviceStatus {
  return statusOf({
    botId,
    sessionId: null,
    state: 'none',
    sub: [],
    allowedApps: [],
    grantedAt: null,
    deadline: null,
    remainingMs: null,
  })
}

/** 一份宿主设置（字段来自 generated/DeviceSettings.ts）。 */
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

describe('deviceStore', () => {
  beforeEach(() => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE })
    dispatchMock.mockReset()
    dispatchMock.mockImplementation(async () => null)
  })

  it('① 收到 device.status 就用事件里的状态覆盖当前状态', () => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1' })
    const status = statusOf({ state: 'active' })

    useDeviceStore.getState().ingestEvent({ type: 'device.status', status })

    expect(useDeviceStore.getState().status).toEqual(status)
  })

  it('② 收到 device.request 只设 pending，不动 status', () => {
    const status = statusOf({ state: 'expired' })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', status })

    useDeviceStore.getState().ingestEvent({
      type: 'device.request',
      botId: 'bot-1',
      sub: ['screen', 'audio'],
      reason: '帮我点一下确认按钮',
      requestId: 'req-1',
    })

    const state = useDeviceStore.getState()
    expect(state.pending).toEqual({
      requestId: 'req-1',
      sub: ['screen', 'audio'],
      reason: '帮我点一下确认按钮',
    })
    // 同一个对象（不是"复制一份再改"）：请求不该把会话状态换掉。
    expect(state.status).toBe(status)
  })

  it('③ grant 失败时 error 有值，status 保持上一次读到的会话', async () => {
    const status = noneStatus()
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', status })
    dispatchMock.mockRejectedValueOnce(new Error('device_grant: allowedApps 不能为空（-32002）'))

    const ok = await useDeviceStore
      .getState()
      .grant({ sub: ['screen'], allowedApps: [], minutes: 5 })

    const state = useDeviceStore.getState()
    expect(ok).toBe(false)
    expect(state.error).toContain('allowedApps 不能为空')
    expect(state.status).toBe(status)
    expect(state.busy).toBe(false)
  })

  it('④ revoke 之后不自己伪造状态：status 仍是之前那个，直到 device.status 事件到来', async () => {
    const status = statusOf({ state: 'active' })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', status })

    await useDeviceStore.getState().revoke()

    expect(dispatchMock).toHaveBeenLastCalledWith({ type: 'deviceRevoke', botId: 'bot-1' })
    const state = useDeviceStore.getState()
    expect(state.status).toBe(status)
    expect(state.status?.state).toBe('active')
    expect(state.error).toBeNull()

    // 事件到了才变 —— 这才是唯一真相。
    const revoked = statusOf({ state: 'revoked', remainingMs: null })
    useDeviceStore.getState().ingestEvent({ type: 'device.status', status: revoked })
    expect(useDeviceStore.getState().status?.state).toBe('revoked')
  })

  it('⑤ 未知的 state 字符串不抛错，原样保留在 status.state', () => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1' })

    expect(() =>
      useDeviceStore
        .getState()
        .ingestEvent({ type: 'device.status', status: statusOf({ state: 'quarantined' }) }),
    ).not.toThrow()

    expect(useDeviceStore.getState().status?.state).toBe('quarantined')
  })

  it('⑥ refresh 期间 busy=true，结束后回到 false 并写入返回的状态', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    dispatchMock.mockImplementationOnce(async () => {
      await gate
      return statusOf({ state: 'active' })
    })

    const task = useDeviceStore.getState().refresh('bot-1')
    expect(useDeviceStore.getState().busy).toBe(true)

    release()
    await task

    const state = useDeviceStore.getState()
    expect(state.busy).toBe(false)
    expect(state.status?.state).toBe('active')
    expect(dispatchMock).toHaveBeenLastCalledWith({ type: 'deviceStatus', botId: 'bot-1' })
  })

  it('⑦ grant 成功时用返回值覆盖 status，并清掉待处理请求', async () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      botId: 'bot-1',
      pending: { requestId: 'req-1', sub: ['screen'], reason: 'x' },
    })
    const granted = statusOf({ state: 'active', sub: ['screen', 'input'] })
    dispatchMock.mockResolvedValueOnce(granted)

    const ok = await useDeviceStore
      .getState()
      .grant({ sub: ['screen', 'input'], allowedApps: ['PathOfExile.exe'], minutes: 5 })

    expect(ok).toBe(true)
    expect(useDeviceStore.getState().status).toEqual(granted)
    expect(useDeviceStore.getState().pending).toBeNull()
    expect(dispatchMock).toHaveBeenLastCalledWith({
      type: 'deviceGrant',
      botId: 'bot-1',
      sub: ['screen', 'input'],
      allowedApps: ['PathOfExile.exe'],
      minutes: 5,
    })
  })

  it('⑧ 别的 bot 的 device.status 不会覆盖当前 bot 的状态', () => {
    const mine = statusOf({ botId: 'bot-1', state: 'active' })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', status: mine })

    useDeviceStore
      .getState()
      .ingestEvent({ type: 'device.status', status: statusOf({ botId: 'bot-2', state: 'revoked' }) })

    expect(useDeviceStore.getState().status).toBe(mine)
    expect(useDeviceStore.getState().botId).toBe('bot-1')
  })

  it('⑨ 没有 bot 时 refresh 不发命令（空状态不是错误）', async () => {
    await useDeviceStore.getState().refresh('')

    expect(dispatchMock).not.toHaveBeenCalled()
    expect(useDeviceStore.getState().status).toBeNull()
    expect(useDeviceStore.getState().error).toBeNull()
  })

  it('⑩ 换 bot 会清掉上一个 bot 的会话与待处理请求', () => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', status: statusOf() })

    useDeviceStore.getState().setBotId('bot-2')

    const state = useDeviceStore.getState()
    expect(state.botId).toBe('bot-2')
    expect(state.status).toBeNull()
    expect(state.pending).toBeNull()
  })

  it('⑪ loadSettings 读到设置就写进 state，并清掉上一次的错误', async () => {
    useDeviceStore.setState({
      ...INITIAL_DEVICE_STATE,
      settingsBusy: false,
      settingsError: '上一次失败',
    })
    dispatchMock.mockImplementation(async () => settingsOf({ enabled: true, capabilities: ['ocr'] }))

    await useDeviceStore.getState().loadSettings()

    expect(dispatchMock).toHaveBeenLastCalledWith({ type: 'deviceSettingsGet' })
    const state = useDeviceStore.getState()
    expect(state.settings).toEqual(settingsOf({ enabled: true, capabilities: ['ocr'] }))
    expect(state.settingsBusy).toBe(false)
    expect(state.settingsError).toBeNull()
  })

  it('⑫ loadSettings 失败只写 settingsError（带后端原文），上一份设置留着', async () => {
    const previous = settingsOf({ capabilities: ['screen'] })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, settings: previous })
    dispatchMock.mockImplementation(async () => {
      throw new Error('device_settings_get: 读取失败（-32603）')
    })

    await useDeviceStore.getState().loadSettings()

    const state = useDeviceStore.getState()
    // 清掉只会让"设置存在但这次没读到"和"这台机器没有设置"分不出来。
    expect(state.settings).toBe(previous)
    expect(state.settingsError).toContain('device_settings_get: 读取失败（-32603）')
    expect(state.settingsBusy).toBe(false)
  })

  it('⑬ saveSettings 用**后端生效值**覆盖本地，而不是信自己传进去的那份', async () => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, settings: settingsOf() })
    const effective = settingsOf({ enabled: true, capabilities: ['screen'] })
    dispatchMock.mockImplementation(async () => effective)

    const next = settingsOf({ enabled: true, capabilities: ['screen', 'audio'] })
    const ok = await useDeviceStore.getState().saveSettings(next)

    expect(ok).toBe(true)
    expect(dispatchMock).toHaveBeenLastCalledWith({ type: 'deviceSettingsSet', settings: next })
    // `audio` 被核心丢掉了：界面显示的是核心那份，不是用户刚提交的那份。
    expect(useDeviceStore.getState().settings).toBe(effective)
  })

  it('⑭ saveSettings 失败返回 false、原文写进 settingsError、本地那份不动', async () => {
    const previous = settingsOf({ enabled: false })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, settings: previous })
    dispatchMock.mockImplementation(async () => {
      throw new Error('device_settings_set: maxMinutes 必须在 1..30 之间（收到 999，-32002）')
    })

    const ok = await useDeviceStore
      .getState()
      .saveSettings(settingsOf({ maxMinutes: 999, defaultMinutes: 5 }))

    expect(ok).toBe(false)
    const state = useDeviceStore.getState()
    expect(state.settings).toBe(previous)
    expect(state.settingsError).toContain('maxMinutes 必须在 1..30 之间（收到 999，-32002）')
  })

  it('⑮ 换 bot 不会清掉宿主设置（那是这台机器的，不是某个 bot 的）', () => {
    const settings = settingsOf({ enabled: true })
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, botId: 'bot-1', settings })

    useDeviceStore.getState().setBotId('bot-2')

    expect(useDeviceStore.getState().settings).toBe(settings)
  })

  it('⑯ 收到 device.settings 事件直接覆盖本地设置（别的窗口改的）', () => {
    useDeviceStore.setState({ ...INITIAL_DEVICE_STATE, settings: settingsOf() })
    const next = settingsOf({ enabled: true, maxMinutes: 15 })

    useDeviceStore.getState().ingestEvent({ type: 'device.settings', settings: next })

    expect(useDeviceStore.getState().settings).toBe(next)
  })
})