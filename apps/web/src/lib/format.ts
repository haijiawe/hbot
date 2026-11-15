/** 展示层格式化；纯函数，便于单测与复用。 */

import type { LogLevel } from './ipc'

export function formatClock(at: number): string {
  const date = new Date(at)
  const hours = String(date.getHours()).padStart(2, '0')
  const minutes = String(date.getMinutes()).padStart(2, '0')
  const seconds = String(date.getSeconds()).padStart(2, '0')
  return `${hours}:${minutes}:${seconds}`
}

export function formatCount(value: number): string {
  return value.toLocaleString('zh-CN')
}

/** 日志级别标签：颜色之外必须有文字，避免只靠颜色表达状态（a11y §11）。 */
export function logLevelLabel(level: LogLevel): string {
  switch (level) {
    case 'debug':
      return 'DEBUG'
    case 'info':
      return 'INFO'
    case 'warn':
      return 'WARN'
    case 'error':
      return 'ERROR'
    default: {
      const exhaustive: never = level
      return String(exhaustive)
    }
  }
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  return String(error)
}