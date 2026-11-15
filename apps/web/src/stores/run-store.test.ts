/**
 * runStore 的事件归约（无浏览器：`dispatch` 在 node 下走 `ipc-mock`）。
 *
 * 这条链路的要害是"运行态只有一个真源"：`bot.status` 事件推进状态，
 * `bot.list` 只在没收到过事件时播种一次。测试直接打 `ingestEvent`，
 * 不经过任何组件，所以能覆盖到 UI 看不见的边界（别 bot 的事件、error 留存、环形上界）。
 */

import { beforeEach, describe, expect, it } from 'vitest'
import type { CoreEvent } from '../lib/ipc'
import { MAX_LOG_LINES, useRunStore } from './run-store'

/** 造一条 `bot.status` 事件（默认是本 store 认的那个 bot）。 */
function botStatus(
  status: 'stopped' | 'starting' | 'running' | 'stopping' | 'degraded',
  error: string | null = null,
  botId = 'bot-1',
): CoreEvent {
  return { type: 'bot.status', botId, status, error }
}

/** 造一条 `run.log` 事件。 */
function runLog(
  line: string,
  level: 'trace' | 'debug' | 'info' | 'warn' | 'error' = 'info',
  botId = 'bot-1',
): CoreEvent {
  return { type: 'run.log', botId, level, line, at: 1_700_000_000_000 }
}

function ingest(event: CoreEvent): void {
  useRunStore.getState().ingestEvent(event)
}

beforeEach(() => {
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

describe('runStore.ingestEvent', () => {
  it('bot.status 推进运行态，并记住这是事件给的（live）', () => {
    ingest(botStatus('running'))

    const state = useRunStore.getState()
    expect(state.botId).toBe('bot-1')
    expect(state.status).toBe('running')
    expect(state.live).toBe(true)
  })

  it('bot.status 的 error 文本可见，且下一条无错误的事件不会把它清掉', () => {
    ingest(botStatus('stopped', '适配器连不上：ws 握手超时'))
    expect(useRunStore.getState().error).toBe('适配器连不上：ws 握手超时')

    // 状态事件是连续的：没有新错误时保留旧文本，等用户自己关。
    ingest(botStatus('stopped', null))
    expect(useRunStore.getState().error).toBe('适配器连不上：ws 握手超时')

    useRunStore.getState().dismissError()
    expect(useRunStore.getState().error).toBeNull()
  })

  it('别的 bot 的状态不影响当前 bot', () => {
    ingest(botStatus('running', null, 'bot-1'))
    ingest(botStatus('stopped', '另一个 bot 挂了', 'bot-2'))

    const state = useRunStore.getState()
    expect(state.botId).toBe('bot-1')
    expect(state.status).toBe('running')
    expect(state.error).toBeNull()
  })

  it('run.log 进日志缓冲，trace 折成 debug（面板只有四档）', () => {
    ingest(runLog('收到一条私聊', 'trace'))

    expect(useRunStore.getState().logs).toEqual([
      { level: 'debug', source: 'bot-1', message: '收到一条私聊', at: 1_700_000_000_000 },
    ])
  })

  it('别的 bot 的日志不进当前缓冲', () => {
    ingest(runLog('当前 bot 的日志'))
    ingest(runLog('别人的日志', 'info', 'bot-2'))

    expect(useRunStore.getState().logs.map((line) => line.message)).toEqual(['当前 bot 的日志'])
  })

  it('日志缓冲保持 10k 行上界：多了丢最旧的', () => {
    const full = Array.from({ length: MAX_LOG_LINES }, (_, index) => ({
      level: 'info' as const,
      source: 'bot-1',
      message: `旧 ${index}`,
      at: index,
    }))
    useRunStore.setState({ botId: 'bot-1', logs: full })

    ingest(runLog('新的一行'))

    const logs = useRunStore.getState().logs
    expect(logs).toHaveLength(MAX_LOG_LINES)
    expect(logs[0]?.message).toBe('旧 1')
    expect(logs[logs.length - 1]?.message).toBe('新的一行')
  })
})

describe('runStore.refresh', () => {
  it('用 bot_list 播种一次状态（没收到事件时以快照为准）', async () => {
    await useRunStore.getState().refresh()

    const state = useRunStore.getState()
    expect(state.hydrated).toBe(true)
    // mock 后端的固定 bot（真机上这一行来自 hbot-store 的 bot 表）。
    expect(state.botId).toBe('demo-bot')
    expect(state.status).toBe('stopped')
  })

  it('收到过事件之后，bot_list 不再覆盖状态', async () => {
    ingest(botStatus('running'))

    await useRunStore.getState().refresh()

    const state = useRunStore.getState()
    expect(state.botId).toBe('bot-1')
    expect(state.status).toBe('running')
  })
})