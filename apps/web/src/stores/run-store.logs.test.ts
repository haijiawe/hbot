/**
 * `runStore.loadLogs`：日志面板的首屏补读。
 *
 * 这里 mock `dispatch` 而不是走 `ipc-mock`，因为要断言"没有选中 bot 时命令里
 * `botId` 是 `null`"——mock 后端的固定 bot 会掩盖这一点。
 */

import { beforeEach, describe, expect, it, vi } from 'vitest'

const { dispatchMock } = vi.hoisted(() => ({ dispatchMock: vi.fn() }))

vi.mock('../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/ipc')>()
  return { ...actual, dispatch: dispatchMock }
})

import { LOG_TAIL_LIMIT, useRunStore } from './run-store'

beforeEach(() => {
  dispatchMock.mockReset()
  useRunStore.setState({
    botId: '',
    status: 'stopped',
    logs: [],
    busy: false,
    error: null,
    hydrated: false,
    live: false,
  })
})

describe('runStore.loadLogs', () => {
  it('没有选中 bot 时传 null，并把 Agent 行标成 [agent]', async () => {
    dispatchMock.mockResolvedValue([
      {
        at: 1_700_000_000_000,
        level: 'info',
        line: '[session 3f2a…] agent 工具 fs_read 成功（1 ms）',
        source: 'agent',
      },
    ])

    await useRunStore.getState().loadLogs()

    expect(dispatchMock).toHaveBeenCalledWith({
      type: 'logs.tail',
      botId: null,
      limit: LOG_TAIL_LIMIT,
    })
    expect(useRunStore.getState().logs).toEqual([
      {
        level: 'info',
        source: '[agent]',
        message: '[session 3f2a…] agent 工具 fs_read 成功（1 ms）',
        at: 1_700_000_000_000,
      },
    ])
  })

  it('选中 bot 时按 bot 过滤，bot 行的 source 显示 bot id', async () => {
    useRunStore.setState({ botId: 'bot-1' })
    dispatchMock.mockResolvedValue([
      { at: 1_700_000_000_001, level: 'warn', line: '断线重连中', source: 'bot' },
    ])

    await useRunStore.getState().loadLogs()

    expect(dispatchMock).toHaveBeenCalledWith({
      type: 'logs.tail',
      botId: 'bot-1',
      limit: LOG_TAIL_LIMIT,
    })
    expect(useRunStore.getState().logs[0]?.source).toBe('bot-1')
  })

  it('读取失败写进 error，不清空已经显示的日志', async () => {
    useRunStore.setState({
      logs: [{ level: 'info', source: 'bot-1', message: '已有的行', at: 1 }],
    })
    dispatchMock.mockRejectedValue(new Error('db locked'))

    await useRunStore.getState().loadLogs()

    const state = useRunStore.getState()
    expect(state.error).toContain('读取日志失败')
    expect(state.logs.map((line) => line.message)).toEqual(['已有的行'])
  })
})
