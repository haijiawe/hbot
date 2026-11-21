/**
 * 日志筛选：级别开关 + 子串（大小写不敏感）。
 *
 * 从 `features/workbench/logs-view.tsx` 搬出来的 —— 整页日志面已经删掉，
 * 日志只剩底部面板一个渲染器；筛选数学放 lib 是为了它能单独测（node 环境，不挂 DOM）。
 */

import type { LogLine, LogLevel } from './ipc'

/** 四档级别（与日志面板同口径：contract 的 trace 已在 store 折成 debug）。 */
export const LOG_LEVELS: readonly LogLevel[] = ['error', 'warn', 'info', 'debug']

export const LOG_LEVEL_LABELS: Readonly<Record<LogLevel, string>> = {
  error: '错误',
  warn: '警告',
  info: '信息',
  debug: '调试',
}

/** 过滤：级别开关 + 子串（大小写不敏感）。空查询表示不过滤。 */
export function filterLogs(
  lines: readonly LogLine[],
  levels: readonly LogLevel[],
  query: string,
): LogLine[] {
  const needle = query.trim().toLowerCase()
  return lines.filter((line) => {
    if (levels.length > 0 && !levels.includes(line.level)) return false
    if (needle.length === 0) return true
    return line.message.toLowerCase().includes(needle)
  })
}

/** 各档条数（工具栏上的徽标；不参与过滤，统计的永远是整份缓冲）。 */
export function countsOf(lines: readonly LogLine[]): Record<LogLevel, number> {
  const counts: Record<LogLevel, number> = { error: 0, warn: 0, info: 0, debug: 0 }
  for (const line of lines) counts[line.level] += 1
  return counts
}

/** 点一下级别按钮：已选中就取消，取消到空 = 不按级别过滤。 */
export function toggleLevel(levels: readonly LogLevel[], level: LogLevel): LogLevel[] {
  return levels.includes(level) ? levels.filter((item) => item !== level) : [...levels, level]
}
