/**
 * runStore：bot 运行状态与运行日志。
 *
 * 三条输入，职责分明：
 * - `bot.status` 事件是**唯一**的运行态真源（启停命令的返回值不再参与）；
 * - `bot.list` 只在打开项目 / 启停之后播种一次状态（`refresh()`），
 *   用来对齐"界面刚打开、核心其实早就在跑"，事件一到就以事件为准（`live`）；
 * - `logs.tail` 在首屏补读一次历史日志（`loadLogs()`），没有选中 bot 时读全项目
 *   （包含没有 bot 归属、`source = 'agent'` 的 Agent 行）。
 *
 * 不引用 workspaceStore / agentStore（AGENTS.md §0.10）。
 */

import { create } from 'zustand'
import type { LogLevel as LogLevelDto } from '../generated'
import { errorMessage } from '../lib/format'
import type { BotStatus, BotSummary, CoreEvent, LogLine, LogLevel } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

/** 日志环形缓冲上限：10k 行 —— 够复现一次故障，又不至于把内存吃光。 */
export const MAX_LOG_LINES = 10_000

/** 首屏从数据库补读的日志条数：够看清最近一次运行，又不至于一次拉太多。 */
export const LOG_TAIL_LIMIT = 500

export interface RunState {
  botId: string
  /** 当前项目的全部 bot（`bot_list` 的完整结果，最少含声明未启动的那些）。 */
  bots: BotSummary[]
  status: BotStatus
  logs: LogLine[]
  /** 启停命令在途（只用于禁用按钮，不代表运行态）。 */
  busy: boolean
  error: string | null
  /** 已经用 `bot.list` 播种过（避免重复查询）。 */
  hydrated: boolean
  /** 收到过 `bot.status` 事件：此后 `bot.list` 不再覆盖状态。 */
  live: boolean

  /** 用 `bot.list` 对齐一次状态；事件优先，`live` 时不动 status。 */
  refresh: () => Promise<void>
  /**
   * 选中一个 bot（概览/会话/运行态面板都按它取数据）。
   *
   * 选中的是本进程的 UI 意图，不落库：刷新后如果这个 bot 还在列表里就保留，
   * 不在了就回落到列表第一条（避免界面停在一条已经不存在的记录上）。
   */
  selectBot: (botId: string) => void
  /** 从 `logs_tail` 补读一次历史日志；`botId` 为空时读全项目（含 Agent 行）。 */
  loadLogs: () => Promise<void>
  hydrate: () => Promise<void>
  start: () => Promise<void>
  stop: () => Promise<void>
  /** 把一条核心事件折进 store（未知事件原样忽略）。 */
  ingestEvent: (event: CoreEvent) => void
  clearLogs: () => void
  dismissError: () => void
}

/** 契约的 `LogLevel` 有五档，日志面板只有四档：`trace` 折成 `debug`。 */
function panelLevel(level: LogLevelDto): LogLevel {
  return level === 'trace' ? 'debug' : level
}

/** 追加一行日志并保持环形缓冲上界。 */
function appendLine(existing: LogLine[], line: LogLine): LogLine[] {
  if (existing.length < MAX_LOG_LINES) return [...existing, line]
  // 满了就丢掉最旧的一行：slice 一次比 shift 便宜，也不会留下稀疏数组。
  return [...existing.slice(existing.length - MAX_LOG_LINES + 1), line]
}

export const useRunStore = create<RunState>()((set, get) => ({
  botId: '',
  bots: [],
  status: 'stopped',
  logs: [],
  busy: false,
  error: null,
  hydrated: false,
  live: false,

  selectBot(botId) {
    const found = get().bots.find((bot) => bot.botId === botId)
    set({ botId, status: found ? found.status : get().status })
  },
  async refresh() {
    try {
      const bots = await dispatch({ type: 'bot.list' })
      // 保留当前选中项（它可能不是第一台），只有在列表里找不到时才回落第一条。
      const selected = bots.find((bot) => bot.botId === get().botId) ?? bots[0]
      // 事件已经到过就别用快照覆盖它（快照可能比事件旧）。
      if (!selected || get().live) {
        set({ bots, hydrated: true })
        await get().loadLogs()
        return
      }
      set({ bots, botId: selected.botId, status: selected.status, hydrated: true })
      await get().loadLogs()
    } catch (error) {
      // 查不到列表不是致命错误（可能还没打开项目），但必须让用户看见。
      set({ hydrated: true, error: `读取 bot 列表失败：${errorMessage(error)}` })
    }
  },

  async loadLogs() {
    const botId = get().botId
    try {
      const rows = await dispatch({
        type: 'logs.tail',
        // 没有选中 bot 时读全项目：Agent 行没有 bot 归属，只有 null 视图看得到。
        botId: botId.length > 0 ? botId : null,
        limit: LOG_TAIL_LIMIT,
      })
      const lines: LogLine[] = rows.map((row) => ({
        level: panelLevel(row.level),
        // Agent 行用 `[agent]` 前缀和普通 bot 日志区分开。
        source: row.source === 'agent' ? '[agent]' : botId || 'bot',
        message: row.line,
        at: row.at,
      }))
      set({ logs: lines })
    } catch (error) {
      set({ error: `读取日志失败：${errorMessage(error)}` })
    }
  },

  async hydrate() {
    if (get().hydrated) return
    await get().refresh()
  },

  async start() {
    if (get().busy) return
    if (get().botId.length === 0) await get().refresh()
    const botId = get().botId
    if (botId.length === 0) {
      // 没有 bot 就没什么可启动的：说清楚，不要发一条 bot_start '' 让核心报参数错。
      set({ error: '启动失败：还没有可启动的 bot（先打开项目）' })
      return
    }
    set({ busy: true, error: null })
    try {
      // 只发命令：运行态由 `bot.status` 事件推进，返回值（`null`）不参与。
      await dispatch({ type: 'bot.start', botId })
      set({ busy: false })
      await get().refresh()
    } catch (error) {
      // 失败原因来自核心（-32003 已在运行 / -32006 未上线 / 启动期错误），原文展示。
      set({ busy: false, error: `启动失败：${errorMessage(error)}` })
    }
  },

  async stop() {
    if (get().busy) return
    const botId = get().botId
    if (botId.length === 0) return
    set({ busy: true, error: null })
    try {
      await dispatch({ type: 'bot.stop', botId })
      set({ busy: false })
      await get().refresh()
    } catch (error) {
      set({ busy: false, error: `停止失败：${errorMessage(error)}` })
    }
  },

  ingestEvent(event) {
    switch (event.type) {
      case 'bot.status': {
        const state = get()
        const bots = state.bots.map((bot) =>
          bot.botId === event.botId ? { ...bot, status: event.status } : bot,
        )
        // 只认当前 bot：多 bot 时别的 bot 的状态不该改这条状态栏。
        if (state.botId.length > 0 && state.botId !== event.botId) {
          set({ bots })
          return
        }
        set({
          bots,
          botId: event.botId,
          status: event.status,
          live: true,
          // 错误文本只由事件给：没有新错误时保留旧文本，等用户关掉（否则一闪就没了）。
          error: event.error ?? state.error,
        })
        return
      }

      case 'run.log': {
        const state = get()
        if (state.botId.length > 0 && state.botId !== event.botId) return
        const line: LogLine = {
          level: panelLevel(event.level),
          source: event.botId,
          message: event.line,
          at: event.at,
        }
        set({ botId: event.botId, logs: appendLine(state.logs, line) })
        return
      }

      default:
        return
    }
  },

  clearLogs() {
    set({ logs: [] })
  },

  dismissError() {
    set({ error: null })
  },
}))