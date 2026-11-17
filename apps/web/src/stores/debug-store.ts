/**
 * debugStore：断点（含条件）、暂停现场、调试命令的发起方。
 *
 * 职责边界（docs/06-ide.md §8、AGENTS.md §0.11）：只拥有"调试"这一块状态，**不 import
 * 任何其他 store**。botId 由外壳 `app/App.tsx` 从 runStore 注入；事件由
 * `app/use-core-events.ts` 这一处扇出 —— 两个 store 之间没有直接引用。
 *
 * 断点同步照契约（docs/02-architecture.md §6.5.1）：
 * `debug_set_breakpoints` 是**全量覆盖**语义，`breakpoints: []` 表示移除该文件的断点，
 * 所以每次只为"这一轮改过的文件"各发一条命令，带上它当前的全部条目（行号 + 条件）。
 * 连点几行只发最后一次：120ms 防抖 + 按文件合并（见 `scheduleBreakpointFlush`）。
 *
 * 条件断点：条件是一段**同步 JS 表达式**，由核心在 isolate 的 JS 侧求值（那里才有
 * 作用域），这里只负责存文本、发文本。条件为空串/纯空白 = 无条件断点（wire 上是
 * `null`），所以"清除条件"与"删除断点"是两件事。
 *
 * 日志点（logpoint）：wire 上同一个 `DebugBreakpoint` 的元素，只是 `log` 非 `null` ——
 * 命中时**不暂停**，核心把模板展开后推一条 `debug.log`。所以一行可以**同时**有断点与
 * 日志点：store 里同一行存两条（`log: null` 的那条停下，`log: "..."` 的那条打日志），
 * 与 wire 的数组形状一一对应，不需要第二张表。
 *
 * `debug.log` 进一个**有上限的环形缓冲**（`MAX_LOG_LINES`）：日志点是高频事件，
 * 无上限的数组在长跑里会把内存吃光，而日志面板只需要"最近发生了什么"。
 */

import { create } from 'zustand'
import type { DebugBreakpoint, DebugScope, StackFrame, StepKind } from '../generated'
import { errorMessage } from '../lib/format'
import type { CoreEvent } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

/** 断点同步的防抖窗口（毫秒）：窗口内的多次点击合并成每个文件一条命令。 */
export const BREAKPOINT_FLUSH_MS = 120

/**
 * 条件长度上限（字符，不是字节），与核心的 `MAX_CONDITION_CHARS`
 * （`crates/hbot-rpc/src/app/commands/debug.rs`）一致。
 *
 * 两边都查是有意的：核心那条是契约（绕过 UI 也得拦住），这里这条只是让用户
 * 在输入框里当场看到提示，不必等一次往返才被 `-32002` 打回。
 */
export const MAX_CONDITION_CHARS = 200

/**
 * 日志点模板长度上限（字符，不是字节），与核心的 `MAX_LOG_CHARS`
 * （`crates/hbot-rpc/src/app/commands/debug.rs`）一致。
 */
export const MAX_LOG_CHARS = 500

/**
 * 日志缓冲上限（行）。超出后丢最旧的 —— 日志点是**高频**事件（循环里可能每轮一条），
 * 数组无上限跑一晚上就是内存泄漏。2000 行足够看"刚才发生了什么"。
 */
export const MAX_LOG_LINES = 2000

/** 一条日志点事件（`debug.log`）进缓冲后的形状。 */
export interface LogLine {
  /** 单调递增序号，只用于 React `key`（同一文件行可能连着命中很多次）。 */
  seq: number
  file: string
  line: number
  /** 核心展开后的消息（模板里的 `{表达式}` 已被替换）。 */
  message: string
}

/** 没有可调试目标时的提示（断点变更不会发命令，只留一条错）。 */
const NO_BOT_ID = '还没有可调试的 bot：先启动一个 bot，断点才会同步到核心。'

