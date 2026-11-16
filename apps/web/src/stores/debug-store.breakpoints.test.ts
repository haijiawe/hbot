/**
 * 断点写入的**乐观路径**与"编辑器 ↔ store"最小面（120ms 防抖）。
 *
 * `debug-store.test.ts` 已经钉了合并/移除/换 bot 等主要行为；这里补它没覆盖的三条，
 * 都是编辑器 gutter 点击直接依赖的：
 * 1. 点下去**立刻**写进 store（不等命令往返）——否则 gutter 上的断点要点两下才出现；
 * 2. 防抖窗口内只发一条命令，且发的是窗口结束时的最终行号（不是中间态）；
 * 3. `lines: []` 才是"移除"的线上表示（空数组不能变成"什么都不发"）。
 *
 * 为什么不测 Monaco 那一侧：见 `features/editor/editor-surface.tsx` 顶部注释 ——
 * 断点点击绑在真 Monaco 实例的 `onMouseDown`（`MouseTargetType.GUTTER_GLYPH_MARGIN`）上，
 * jsdom 里没有布局、没有 glyph margin，构造不出这个事件。这一层的最小可替代验证就是
 * "store 在同一个 tick 内已经写好"，也就是下面第一条。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { dispatch } from '../lib/ipc'
import { BREAKPOINT_FLUSH_MS, useDebugStore } from './debug-store'

vi.mock('../lib/ipc', () => ({ dispatch: vi.fn(async () => null) }))

const INITIAL_STATE = {
  botId: 'bot-1',
  breakpoints: {},
  paused: null,
  selectedFrame: 0,
  busy: false,
  error: null,
}

/** 走完防抖窗口，并把 flush 里的 await 链推进到结束。 */
async function flushDebounce(): Promise<void> {
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  for (let round = 0; round < 5; round += 1) await Promise.resolve()
}

const sentCommands = () => vi.mocked(dispatch).mock.calls.map(([command]) => command)

beforeEach(async () => {
  vi.useFakeTimers()
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  useDebugStore.setState({ ...INITIAL_STATE })
  vi.mocked(dispatch).mockClear()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('断点：乐观写入 + 防抖', () => {
  it('点下去 store 立刻有断点，命令要等 120ms 窗口才发（gutter 不必等往返）', async () => {
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 5)

    // 同一个 tick：UI 读到的状态已经是"这一行有断点"。
    expect(useDebugStore.getState().breakpoints).toEqual({ 'src/main.ts': [5] })
    expect(sentCommands()).toEqual([])

    // 窗口没走完也不发。
    await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS - 1)
    expect(sentCommands()).toEqual([])

    await flushDebounce()
    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/main.ts', lines: [5] },
    ])
  })

  it('窗口内点两下同一行：只发一条，且不带这一行（第二次点击是移除）', async () => {
    const { toggleBreakpoint } = useDebugStore.getState()

    toggleBreakpoint('src/main.ts', 5)
    expect(useDebugStore.getState().breakpoints).toEqual({ 'src/main.ts': [5] })

    toggleBreakpoint('src/main.ts', 5)
    expect(useDebugStore.getState().breakpoints).toEqual({})

    await flushDebounce()
    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/main.ts', lines: [] },
    ])
  })

  it('清空所有断点：每个文件各发一条 lines: []（空数组 = 移除，不是"不发"）', async () => {
    useDebugStore.setState({ breakpoints: { 'src/a.ts': [1, 4], 'src/b.ts': [9] } })

    useDebugStore.getState().clearBreakpoints()
    expect(useDebugStore.getState().breakpoints).toEqual({})

    await flushDebounce()
    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/a.ts', lines: [] },
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/b.ts', lines: [] },
    ])
  })

  it('换调试目标：已有断点重发给新 bot（旧 bot 上不残留）', async () => {
    useDebugStore.setState({ botId: 'bot-1', breakpoints: { 'src/a.ts': [2] } })

    useDebugStore.getState().setBotId('bot-2')
    await flushDebounce()

    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-2', file: 'src/a.ts', lines: [2] },
    ])
    // 换目标时旧暂停现场作废。
    expect(useDebugStore.getState().paused).toBeNull()
  })

  it('没有 bot 时只报错不发命令（命令里没有合法 botId 可填）', async () => {
    useDebugStore.setState({ botId: '' })

    useDebugStore.getState().toggleBreakpoint('src/main.ts', 5)
    await flushDebounce()

    expect(sentCommands()).toEqual([])
    expect(useDebugStore.getState().error).toContain('还没有可调试的 bot')
    // 断点本身保留：等 bot 起来（setBotId）再同步，不能让用户白点。
    expect(useDebugStore.getState().breakpoints).toEqual({ 'src/main.ts': [5] })
  })

  it('停止调试时本来就没暂停：只清断点，不凭空发 resume', async () => {
    useDebugStore.setState({ breakpoints: { 'src/main.ts': [5] } })

    await useDebugStore.getState().stopDebugging()

    expect(sentCommands()).toEqual([
      { type: 'debug.set-breakpoints', botId: 'bot-1', file: 'src/main.ts', lines: [] },
    ])
  })
})