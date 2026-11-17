/**
 * mock 传输里的回放命令（`lib/ipc-mock.ts` 的 `replay_run`）。
 *
 * 为什么值得测：浏览器 dev / 没有桌面壳时，回放面板唯一能看到的报告就是这份假数据。
 * 这里守两件事：报告必须**真的有差异**（否则面板的三种状态永远没机会显示），
 * 以及 `limit` 的两条边界与核心一致（正整数截断、`0` 是 `-32002` 而不是"全量"）。
 */

import { describe, expect, it } from 'vitest'
import type { ReplayReport } from '../generated'
import { mockDispatch } from './ipc-mock'

const BOT_ID = 'demo-bot'

/** `mockDispatch` 的返回类型是全命令的联合，这里按用例收窄到回放报告。 */
async function replayOf(from: number | null, limit: number | null): Promise<ReplayReport> {
  return (await mockDispatch({ type: 'replay_run', botId: BOT_ID, from, limit })) as ReplayReport
}

describe('mockDispatch：replay_run', () => {
  it('报告里三种状态各一条，diff 那条带 extra / missing / reordered', async () => {
    const report = await replayOf(null, null)

    expect(report.entries.map((entry) => entry.status)).toEqual(['match', 'diff', 'error'])
    const diff = report.entries.find((entry) => entry.status === 'diff')
    if (diff === undefined) throw new Error('mock 报告里必须有一条 diff')
    expect(diff.extra.length).toBeGreaterThan(0)
    expect(diff.missing.length).toBeGreaterThan(0)
    expect(diff.reordered.length).toBeGreaterThan(0)

    const failed = report.entries.find((entry) => entry.status === 'error')
    expect(failed?.error).toBeTruthy()

    // 汇总计数与逐条内容对得上（面板顶部的统计行直接读它们）。
    expect(report.events).toBe(3)
    expect(report.matchedEvents).toBe(1)
    expect(report.differingEvents).toBe(1)
    expect(report.failedEvents).toBe(1)
    expect(report.extra).toBe(diff.extra.length)
    expect(report.missing).toBe(diff.missing.length)
    expect(report.reordered).toBe(diff.reordered.length)
  })

  it('`limit` 按条数截断，并把生效的范围回显在报告里', async () => {
    const report = await replayOf(null, 2)

    expect(report.entries).toHaveLength(2)
    expect(report.events).toBe(2)
    expect(report.limit).toBe(2)
  })

  it('`limit = 0` 按 -32002 拒绝（0 不是"全量"）', async () => {
    await expect(
      mockDispatch({ type: 'replay_run', botId: BOT_ID, from: null, limit: 0 }),
    ).rejects.toThrow('-32002')
  })

  it('`from` 按 receivedAt 下限过滤：取第二条的时间戳就只剩后两条', async () => {
    const all = await replayOf(null, null)
    const second = all.entries[1]
    if (second === undefined) throw new Error('mock 报告至少要有两条才能测 from')

    const filtered = await replayOf(second.receivedAt, null)

    expect(filtered.entries.map((entry) => entry.inboundId)).toEqual(
      all.entries.slice(1).map((entry) => entry.inboundId),
    )
    expect(filtered.from).toBe(second.receivedAt)
  })

  it('未知 bot 被拒绝，不返回一份"看起来成功"的空报告', async () => {
    await expect(
      mockDispatch({ type: 'replay_run', botId: 'nobody', from: null, limit: null }),
    ).rejects.toThrow('未知 bot')
  })
})