/** 暂停现场：`debug.paused` 的载荷（`selectedFrame` 是 UI 的选中项，不在载荷里）。 */
export interface PausedState {
  file: string
  line: number
  /** 命中的条件原文；`null` = 无条件断点，或单步停在这里。 */
  condition: string | null
  scopes: DebugScope[]
  callStack: StackFrame[]
}

export interface DebugState {
  /** 当前调试目标；空串表示没有可调试的 bot（此时不发任何调试命令）。 */
  botId: string
  /**
   * 按文件索引的断点与日志点（行号 1-based、升序；同一行最多两条：`log: null` 的
   * 停下，`log: "..."` 的打日志）；空数组不落库（表示该文件两样都没有）。
   *
   * 单一真源：条件与模板都存这里，没有第二份 `conditions` / `logs` 表 —— 否则
   * "删断点但没删条件"之类的半更新迟早出现，而 wire 要的是同一行的东西一起发。
   */
  breakpoints: Record<string, DebugBreakpoint[]>
  /** `debug.log` 的环形缓冲（最旧在前，上限 `MAX_LOG_LINES`）。 */
  logs: LogLine[]
  /** 日志序号（只为了给 React `key` 一个稳定值）。 */
  logSeq: number
  /** 暂停现场；`null` = 未在调试。 */
  paused: PausedState | null
  /** 调用栈里选中的帧下标（默认指向当前帧）。 */
  selectedFrame: number
  busy: boolean
  error: string | null

  setBotId: (botId: string) => void
  toggleBreakpoint: (file: string, line: number) => void
  removeBreakpoint: (file: string, line: number) => void
  /** 设置/清除某一行的条件（空串或纯空白 = 清除条件，断点本身留下）。 */
  setBreakpointCondition: (file: string, line: number, condition: string | null) => void
  /**
   * 设置/删除某一行的日志点（空串或纯空白 = 删除日志点）。
   *
   * 只碰 `log` 那条条目：同一行如果还有断点，断点留下（"打了日志再停下"是合法组合）。
   */
  setLogpoint: (file: string, line: number, template: string | null) => void
  /** 删掉某一行的日志点（断点若在同一行则留下）。 */
  removeLogpoint: (file: string, line: number) => void
  clearBreakpoints: () => void
  /** 清空日志缓冲（不影响日志点本身）。 */
  clearLogs: () => void
  selectFrame: (index: number) => void
  resume: () => Promise<void>
  step: (kind: StepKind) => Promise<void>
  stopDebugging: () => Promise<void>
  ingestEvent: (event: CoreEvent) => void
  dismissError: () => void
}

/** 该行那一份**断点**条目（`log: null` 的；日志点条目不算）。 */
function breakpointAt(
  entries: readonly DebugBreakpoint[] | undefined,
  line: number,
): DebugBreakpoint | undefined {
  return (entries ?? []).find((entry) => entry.line === line && entry.log === null)
}

/** 该行有没有断点（**停下**的那种；只有日志点不算）。 */
export function hasBreakpointAt(
  entries: readonly DebugBreakpoint[] | undefined,
  line: number,
): boolean {
  return breakpointAt(entries, line) !== undefined
}

/** 该行的条件；没有条件（或没有断点）都是 `null`。 */
export function conditionAt(
  entries: readonly DebugBreakpoint[] | undefined,
  line: number,
): string | null {
  return breakpointAt(entries, line)?.condition ?? null
}

/** 该行的日志点模板；没有日志点就是 `null`。 */
export function logpointAt(
  entries: readonly DebugBreakpoint[] | undefined,
  line: number,
): string | null {
  return (entries ?? []).find((entry) => entry.line === line && entry.log !== null)?.log ?? null
}

/** 该行有没有日志点。 */
export function hasLogpointAt(
  entries: readonly DebugBreakpoint[] | undefined,
  line: number,
): boolean {
  return logpointAt(entries, line) !== null
}

/**
 * 把输入框里的文本归一成 wire 值：去掉首尾空白，空串 = 无条件（`null`）。
 *
 * 归一化只发生一次（写进 store 之前），下游（gutter 装饰、面板、IPC 载荷）看到的
 * 就是最终形态 —— 否则"有断点但条件是空串"会在三处各自解释一遍。
 */
