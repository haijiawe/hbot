import { describe, expect, it } from 'vitest'
import type { JobView } from '../generated'
import { BOARD_COLUMNS, clockOf, groupJobs, inFlightCount, jobOutcome, jobStateLabel } from './job-board'

/** 一条作业的测试替身：只关心看板用到的字段，其余给稳定默认值。 */
function job(id: string, state: string, overrides: Partial<JobView> = {}): JobView {
  return {
    id: id as JobView['id'],
    name: 'tick',
    state,
    attempts: 1,
    budgetMs: 60_000,
    conversation: null,
    nextAt: 0,
    lastError: null,
    createdAt: 1_700_000_000_000,
    ...overrides,
  }
}

describe('groupJobs', () => {
  it('按三列分组且保持输入顺序', () => {
    const group = groupJobs([job('a', 'pending'), job('b', 'running'), job('c', 'done'), job('d', 'pending')])
    expect(group.queued.map((item) => item.id)).toEqual(['a', 'd'])
    expect(group.running.map((item) => item.id)).toEqual(['b'])
    expect(group.finished.map((item) => item.id)).toEqual(['c'])
  })

  it('三种终态都进已结束列', () => {
    const group = groupJobs([job('a', 'done'), job('b', 'failed'), job('c', 'cancelled')])
    expect(group.finished).toHaveLength(3)
    expect(group.queued).toHaveLength(0)
    expect(group.running).toHaveLength(0)
  })

  it('不认识的状态进已结束列，不丢行', () => {
    const group = groupJobs([job('a', 'quarantined')])
    expect(group.finished).toHaveLength(1)
    const total = BOARD_COLUMNS.reduce((sum, column) => sum + group[column.id].length, 0)
    expect(total).toBe(1)
  })

  it('空输入给三个空列', () => {
    const group = groupJobs([])
    expect(group).toEqual({ queued: [], running: [], finished: [] })
  })
})

describe('inFlightCount', () => {
  it('只数排队与运行中', () => {
    expect(inFlightCount([job('a', 'pending'), job('b', 'running'), job('c', 'done')])).toBe(2)
  })

  it('全结束是 0', () => {
    expect(inFlightCount([job('a', 'failed')])).toBe(0)
  })
})

describe('jobStateLabel', () => {
  it('认识的给中文', () => {
    expect(jobStateLabel('pending')).toBe('排队中')
    expect(jobStateLabel('running')).toBe('运行中')
    expect(jobStateLabel('done')).toBe('已完成')
    expect(jobStateLabel('failed')).toBe('失败')
    expect(jobStateLabel('cancelled')).toBe('已取消')
  })

  it('不认识的原样返回（不显示「未知」把线索藏起来）', () => {
    expect(jobStateLabel('quarantined')).toBe('quarantined')
  })
})

describe('jobOutcome', () => {
  it('只有 done 是 ok、failed 是 bad、其余 neutral', () => {
    expect(jobOutcome('done')).toBe('ok')
    expect(jobOutcome('failed')).toBe('bad')
    expect(jobOutcome('running')).toBe('neutral')
    expect(jobOutcome('quarantined')).toBe('neutral')
  })
})

describe('clockOf', () => {
  it('0 与非有限值给空串（不显示 1970 年）', () => {
    expect(clockOf(0)).toBe('')
    expect(clockOf(Number.NaN)).toBe('')
    expect(clockOf(-1)).toBe('')
  })

  it('给本地时分秒，两位补零', () => {
    const at = new Date(2026, 10, 21, 9, 5, 7).getTime()
    expect(clockOf(at)).toBe('09:05:07')
  })
})
