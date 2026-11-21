/**
 * 日志筛选的纯函数（`lib/log-filter.ts`）。
 *
 * 这几条钉的是「筛选不许改变数据、只改变视图」：`countsOf` 统计的永远是整份缓冲
 * （工具栏徽标上的数字不能随筛选变化，否则用户会以为日志被删了），
 * 而 `filterLogs` 只做投影、不改顺序。
 */

import { describe, expect, it } from 'vitest'
import type { LogLine } from './ipc'
import { LOG_LEVELS, LOG_LEVEL_LABELS, countsOf, filterLogs, toggleLevel } from './log-filter'

function line(level: LogLine['level'], message: string, at = 1): LogLine {
  return { level, source: 'bot', message, at }
}

const SAMPLE: LogLine[] = [
  line('info', '启动完成', 1),
  line('error', 'HBOT_JOBS_FAILED: 清单里没有 onJob 钩子', 2),
  line('warn', '重试第 1 次', 3),
  line('debug', 'SWC 编译 12ms', 4),
]

describe('filterLogs', () => {
  it('没有任何条件时原样返回，且保持顺序', () => {
    expect(filterLogs(SAMPLE, [], '')).toEqual(SAMPLE)
    expect(filterLogs(SAMPLE, [], '   ')).toEqual(SAMPLE)
  })

  it('级别是并集：选中两档就同时留两档', () => {
    const out = filterLogs(SAMPLE, ['error', 'warn'], '')
    expect(out.map((item) => item.level)).toEqual(['error', 'warn'])
  })

  it('关键词按子串、大小写不敏感地匹配 message', () => {
    expect(filterLogs(SAMPLE, [], 'hbot_jobs').map((item) => item.message)).toEqual([
      'HBOT_JOBS_FAILED: 清单里没有 onJob 钩子',
    ])
    expect(filterLogs(SAMPLE, [], '重试')).toHaveLength(1)
  })

  it('级别与关键词是交集', () => {
    expect(filterLogs(SAMPLE, ['info'], 'hbot')).toEqual([])
  })

  it('不匹配时返回空数组而不是抛错（面板据此显示"没有匹配的日志"）', () => {
    expect(filterLogs(SAMPLE, [], '不存在的关键词')).toEqual([])
  })
})

describe('countsOf', () => {
  it('统计整份缓冲的四档条数（不随筛选变化）', () => {
    expect(countsOf(SAMPLE)).toEqual({ error: 1, warn: 1, info: 1, debug: 1 })
    expect(countsOf([])).toEqual({ error: 0, warn: 0, info: 0, debug: 0 })
  })
})

describe('toggleLevel', () => {
  it('没选中就加、已选中就减，结果为空表示不按级别过滤', () => {
    expect(toggleLevel([], 'warn')).toEqual(['warn'])
    expect(toggleLevel(['warn'], 'warn')).toEqual([])
    expect(toggleLevel(['error'], 'debug')).toEqual(['error', 'debug'])
    expect(toggleLevel(['error', 'warn'], 'warn')).toEqual(['error'])
  })

  it('不修改传入的数组', () => {
    const levels: LogLine['level'][] = ['info']
    toggleLevel(levels, 'warn')
    expect(levels).toEqual(['info'])
  })
})

describe('常量', () => {
  it('四档级别与标签一一对应（trace 已在 store 折成 debug）', () => {
    expect(LOG_LEVELS).toEqual(['error', 'warn', 'info', 'debug'])
    expect(Object.keys(LOG_LEVEL_LABELS).sort()).toEqual([...LOG_LEVELS].sort())
  })
})