export function normalizeCondition(condition: string | null): string | null {
  if (condition === null) return null
  const trimmed = condition.trim()
  return trimmed.length === 0 ? null : trimmed
}

/**
 * 日志点模板归一：去掉首尾空白，空串 = 没有日志点（`null`）。
 *
 * 与 `normalizeCondition` 分开写而不是复用：两边的**上限与含义**都不同（模板要的
 * 是给人看的消息，条件要的是一段表达式），共用一个函数迟早会有人往模板上套 200 字符。
 */
export function normalizeLog(template: string | null): string | null {
  if (template === null) return null
  const trimmed = template.trim()
  return trimmed.length === 0 ? null : trimmed
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

/** 把这一轮改过的文件各发一条 `debug_set_breakpoints`（breakpoints = 该文件当前全量条目）。 */
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
      await dispatch({
        type: 'debug.set-breakpoints',
        botId,
        file,
        breakpoints: breakpoints[file] ?? [],
      })
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

/**
 * 排序：行号升序；同一行**断点在前、日志点在后**。
 *
 * 顺序固定不是因为 wire 在意（核心按表存），而是 gutter 装饰按数组顺序画：
 * 同一行两样都有时最后画的那个可见，让日志点的方块压在圆点上是错的 —— 圆点才是
 * "会停下来"那个更该被看见的信号，所以断点排前面、日志点后画（日志点是方块，
 * 故意用不同形状，压在上面反而认不出是日志点）。装饰层会**跳过**同一行的第二个，
 * 见 `breakpointGlyphs`。
 */
function sortEntries(entries: DebugBreakpoint[]): DebugBreakpoint[] {
  return entries.sort(
    (left, right) =>
      left.line - right.line || (left.log === null ? 0 : 1) - (right.log === null ? 0 : 1),
  )
}

/** 按行号写入一条**断点**（同一行的日志点条目原样保留）。 */
function withBreakpoint(
  entries: readonly DebugBreakpoint[],
  line: number,
  condition: string | null,
): DebugBreakpoint[] {
  const next = entries.filter((entry) => !(entry.line === line && entry.log === null))
  next.push({ line, condition, log: null })
  return sortEntries(next)
}

/** 按行号写入一条**日志点**（同一行的断点条目原样保留，其条件与日志点无关）。 */
function withLogpoint(
  entries: readonly DebugBreakpoint[],
  line: number,
  template: string,
): DebugBreakpoint[] {
  const next = entries.filter((entry) => !(entry.line === line && entry.log !== null))
  next.push({ line, condition: null, log: template })
  return sortEntries(next)
}

/** 从某个文件的条目里去掉匹配的那些；空了就把文件键删掉（空数组不落库）。 */
function withoutEntries(
  breakpoints: Record<string, DebugBreakpoint[]>,
  file: string,
  keep: (entry: DebugBreakpoint) => boolean,
): Record<string, DebugBreakpoint[]> {
  const next = (breakpoints[file] ?? []).filter(keep)
  const copy = { ...breakpoints }
  if (next.length === 0) delete copy[file]
  else copy[file] = next
  return copy
}

export const useDebugStore = create<DebugState>()((set, get) => ({
  botId: '',
  breakpoints: {},
  logs: [],
  logSeq: 0,
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
      return {
        // 已有断点 → 只删断点那条（同一行的日志点留下）；没有 → 新加一条无条件断点。
        breakpoints: hasBreakpointAt(current, line)
          ? withoutEntries(state.breakpoints, file, (entry) => !(entry.line === line && entry.log === null))
          : { ...state.breakpoints, [file]: withBreakpoint(current, line, null) },
        error: null,
      }
    })
    scheduleBreakpointFlush(file)
  },

  removeBreakpoint: (file, line) => {
    if (!hasBreakpointAt(get().breakpoints[file], line)) return
    set((state) => ({
      breakpoints: withoutEntries(
        state.breakpoints,
        file,
        (entry) => !(entry.line === line && entry.log === null),
      ),
      error: null,
    }))
    scheduleBreakpointFlush(file)
  },

  setBreakpointCondition: (file, line, condition) => {
    if (file.length === 0 || !Number.isInteger(line) || line < 1) return
    const normalized = normalizeCondition(condition)
    if (normalized !== null && normalized.length > MAX_CONDITION_CHARS) {
      set({
        error: `第 ${line} 行的断点条件有 ${normalized.length} 个字符，超过上限 ${MAX_CONDITION_CHARS}：条件是一段同步 JS 表达式（如 \`count > 3\`），不该把整段代码粘进来。`,
      })
      return
    }
    set((state) => {
      // 没有断点时"设置条件"顺便把断点建出来：右键一行的意图就是"要在这里停"。
      const current = state.breakpoints[file] ?? []
      return {
        breakpoints: { ...state.breakpoints, [file]: withBreakpoint(current, line, normalized) },
        error: null,
      }
    })
    scheduleBreakpointFlush(file)
  },

  setLogpoint: (file, line, template) => {
    if (file.length === 0 || !Number.isInteger(line) || line < 1) return
    const normalized = normalizeLog(template)
    if (normalized !== null && normalized.length > MAX_LOG_CHARS) {
      set({
        error: `第 ${line} 行的日志点模板有 ${normalized.length} 个字符，超过上限 ${MAX_LOG_CHARS}：模板里可以写 {表达式}，但不是用来粘整段代码的。`,
      })
      return
    }
    set((state) => {
      const current = state.breakpoints[file] ?? []
      if (normalized === null) {
        // 留空 = 删掉日志点（同一行的断点不动）。
        return {
          breakpoints: withoutEntries(
            state.breakpoints,
            file,
            (entry) => !(entry.line === line && entry.log !== null),
          ),
          error: null,
        }
      }
      return {
        breakpoints: { ...state.breakpoints, [file]: withLogpoint(current, line, normalized) },
        error: null,
      }
    })
    scheduleBreakpointFlush(file)
  },

  removeLogpoint: (file, line) => {
    if (!hasLogpointAt(get().breakpoints[file], line)) return
    set((state) => ({
      breakpoints: withoutEntries(
        state.breakpoints,
        file,
        (entry) => !(entry.line === line && entry.log !== null),
      ),
      error: null,
    }))
    scheduleBreakpointFlush(file)
  },

  clearBreakpoints: () => {
    const files = Object.keys(get().breakpoints)
    set({ breakpoints: {} })
    // 仍走防抖通道：清空同样是"全量覆盖"，`breakpoints: []` 才是移除。
    for (const file of files) scheduleBreakpointFlush(file)
  },

  clearLogs: () => set({ logs: [] }),

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
      // 撞上下一个断点，白停一次。所以先逐文件 `breakpoints: []`，再继续。
      for (const file of files) {
        await dispatch({ type: 'debug.set-breakpoints', botId, file, breakpoints: [] })
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
          paused: {
            file: event.file,
            line: event.line,
            // 核心带过来的"命中的条件"原文；无条件断点与单步都是 null。
            condition: event.condition,
            scopes: event.scopes,
            callStack,
          },
          // 契约：callStack 外层在前，当前帧是最后一项。
          selectedFrame: Math.max(callStack.length - 1, 0),
          busy: false,
          error: null,
        }
      })
      return
    }
    if (event.type === 'debug.log') {
      set((state) => {
        // 与 `debug.paused` 同口径过滤别的 bot；但日志不设置 botId（一条日志不该
        // 把调试目标换掉 —— 那会让暂停面板跟着跳）。
        if (state.botId.length > 0 && state.botId !== event.botId) return state
        const seq = state.logSeq + 1
        const logs = [...state.logs, { seq, file: event.file, line: event.line, message: event.message }]
        return {
          logSeq: seq,
          // 超上限只留最近 MAX_LOG_LINES 条（丢最旧的，不丢最新的）。
          logs: logs.length > MAX_LOG_LINES ? logs.slice(logs.length - MAX_LOG_LINES) : logs,
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