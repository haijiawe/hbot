/**
 * 作业看板的纯函数：把 `bot_jobs` 的只读快照分成三列。
 *
 * 为什么在 Web 侧分列：核心只给行（`BotJobs` 是只读快照），"哪些算在途"是显示口径，
 * 不是业务规则；把它写成纯函数就能直接喂字符串/对象测试，不必起核心。
 */

import type { JobView } from '../generated'

/** 看板列的唯一真源：新列只加在这里，组件按它渲染。 */
export interface BoardColumn {
  id: 'queued' | 'running' | 'finished'
  label: string
  /** 这一列收哪些 `state` 原值（不认识的状态一律进 finished，见 `groupJobs`）。 */
  states: readonly string[]
}

export const BOARD_COLUMNS: readonly BoardColumn[] = [
  { id: 'queued', label: '排队', states: ['pending'] },
  { id: 'running', label: '运行中', states: ['running'] },
  { id: 'finished', label: '已结束', states: ['done', 'failed', 'cancelled'] },
]

/** 三种归宿：在途（排队 + 运行中）、等结果（排队）、已结束。 */
export type BoardGroup = Record<BoardColumn['id'], JobView[]>

/**
 * 按列分组，保持输入顺序（核心已经按入队时刻新的在前）。
 *
 * 不认识的状态进 `finished` 而不是丢掉：核心的状态集合会继续长（`hbot_core::bot_jobs`），
 * 静默丢行会让用户以为作业消失了，而它其实还在库里。
 */
export function groupJobs(jobs: readonly JobView[]): BoardGroup {
  const group: BoardGroup = { queued: [], running: [], finished: [] }
  for (const job of jobs) {
    const column = BOARD_COLUMNS.find((item) => item.states.includes(job.state))
    group[column ? column.id : 'finished'].push(job)
  }
  return group
}

/** 在途条数（排队 + 运行中）：概览与看板标题上的 pill 都用它。 */
export function inFlightCount(jobs: readonly JobView[]): number {
  const group = groupJobs(jobs)
  return group.queued.length + group.running.length
}

/** 状态中文名；不认识的原样返回（宁可显示 pending2 也不显示「未知」把线索藏起来）。 */
const STATE_LABELS: Readonly<Record<string, string>> = {
  pending: '排队中',
  running: '运行中',
  done: '已完成',
  failed: '失败',
  cancelled: '已取消',
}

export function jobStateLabel(state: string): string {
  return STATE_LABELS[state] ?? state
}

/** 结束态的成败：卡片据此决定标签色调（`ok` 绿 / `bad` 红 / `neutral` 灰）。 */
export function jobOutcome(state: string): 'ok' | 'bad' | 'neutral' {
  if (state === 'done') return 'ok'
  if (state === 'failed') return 'bad'
  return 'neutral'
}

/** 毫秒时间戳转本地 `HH:MM:SS`；0 或非法值给空串（面板不显示 1970 年）。 */
export function clockOf(at: number): string {
  if (!Number.isFinite(at) || at <= 0) return ''
  const date = new Date(at)
  const pad = (value: number) => String(value).padStart(2, '0')
  return pad(date.getHours()) + ':' + pad(date.getMinutes()) + ':' + pad(date.getSeconds())
}
