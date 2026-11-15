/**
 * debugStore：断点、暂停现场、调试命令的发起方。
 *
 * 职责边界（docs/06-ide.md §8、AGENTS.md §0.11）：只拥有"调试"这一块状态，**不 import
 * 任何其他 store**。botId 由外壳 `app/App.tsx` 从 runStore 注入；事件由
 * `app/use-core-events.ts` 这一处扇出 —— 两个 store 之间没有直接引用。
 *
 * 断点同步照契约（docs/02-architecture.md §6.5.1）：
 * `debug_set_breakpoints` 是**全量覆盖**语义，`lines: []` 表示移除该文件的断点，
 * 所以每次只为"这一轮改过的文件"各发一条命令，带上它当前的全部行号。
 * 连点几行只发最后一次：120ms 防抖 + 按文件合并（见 `scheduleBreakpointFlush`）。
 */

import { create } from 'zustand'
import type { DebugScope, StackFrame, StepKind } from '../generated'
import { errorMessage } from '../lib/format'
import type { CoreEvent } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

/** 断点同步的防抖窗口（毫秒）：窗口内的多次点击合并成每个文件一条命令。 */
export const BREAKPOINT_FLUSH_MS = 120

/** 没有可调试目标时的提示（断点变更不会发命令，只留一条错）。 */
const NO_BOT_ID = '还没有可调试的 bot：先启动一个 bot，断点才会同步到核心。'

/** 暂停现场：`debug.paused` 的载荷（`selectedFrame` 是 UI 的选中项，不在载荷里）。 */
export interface PausedState {
  file: string
  line: number
  scopes: DebugScope[]
  callStack: StackFrame[]
}

export interface DebugState {
  /** 当前调试目标；空串表示没有可调试的 bot（此时不发任何调试命令）。 */
  botId: string
  /** 按文件索引的断点行号（1-based、升序、去重）；空数组不落库（表示该文件没有断点）。 */
  breakpoints: Record<string, number[]>
  /** 暂停现场；`null` = 未在调试。 */
  paused: PausedState | null
  /** 调用栈里选中的帧下标（默认指向当前帧）。 */
  selectedFrame: number
  busy: boolean
  error: string | null

  setBotId: (botId: string) => void
  toggleBreakpoint: (file: string, line: number) => void
  clearBreakpoints: () => void
  selectFrame: (index: number) => void
  resume: () => Promise<void>
  step: (kind: StepKind) => Promise<void>
  stopDebugging: () => Promise<void>
  ingestEvent: (event: CoreEvent) => void
  dismissError: () => void
}

/** 这一轮改过的文件（与一个定时器 = "按文件合并"的防抖）。 */
const pendingFiles = new Set<string>()
let flushTimer: ReturnType<typeof setTimeout> | null = null

function cancelPendingFlush(): void {
  pendingFiles.clear()
  if (flushTimer !== null) {
    clearTimeout(flushTimer)
    flushTimer = null
  }
}

function scheduleBreakpointFlush(file: string): void {
  pendingFiles.add(file)
  if (flushTimer !== null) return
  flushTimer = setTimeout(() => {
    flushTimer = null
    void flushBreakpoints()
  }, BREAKPOINT_FLUSH_MS)
}

/** 把这一轮改过的文件各发一条 `debug_set_breakpoints`（lines = 该文件当前全量行号）。 */
async function flushBreakpoints(): Promise<void> {
  const files = [...pendingFiles]
  pendingFiles.clear()
  const { botId, breakpoints } = useDebugStore.getState()
  if (botId.length === 0) {
    useDebugStore.setState({ error: NO_BOT_ID })
    return
  }
  for (const file of files) {
    try {
      await dispatch({ type: 'debug.set-breakpoints', botId, file, lines: breakpoints[file] ?? [] })
    } catch (error) {
      useDebugStore.setState({ error: `同步断点失败：${errorMessage(error)}` })
    }
  }
}

/** 换 bot 后要重新把已有断点同步过去（没有 bot 时只留状态）。 */
function syncAllBreakpoints(): void {
  const { botId, breakpoints } = useDebugStore.getState()
  if (botId.length === 0) return
  for (const file of Object.keys(breakpoints)) scheduleBreakpointFlush(file)
}

