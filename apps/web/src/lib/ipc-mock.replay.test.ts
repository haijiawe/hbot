/**
 * mock 传输里的回放命令（`lib/ipc-mock.ts` 的 `replay_run`）。
 *
 * 为什么值得测：浏览器 dev / 没有桌面壳时，回放面板唯一能看到的报告就是这份假数据。
 * 这里守两件事：报告必须**真的有差异**（否则面板的三种状态永远没机会显示），
 * 以及 `limit` 的两条边界与核心一致（正整数截断、`0` 是 `-32002` 而不是"全量"）。
 */

import { describe, expect, it } from 'vitest'
import type { FixtureRunReport, ReplayReport, SavedFixture } from '../generated'
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

/** 夹具命令的包装：mock 的返回类型是全命令联合，这里按用例收窄。 */
async function saveFixture(path: string, limit: number | null = null): Promise<SavedFixture> {
  return (await mockDispatch({
    type: 'replaySaveFixture',
    botId: BOT_ID,
    from: null,
    limit,
    path,
  })) as SavedFixture
}

async function runFixture(botId: string, path: string): Promise<FixtureRunReport> {
  return (await mockDispatch({ type: 'fixtureRun', botId, path })) as FixtureRunReport
}

describe('mockDispatch：夹具的保存与重跑', () => {
  it('存进去再跑：条目与回放同一批，回复可预测，出错那条仍然出错', async () => {
    const saved = await saveFixture('.hbot/fixtures/mock-roundtrip.json')
    // 事件数与 `replay_run` 的全量报告一致（mock 没有库，两处必须是同一批历史）。
    expect(saved.path).toBe('.hbot/fixtures/mock-roundtrip.json')
    expect(saved.events).toBe(3)

    const report = await runFixture(BOT_ID, saved.path)

    expect(report.path).toBe('.hbot/fixtures/mock-roundtrip.json')
    expect(report.events).toBe(3)
    expect(report.entries.map((entry) => entry.index)).toEqual([0, 1, 2])
    expect(report.entries[0]?.replies).toEqual(['pong:在的，有什么可以帮你'])
    // 第 3 条是历史里就没有回复的那条：重跑时 handler 抛错，一条出错不影响其余。
    expect(report.entries[2]?.error).toContain('handler 抛错')
    expect(report.replies).toBe(2)
    expect(report.errors).toBe(1)
  })

  it('路径守卫：跳出项目、不是 .json 都拒绝', async () => {
    // 复用 mock 已有的"项目内"判据（它自己带文案，不重复 -32002 前缀）。
    await expect(saveFixture('../escape.json')).rejects.toThrow('路径必须位于项目内')
    await expect(saveFixture('.hbot/fixtures/not-json.txt')).rejects.toThrow('.json')
    // 允许 `.hbot/`：夹具的默认位置就在那儿（与 `assertWritable` 的"不写 .hbot"不同）。
    await expect(saveFixture('.hbot/fixtures/allowed.json')).resolves.toBeTruthy()
  })

  it('`limit` 截断与 `0` 的拒绝与核心一致', async () => {
    const saved = await saveFixture('.hbot/fixtures/mock-limit.json', 2)
    expect(saved.events).toBe(2)
    await expect(saveFixture('.hbot/fixtures/mock-zero.json', 0)).rejects.toThrow('-32002')
  })

  it('读不到的那份夹具直接报错，不静默造一份假的', async () => {
    await expect(runFixture(BOT_ID, '.hbot/fixtures/never-saved.json')).rejects.toThrow(
      '读不了夹具文件',
    )
  })

  it('重跑不查库：库里没有的 bot 也能跑（`botId` 只当归属）', async () => {
    const saved = await saveFixture('.hbot/fixtures/mock-other-bot.json')
    const report = await runFixture('nobody', saved.path)

    expect(report.events).toBe(saved.events)
    expect(report.entries[0]?.replies.length).toBeGreaterThan(0)
  })
})