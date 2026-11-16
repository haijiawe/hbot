/**
 * 日志缓冲的**真实**语义（`stores/run-store.ts`）：落地「10k 行不卡」这条出口标准时
 * 最容易搞混的两个上限。
 *
 * 两个上限不是一回事：
 * - `MAX_LOG_LINES = 10_000` —— **内存环形缓冲**的上界（`run-store.ts:21`）。`run.log`
 *   事件进来后按它裁剪，所以面板里最多同时有 10,000 行。
 * - `LOG_TAIL_LIMIT = 500` —— **首屏从 SQLite 补读**的行数（`run-store.ts:24`），
 *   只影响「刚打开界面时显示多少行历史」，不是内存上限。
 *
 * 这条测试钉住三件事，防的是「以为上限是 500 / 以为超出的行会留着」这类误读：
 * 1. 上限是 10,000（不是 500），且超出后丢的是**最旧**的行；
 * 2. 10000 条事件全部到达时，留下的是**最后** 10,000 条、顺序与到达顺序一致；
 * 3. 吞 10,000 条事件的耗时在明确标注的**防回归阈值**内（见下方常量注释）。
 *
 * 它 **不是** 渲染测：这里量的是 Node 进程里的 store 归约（zustand `set` + 数组拷贝），
 * 没有 DOM、没有 React、没有浏览器合成。真机「10k 行不卡」需要浏览器里量帧时间。
 */

import { beforeEach, describe, expect, it } from 'vitest'
import type { CoreEvent } from '../lib/ipc'
import { LOG_TAIL_LIMIT, MAX_LOG_LINES, useRunStore } from './run-store'

/**
 * 防回归阈值，**不是性能承诺**。
 *
 * 本机（Windows / Node 24 / vitest forks）实测：10,000 条合计 **104–167 ms**（3 轮），
 * 满缓冲上再追加 1,000 条 **32–61 ms**。给 2 s 是留 12–20× 余量，专门避免 CI 抖动误红。
 * 阈值存在的意义只是「有人把 append 改成每次全量深拷贝 / 排序」时能红一下，
 * 不要拿它当性能读数引用。
 */
const FILL_BUDGET_MS = 2_000
/** 同上（满缓冲上追加的防回归阈值）。 */
const TOPUP_BUDGET_MS = 2_000

const BOT_ID = 'bot-1'
const BASE_AT = 1_700_000_000_000

/** 造一条 `run.log` 事件。 */
function runLog(index: number): CoreEvent {
  return {
    type: 'run.log',
    botId: BOT_ID,
    level: 'info',
    line: `line ${index}`,
    at: BASE_AT + index,
  }
}

function reset(): void {
  useRunStore.setState({
    botId: BOT_ID,
    status: 'running',
    logs: [],
    busy: false,
    error: null,
    hydrated: true,
    live: true,
  })
}

/** 推 `count` 条日志（`offset` 只是用来造不重复的文本）。 */
function push(count: number, offset = 0): void {
  for (let index = offset; index < offset + count; index += 1) {
    useRunStore.getState().ingestEvent(runLog(index))
  }
}

describe('runStore 日志上限', () => {
  beforeEach(reset)

  it('内存上限是 10_000 行，500 是"首屏补读行数"而不是缓冲上限', () => {
    expect(MAX_LOG_LINES).toBe(10_000)
    expect(LOG_TAIL_LIMIT).toBe(500)
    // 两者职责不同：补读的 500 行只是缓冲的起始内容，之后事件继续往里推。
    expect(LOG_TAIL_LIMIT).toBeLessThan(MAX_LOG_LINES)
  })

  it('推 10_000 条：正好留 10_000 行，顺序与到达顺序一致（耗时在防回归阈值内）', () => {
    const started = performance.now()
    push(MAX_LOG_LINES)
    const elapsed = performance.now() - started

    const logs = useRunStore.getState().logs
    expect(logs).toHaveLength(MAX_LOG_LINES)
    expect(logs[0]?.message).toBe('line 0')
    expect(logs[MAX_LOG_LINES - 1]?.message).toBe(`line ${MAX_LOG_LINES - 1}`)
    // 顺序：第 i 行必须是第 i 条事件（不是被排序过、也没有空洞）。
    const outOfOrder = logs.filter((entry, index) => entry.message !== `line ${index}`)
    expect(outOfOrder).toEqual([])
    // `at` 随事件原样保留（面板按它显示时间）。
    expect(logs[0]?.at).toBe(BASE_AT)
    expect(logs[MAX_LOG_LINES - 1]?.at).toBe(BASE_AT + MAX_LOG_LINES - 1)
    // 裁剪发生在"满了之后"，所以这 10_000 次里没有一次命中丢旧行分支。
    expect(elapsed).toBeLessThan(FILL_BUDGET_MS)
  })

  it('超出上限：只保留最后 10_000 条，丢的是最旧的、顺序不乱', () => {
    push(MAX_LOG_LINES + 500)

    const logs = useRunStore.getState().logs
    expect(logs).toHaveLength(MAX_LOG_LINES)
    // 前 500 条（line 0…499）被丢掉了。
    expect(logs[0]?.message).toBe('line 500')
    expect(logs[logs.length - 1]?.message).toBe(`line ${MAX_LOG_LINES + 500 - 1}`)
    const outOfOrder = logs.filter((entry, index) => entry.message !== `line ${index + 500}`)
    expect(outOfOrder).toEqual([])
  })

  it('满缓冲上再推 1_000 条：长度不变、仍是最后 10_000 条（耗时在防回归阈值内）', () => {
    push(MAX_LOG_LINES)
    const started = performance.now()
    push(1_000, MAX_LOG_LINES)
    const elapsed = performance.now() - started

    const logs = useRunStore.getState().logs
    expect(logs).toHaveLength(MAX_LOG_LINES)
    expect(logs[0]?.message).toBe('line 1000')
    expect(logs[logs.length - 1]?.message).toBe(`line ${MAX_LOG_LINES + 1_000 - 1}`)
    expect(elapsed).toBeLessThan(TOPUP_BUDGET_MS)
  })
})