export const useDebugStore = create<DebugState>()((set, get) => ({
  botId: '',
  breakpoints: {},
  paused: null,
  selectedFrame: 0,
  busy: false,
  error: null,

  setBotId: (botId) => {
    if (get().botId === botId) return
    // 换了调试目标：旧目标上的暂停现场不再有意义，断点保留并同步给新目标。
    cancelPendingFlush()
    set({ botId, paused: null, selectedFrame: 0, busy: false, error: null })
    syncAllBreakpoints()
  },

  toggleBreakpoint: (file, line) => {
    if (file.length === 0 || !Number.isInteger(line) || line < 1) return
    set((state) => {
      const current = state.breakpoints[file] ?? []
      const next = current.includes(line)
        ? current.filter((candidate) => candidate !== line)
        : [...current, line].sort((left, right) => left - right)
      const breakpoints = { ...state.breakpoints }
      if (next.length === 0) delete breakpoints[file]
      else breakpoints[file] = next
      return { breakpoints, error: null }
    })
    scheduleBreakpointFlush(file)
  },

  clearBreakpoints: () => {
    const files = Object.keys(get().breakpoints)
    set({ breakpoints: {} })
    // 仍走防抖通道：清空同样是"全量覆盖"，`lines: []` 才是移除。
    for (const file of files) scheduleBreakpointFlush(file)
  },

  selectFrame: (index) => {
    const { paused } = get()
    if (paused === null || !Number.isFinite(index)) return
    const last = Math.max(paused.callStack.length - 1, 0)
    set({ selectedFrame: Math.min(Math.max(Math.trunc(index), 0), last) })
  },

  resume: async () => {
    const { botId, paused } = get()
    if (paused === null) return
    if (botId.length === 0) {
      set({ error: NO_BOT_ID })
      return
    }
    set({ busy: true, error: null })
    try {
      await dispatch({ type: 'debug.resume', botId })
      // 继续之后暂停现场就结束了；命中下一个断点时核心会重新推 `debug.paused`。
      set({ paused: null, selectedFrame: 0, busy: false })
    } catch (error) {
      set({ busy: false, error: `继续失败：${errorMessage(error)}` })
    }
  },

  step: async (kind) => {
    const { botId, paused } = get()
    if (paused === null) return
    if (botId.length === 0) {
      set({ error: NO_BOT_ID })
      return
    }
    set({ busy: true, error: null })
    try {
      await dispatch({ type: 'debug.step', botId, kind })
      // 步进期间保留暂停现场（高亮停在原行），等核心推来新的 `debug.paused` / `debug.resumed`。
      set({ busy: false })
    } catch (error) {
      set({ busy: false, error: `单步失败：${errorMessage(error)}` })
    }
  },

  stopDebugging: async () => {
    const { botId, paused, breakpoints } = get()
    const files = Object.keys(breakpoints)
    cancelPendingFlush()
    set({ paused: null, selectedFrame: 0, breakpoints: {}, busy: botId.length > 0, error: null })
    if (botId.length === 0) {
      set({ busy: false })
      return
    }
    try {
      // 顺序不能反：worker 正阻塞在 DebugSink 里等一条命令，先 resume 会在清断点到达之前
      // 撞上下一个断点，白停一次。所以先逐文件 `lines: []`，再继续。
      for (const file of files) {
        await dispatch({ type: 'debug.set-breakpoints', botId, file, lines: [] })
      }
      if (paused !== null) await dispatch({ type: 'debug.resume', botId })
    } catch (error) {
      set({ error: `停止调试失败：${errorMessage(error)}` })
    } finally {
      set({ busy: false })
    }
  },

  ingestEvent: (event) => {
    if (event.type === 'debug.paused') {
      set((state) => {
        // 别的 bot 的暂停现场不属于这里（与 runStore 过滤 bot.status 的口径一致）。
        if (state.botId.length > 0 && state.botId !== event.botId) return state
        const callStack = event.callStack
        return {
          botId: event.botId,
          paused: { file: event.file, line: event.line, scopes: event.scopes, callStack },
          // 契约：callStack 外层在前，当前帧是最后一项。
          selectedFrame: Math.max(callStack.length - 1, 0),
          busy: false,
          error: null,
        }
      })
      return
    }
    if (event.type === 'debug.resumed') {
      set((state) => {
        if (state.botId.length > 0 && state.botId !== event.botId) return state
        return { paused: null, selectedFrame: 0, busy: false }
      })
    }
  },

  dismissError: () => set({ error: null }),
}))