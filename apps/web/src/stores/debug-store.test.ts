import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { CoreEvent } from '../lib/ipc'
import { dispatch } from '../lib/ipc'
import { BREAKPOINT_FLUSH_MS, useDebugStore } from './debug-store'

// 拦截 ipc：断点同步 / 继续 / 单步都在这里被断言（不碰 Tauri、不碰 mock 运行时）。
vi.mock('../lib/ipc', () => ({ dispatch: vi.fn(async () => null) }))

/** 一个两帧的暂停事件（契约：callStack 外层在前，当前帧是最后一项）。 */
function pausedEvent(botId = 'bot-1'): CoreEvent {
  return {
    type: 'debug.paused',
    botId,
    file: 'src/main.ts',
    line: 12,
    scopes: [{ name: '局部变量', variables: { text: 'hi', count: 2 } }],
    callStack: [
      { name: 'onMessage', file: 'src/main.ts', line: 12, column: 3 },
      { name: 'handleMessage', file: 'src/lib/router.ts', line: 40, column: 1 },
    ],
  }
}

const INITIAL_STATE = {
  botId: '',
  breakpoints: {},
  paused: null,
  selectedFrame: 0,
  busy: false,
  error: null,
}

/** 走完 120ms 防抖窗口，并把 flush 里的 await 链推进到结束。 */
async function flushDebounce(): Promise<void> {
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  for (let round = 0; round < 5; round += 1) await Promise.resolve()
}

const sentCommands = () => vi.mocked(dispatch).mock.calls.map(([command]) => command)

beforeEach(async () => {
  vi.useFakeTimers()
  // 先排掉上一个用例可能残留的防抖窗口，再重置状态，免得旧定时器污染断言。
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  useDebugStore.setState({ ...INITIAL_STATE })
  vi.mocked(dispatch).mockClear()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('debugStore 断点同步', () => {
  it('同一文件连点几行只发一条 debug_set_breakpoints，lines 是合并后的全量行号', async () => {
    useDebugStore.setState({ botId: 'bot-1' })
    const { toggleBreakpoint } = useDebugStore.getState()

    toggleBreakpoint('src/main.ts', 3)
    toggleBreakpoint('src/main.ts', 1)
    toggleBreakpoint('src/main.ts', 2)

    // 防抖窗口内一条都不发。
    expect(dispatch).not.toHaveBeenCalled()
    await flushDebounce()

    expect(dispatch).toHaveBeenCalledTimes(1)
    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/main.ts', lines: [1, 2, 3] },
    ])
  })

  it('再点一次已存在的行等于移除，lines 按全量覆盖语义变短', async () => {
    useDebugStore.setState({ botId: 'bot-1' })
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 5)
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 9)
    await flushDebounce()
    vi.mocked(dispatch).mockClear()

    useDebugStore.getState().toggleBreakpoint('src/main.ts', 5)
    await flushDebounce()

    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/main.ts', lines: [9] },
    ])
  })

  it('两个文件各改一次 → 两条命令，各带自己的全量行号', async () => {
    useDebugStore.setState({ botId: 'bot-1' })
    const { toggleBreakpoint } = useDebugStore.getState()

    toggleBreakpoint('src/a.ts', 2)
    toggleBreakpoint('src/b.ts', 7)
    toggleBreakpoint('src/a.ts', 4)
    await flushDebounce()

    expect(dispatch).toHaveBeenCalledTimes(2)
    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/a.ts', lines: [2, 4] },
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/b.ts', lines: [7] },
    ])
  })

  it('没有 botId 时不发命令，只留一条提示', async () => {
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 1)
    await flushDebounce()

    expect(dispatch).not.toHaveBeenCalled()
    expect(useDebugStore.getState().error).toContain('bot')
    // 断点本身仍然记在界面上，启动 bot 后会重新同步。
    expect(useDebugStore.getState().breakpoints).toEqual({ 'src/main.ts': [1] })
  })
})

describe('debugStore 暂停现场', () => {
  it('收到 debug.paused 后写入现场，并选中当前帧（callStack 最后一项）', () => {
    useDebugStore.getState().ingestEvent(pausedEvent())

    const state = useDebugStore.getState()
    expect(state.botId).toBe('bot-1')
    expect(state.paused).toEqual({
      file: 'src/main.ts',
      line: 12,
      scopes: [{ name: '局部变量', variables: { text: 'hi', count: 2 } }],
      callStack: [
        { name: 'onMessage', file: 'src/main.ts', line: 12, column: 3 },
        { name: 'handleMessage', file: 'src/lib/router.ts', line: 40, column: 1 },
      ],
    })
    expect(state.selectedFrame).toBe(1)
  })

  it('selectFrame 选中指定帧，越界 / 非法值夹到有效下标', () => {
    useDebugStore.getState().ingestEvent(pausedEvent())
    const { selectFrame } = useDebugStore.getState()

    selectFrame(0)
    expect(useDebugStore.getState().selectedFrame).toBe(0)
    selectFrame(99)
    expect(useDebugStore.getState().selectedFrame).toBe(1)
    selectFrame(-5)
    expect(useDebugStore.getState().selectedFrame).toBe(0)
    selectFrame(Number.NaN)
    expect(useDebugStore.getState().selectedFrame).toBe(0)
  })

  it('收到 debug.resumed 后清空暂停现场', () => {
    useDebugStore.getState().ingestEvent(pausedEvent())
    useDebugStore.getState().ingestEvent({ type: 'debug.resumed', botId: 'bot-1' })

    expect(useDebugStore.getState().paused).toBeNull()
    expect(useDebugStore.getState().selectedFrame).toBe(0)
  })

  it('别的 bot 的暂停事件不覆盖当前调试目标', () => {
    useDebugStore.setState({ botId: 'bot-1' })
    useDebugStore.getState().ingestEvent(pausedEvent('bot-2'))

    expect(useDebugStore.getState().paused).toBeNull()
  })

  it('停止调试先逐文件清断点、再继续（顺序反了会撞下一个断点）', async () => {
    useDebugStore.setState({ botId: 'bot-1', breakpoints: { 'src/a.ts': [1, 2] } })
    useDebugStore.getState().ingestEvent(pausedEvent())

    await useDebugStore.getState().stopDebugging()

    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/a.ts', lines: [] },
      { type: 'debug.resume', botId: 'bot-1' },
    ])
    expect(useDebugStore.getState().breakpoints).toEqual({})
    expect(useDebugStore.getState().paused).toBeNull()
  })
})