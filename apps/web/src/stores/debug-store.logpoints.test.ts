/**
 * 日志点（logpoint）在 store 里的行为：与断点同一张表、同一行可共存、模板上限、
 * 以及 `debug.log` 的环形缓冲。
 *
 * 为什么这些必须钉住：
 * 1. 日志点与断点在 wire 上是**同一个元素类型**，只是 `log` 非 `null`。store 里若只存
 *    一份"每行一个条目"，"同一行既打日志又停下"就无处安放 —— 而核心是支持的。
 * 2. 日志是高频事件。缓冲必须有上限且丢最旧的，否则跑一晚上就是内存泄漏。
 * 3. 模板长度上限与核心的 `MAX_LOG_CHARS` 必须一致：UI 先拦一道，别等一次往返才被
 *    `-32002` 打回。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { dispatch } from '../lib/ipc'
import type { CoreEvent } from '../lib/ipc'
import {
  BREAKPOINT_FLUSH_MS,
  MAX_LOG_CHARS,
  MAX_LOG_LINES,
  conditionAt,
  hasBreakpointAt,
  hasLogpointAt,
  logpointAt,
  useDebugStore,
} from './debug-store'

vi.mock('../lib/ipc', () => ({ dispatch: vi.fn(async () => null) }))

const INITIAL_STATE = {
  botId: 'bot-1',
  breakpoints: {},
  logs: [],
  logSeq: 0,
  paused: null,
  selectedFrame: 0,
  busy: false,
  error: null,
}

async function flushDebounce(): Promise<void> {
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  for (let round = 0; round < 5; round += 1) await Promise.resolve()
}

const sentCommands = () => vi.mocked(dispatch).mock.calls.map(([command]) => command)

function logEvent(line: number, message: string, botId = 'bot-1'): CoreEvent {
  return { type: 'debug.log', botId, file: 'src/main.ts', line, message }
}

beforeEach(async () => {
  vi.useFakeTimers()
  await vi.advanceTimersByTimeAsync(BREAKPOINT_FLUSH_MS)
  useDebugStore.setState({ ...INITIAL_STATE })
  vi.mocked(dispatch).mockClear()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('日志点：写入 store 与 wire 载荷', () => {
  it('设置日志点后 store 里有值，wire 上是一条 log 非 null 的条目', async () => {
    useDebugStore.getState().setLogpoint('src/main.ts', 7, 'count={count}')

    expect(useDebugStore.getState().breakpoints).toEqual({
      'src/main.ts': [{ line: 7, condition: null, log: 'count={count}' }],
    })
    expect(logpointAt(useDebugStore.getState().breakpoints['src/main.ts'], 7)).toBe('count={count}')
    // 日志点**不是**断点：它不会停下来。
    expect(hasBreakpointAt(useDebugStore.getState().breakpoints['src/main.ts'], 7)).toBe(false)

    await flushDebounce()
    expect(sentCommands()).toEqual([
      {
        type: 'debug.set-breakpoints',
        botId: 'bot-1',
        file: 'src/main.ts',
        breakpoints: [{ line: 7, condition: null, log: 'count={count}' }],
      },
    ])
  })

  it('同一行可以既是断点又是日志点：两条条目，两样都不丢', () => {
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 7)
    useDebugStore.getState().setLogpoint('src/main.ts', 7, 'count={count}')

    const entries = useDebugStore.getState().breakpoints['src/main.ts'] ?? []
    expect(entries).toHaveLength(2)
    // 断点在前、日志点在后（排序是 UI 的可见性约定）。
    expect(entries[0]).toEqual({ line: 7, condition: null, log: null })
    expect(entries[1]).toEqual({ line: 7, condition: null, log: 'count={count}' })
    expect(hasBreakpointAt(entries, 7)).toBe(true)
    expect(hasLogpointAt(entries, 7)).toBe(true)
  })

  it('留空 = 删除日志点，同一行的断点留下', () => {
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 7)
    useDebugStore.getState().setLogpoint('src/main.ts', 7, 'x={x}')
    useDebugStore.getState().setLogpoint('src/main.ts', 7, '   ')

    const entries = useDebugStore.getState().breakpoints['src/main.ts'] ?? []
    expect(entries).toEqual([{ line: 7, condition: null, log: null }])
    expect(conditionAt(entries, 7)).toBeNull()
  })

  it('删断点不会顺手删掉同一行的日志点', () => {
    useDebugStore.getState().toggleBreakpoint('src/main.ts', 7)
    useDebugStore.getState().setLogpoint('src/main.ts', 7, 'x={x}')
    useDebugStore.getState().removeBreakpoint('src/main.ts', 7)

    const entries = useDebugStore.getState().breakpoints['src/main.ts'] ?? []
    expect(entries).toEqual([{ line: 7, condition: null, log: 'x={x}' }])
    expect(hasLogpointAt(entries, 7)).toBe(true)
  })

  it('模板超过上限时拒绝并给出错误，store 不变', () => {
    const tooLong = 'x'.repeat(MAX_LOG_CHARS + 1)
    useDebugStore.getState().setLogpoint('src/main.ts', 7, tooLong)

    expect(useDebugStore.getState().breakpoints).toEqual({})
    expect(useDebugStore.getState().error).toContain('上限')
    // 上限是**字符**数，不是字节数：中文模板也在同一把尺子上。
    expect(MAX_LOG_CHARS).toBe(500)
  })
})

describe('debug.log 环形缓冲', () => {
  it('事件进缓冲（file / line / message 都在，序号单调递增）', () => {
    useDebugStore.getState().ingestEvent(logEvent(7, '3 items'))
    useDebugStore.getState().ingestEvent(logEvent(8, 'done'))

    expect(useDebugStore.getState().logs.map(({ file, line, message }) => ({ file, line, message }))).toEqual([
      { file: 'src/main.ts', line: 7, message: '3 items' },
      { file: 'src/main.ts', line: 8, message: 'done' },
    ])
    expect(useDebugStore.getState().logs[1]?.seq).toBeGreaterThan(useDebugStore.getState().logs[0]?.seq ?? 0)
  })

  it('超过上限时丢最旧的，长度不涨过 MAX_LOG_LINES', () => {
    for (let index = 1; index <= MAX_LOG_LINES + 5; index += 1) {
      useDebugStore.getState().ingestEvent(logEvent(index, `第 ${index} 条`))
    }
    const logs = useDebugStore.getState().logs
    expect(logs).toHaveLength(MAX_LOG_LINES)
    // 最旧的 5 条被丢掉：第一条留下的是第 6 条。
    expect(logs[0]?.message).toBe('第 6 条')
    expect(logs[logs.length - 1]?.message).toBe(`第 ${MAX_LOG_LINES + 5} 条`)
  })

  it('别的 bot 的日志不会串进来（与 debug.paused 同口径）', () => {
    useDebugStore.getState().ingestEvent(logEvent(7, 'ours'))
    useDebugStore.getState().ingestEvent(logEvent(8, 'theirs', 'bot-2'))

    expect(useDebugStore.getState().logs.map((entry) => entry.message)).toEqual(['ours'])
  })

  it('清空日志只清缓冲，不动日志点本身', () => {
    useDebugStore.getState().setLogpoint('src/main.ts', 7, 'x={x}')
    useDebugStore.getState().ingestEvent(logEvent(7, 'a'))
    useDebugStore.getState().clearLogs()

    expect(useDebugStore.getState().logs).toEqual([])
    expect(hasLogpointAt(useDebugStore.getState().breakpoints['src/main.ts'], 7)).toBe(true)
  })
})