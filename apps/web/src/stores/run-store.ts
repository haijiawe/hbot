/**
 * runStore：bot 运行状态与运行日志。
 *
 * 状态只由 `dispatch({type:'bot.start'|'bot.stop'})` 的返回值驱动，UI 不自己编运行态。
 */

import { create } from 'zustand'
import { errorMessage } from '../lib/format'
import type { BotStatus, LogLine } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

const MAX_LOG_LINES = 5000

export interface RunState {
  botId: string
  status: BotStatus
  queueDepth: number
  logs: LogLine[]
  busy: boolean
  error: string | null

  start: () => Promise<void>
  stop: () => Promise<void>
  clearLogs: () => void
  dismissError: () => void
}

function appendLogs(existing: LogLine[], incoming: LogLine[]): LogLine[] {
  const merged = [...existing, ...incoming]
  return merged.length > MAX_LOG_LINES ? merged.slice(merged.length - MAX_LOG_LINES) : merged
}

export const useRunStore = create<RunState>()((set, get) => ({
  botId: 'demo-bot',
  status: 'stopped',
  queueDepth: 0,
  logs: [],
  busy: false,
  error: null,

  async start() {
    if (get().busy) return
    set({ busy: true, error: null })
    try {
      const transition = await dispatch({ type: 'bot.start', botId: get().botId })
      set((state) => ({
        status: transition.bot.status,
        queueDepth: transition.bot.queueDepth,
        logs: appendLogs(state.logs, transition.lines),
        busy: false,
      }))
    } catch (error) {
      set({ busy: false, status: 'error', error: `启动失败：${errorMessage(error)}` })
    }
  },

  async stop() {
    if (get().busy) return
    set({ busy: true, error: null })
    try {
      const transition = await dispatch({ type: 'bot.stop', botId: get().botId })
      set((state) => ({
        status: transition.bot.status,
        queueDepth: transition.bot.queueDepth,
        logs: appendLogs(state.logs, transition.lines),
        busy: false,
      }))
    } catch (error) {
      set({ busy: false, error: `停止失败：${errorMessage(error)}` })
    }
  },

  clearLogs() {
    set({ logs: [] })
  },

  dismissError() {
    set({ error: null })
  },
}))