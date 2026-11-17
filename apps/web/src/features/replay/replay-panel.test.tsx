// @vitest-environment jsdom
/**
 * 回放面板的界面契约。
 *
 * 这组测试守的是四条**用户可见**的规则：
 * 1. 打开面板不许自动跑回放（回放会执行用户的 handler，绝不能因为切了个页签就触发）；
 * 2. 表单 → 命令的映射：`from` 文本转 Unix 毫秒、空则是 `null`（`null` 才表示"不限"）；
 * 3. `limit = 0` 在前端拦住（契约里 0 是 `-32002`，不是全量），一条命令都不发；
 * 4. 结果区把「历史 / 重跑」并排画出来，差异标记有中文标签（颜色不是唯一信号），
 *    后端报错时 `role="alert"` 里是**原文**。
 */

import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BotSummary, ReplayEntry, ReplayReport } from '../../generated'
import { useRunStore } from '../../stores/run-store'
import { MAX_RENDERED_ENTRIES, ReplayPanel } from './replay-panel'

const BOT_ID = 'demo-bot'

const dispatchMock = vi.hoisted(() => vi.fn(async (_command: unknown): Promise<unknown> => null))

// 面板只从 `lib/ipc` 拿 `dispatch`；其余导出（类型）在测试里不需要。
vi.mock('../../lib/ipc', () => ({ dispatch: dispatchMock }))

const BOT: BotSummary = {
  botId: BOT_ID,
  name: '演示 bot',
  platform: 'local',
  status: 'stopped',
  updatedAt: 1_700_000_000_000,
}

/** 一条回放 entry（字段与 generated/ReplayEntry.ts 对齐，只覆盖需要的那几项）。 */
function entryOf(overrides: Partial<ReplayEntry>): ReplayEntry {
  return {
    inboundId: 'inbound-1',
    externalId: 'local:default',
    conversationExternalId: 'local:default',
    receivedAt: 1_735_689_600_000,
    status: 'match',
    historical: [],
    replayed: [],
    same: [],
    extra: [],
    missing: [],
    reordered: [],
    error: null,
    ...overrides,
  }
}

/** 三种状态各一条的报告：与 `ipc-mock.ts` 的假报告同形。 */
function reportOf(entries: ReplayEntry[]): ReplayReport {
  return {
    project: 'C:/work/demo',
    bot: BOT_ID,
    from: null,
    limit: null,
    events: entries.length,
    matchedEvents: entries.filter((entry) => entry.status === 'match').length,
    differingEvents: entries.filter((entry) => entry.status === 'diff').length,
    failedEvents: entries.filter((entry) => entry.status === 'error').length,
    same: 1,
    extra: 1,
    missing: 1,
    reordered: 2,
    entries,
  }
}

const MIXED_ENTRIES: ReplayEntry[] = [
  entryOf({
    inboundId: 'inbound-1',
    status: 'match',
    historical: ['在的，有什么可以帮你'],
    replayed: ['在的，有什么可以帮你'],
    same: ['在的，有什么可以帮你'],
  }),
  entryOf({
    inboundId: 'inbound-2',
    status: 'diff',
    receivedAt: 1_735_689_660_000,
    historical: ['收到，我看一下', '查到了：今天有 3 条待办', '稍后给你详细列表'],
    replayed: ['查到了：今天有 3 条待办', '收到，我看一下', '另外提醒你喝水'],
    extra: ['另外提醒你喝水'],
    missing: ['稍后给你详细列表'],
    reordered: [
      { text: '收到，我看一下', historicalIndex: 0, replayedIndex: 1 },
      { text: '查到了：今天有 3 条待办', historicalIndex: 1, replayedIndex: 0 },
    ],
  }),
  entryOf({
    inboundId: 'inbound-3',
    status: 'error',
    receivedAt: 1_735_689_720_000,
    error: 'handler 抛错：TypeError: reply 不是函数',
  }),
]

/** 面板会自己发 `bot.list`；回放命令按用例注入返回值。 */
function stubDispatch(report: ReplayReport | Error = reportOf(MIXED_ENTRIES)): void {
  dispatchMock.mockImplementation(async (command) => {
    const type = (command as { type: string }).type
    if (type === 'bot.list') return [BOT]
    if (type === 'replay_run') {
      if (report instanceof Error) throw report
      return report
    }
    return null
  })
}

/** 取已发出的某类命令（去掉 `type`，只留参数，断言更直观）。 */
function commandsOf(type: string): Record<string, unknown>[] {
  return dispatchMock.mock.calls
    .map(([command]) => command as { type: string } & Record<string, unknown>)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

function botSelect(): HTMLSelectElement {
  return screen.getByLabelText('选择要回放的 bot') as HTMLSelectElement
}

function fromInput(): HTMLInputElement {
  return screen.getByLabelText('起始时间（留空表示不限）') as HTMLInputElement
}

function limitInput(): HTMLInputElement {
  return screen.getByLabelText('条数上限（留空表示全量）') as HTMLInputElement
}

/** 等 bot 清单落地（默认选中第一个 bot），否则提交会被"还没选 bot"拦住。 */
async function waitForBotSelected(): Promise<void> {
  await waitFor(() => expect(botSelect().value).toBe(BOT_ID))
}

beforeEach(() => {
  useRunStore.setState({ botId: '' })
  dispatchMock.mockReset()
  stubDispatch()
})

afterEach(cleanup)

describe('ReplayPanel：不自动跑', () => {
  it('① 打开面板只读 bot 清单，一条 replay_run 都不发', async () => {
    render(<ReplayPanel />)

    await waitFor(() => expect(commandsOf('bot.list')).toHaveLength(1))
    expect(commandsOf('replay_run')).toHaveLength(0)
    expect(screen.getByText(/还没有回放结果/)).toBeTruthy()
  })
})

describe('ReplayPanel：表单 → 命令', () => {
  it('② 从文本转成 Unix 毫秒，limit 转成数字，botId 用选中的 bot', async () => {
    render(<ReplayPanel />)
    await waitForBotSelected()

    fireEvent.change(fromInput(), { target: { value: '2025-01-02T03:04' } })
    fireEvent.change(limitInput(), { target: { value: '2' } })
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    await waitFor(() => expect(commandsOf('replay_run')).toHaveLength(1))
    expect(commandsOf('replay_run')[0]).toEqual({
      botId: BOT_ID,
      // datetime-local 是本地时区文本，`new Date(...)` 用同一套解释，两边必须相等。
      from: new Date('2025-01-02T03:04').getTime(),
      limit: 2,
    })
  })

  it('② 两个输入都留空时 `from` / `limit` 都是 null（不是 0）', async () => {
    render(<ReplayPanel />)
    await waitForBotSelected()

    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    await waitFor(() => expect(commandsOf('replay_run')).toHaveLength(1))
    expect(commandsOf('replay_run')[0]).toEqual({ botId: BOT_ID, from: null, limit: null })
  })
})

describe('ReplayPanel：limit = 0 的前端拦截', () => {
  it('③ 拦住 0：给出中文原因，且不发命令', async () => {
    render(<ReplayPanel />)
    await waitForBotSelected()

    fireEvent.change(limitInput(), { target: { value: '0' } })
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    expect(screen.getByRole('alert').textContent).toContain('-32002')
    expect(screen.getByRole('alert').textContent).toContain('留空表示全量')
    // 拦在表单里 ≠ 发了被拒：报告区根本没出现，命令也一条没发。
    expect(screen.queryByLabelText('回放统计')).toBeNull()
    expect(commandsOf('replay_run')).toHaveLength(0)
  })
})

describe('ReplayPanel：报告渲染', () => {
  it('④ 三种状态各有中文标签，历史与重跑两栏并排', async () => {
    render(<ReplayPanel />)
    await waitForBotSelected()
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    const summary = await screen.findByLabelText('回放统计')
    expect(within(summary).getByText(/^事件/).textContent).toBe('事件 3')
    expect(within(summary).getByText(/^一致/).textContent).toBe('一致 1')
    expect(within(summary).getByText(/^有差异/).textContent).toBe('有差异 1')
    expect(within(summary).getByText(/^出错/).textContent).toBe('出错 1')

    // 状态标签：一致 / 有差异 / 出错三档都在（颜色之外必须有文字）。
    expect(screen.getByText('一致')).toBeTruthy()
    expect(screen.getByText('有差异')).toBeTruthy()
    expect(screen.getByText('出错')).toBeTruthy()

    // 两栏并排：各自按自己的顺序渲染同一批回复，列内顺序也必须不同 ——
    // 只断言"两边都有这句"会漏掉"顺序被排成一样"的退化。
    // 三条 entry 都有这两栏，夹具里第 2 条是 diff 那条，取它的两栏比顺序。
    const diffHistorical = screen.getAllByLabelText('历史回复')[1]
    const diffReplayed = screen.getAllByLabelText('重跑回复')[1]
    if (diffHistorical === undefined || diffReplayed === undefined) {
      throw new Error('第 2 条（diff）必须两栏都在')
    }
    const texts = (column: HTMLElement): (string | null)[] =>
      within(column)
        .getAllByRole('listitem')
        .map((item) => item.textContent)
    expect(texts(diffHistorical)).toEqual([
      '收到，我看一下',
      '查到了：今天有 3 条待办',
      '稍后给你详细列表',
    ])
    expect(texts(diffReplayed)).toEqual(['查到了：今天有 3 条待办', '收到，我看一下', '另外提醒你喝水'])
  })

  it('⑤ extra / missing / reordered 都有可读的中文标记', async () => {
    render(<ReplayPanel />)
    await waitForBotSelected()
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    const marks = await screen.findByLabelText('回复差异')
    expect(within(marks).getByText('多出')).toBeTruthy()
    expect(within(marks).getByText('缺少')).toBeTruthy()
    expect(within(marks).getByText('另外提醒你喝水')).toBeTruthy()
    expect(within(marks).getByText('稍后给你详细列表')).toBeTruthy()
    // 下标从 0 起（契约如此），界面上必须是给人看的 1 起序号。
    expect(within(marks).getByText('历史第 1 条 ↔ 重跑第 2 条')).toBeTruthy()
  })

  it('未知 status 按「有差异」处理，不抛错也不崩', async () => {
    stubDispatch(reportOf([entryOf({ status: 'something-new', historical: ['甲'], replayed: ['乙'] })]))
    render(<ReplayPanel />)
    await waitForBotSelected()
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    expect(await screen.findByText('有差异')).toBeTruthy()
  })

  it('超过 200 条只渲染前 200 条，并明说"只显示前 200 条"', async () => {
    const many = Array.from({ length: MAX_RENDERED_ENTRIES + 3 }, (_unused, index) =>
      entryOf({ inboundId: `inbound-${index}`, historical: [`第 ${index} 条`] }),
    )
    stubDispatch(reportOf(many))
    const view = render(<ReplayPanel />)
    await waitForBotSelected()
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    await screen.findByText(/只显示前 200 条/)
    expect(view.container.querySelectorAll('.replay-entry')).toHaveLength(MAX_RENDERED_ENTRIES)
  })
})

describe('ReplayPanel：后端错误', () => {
  it('⑥ alert 里是后端 message 原文，不做二次包装', async () => {
    const message = 'replay_run: 回放期间 handler 抛错（-32603）'
    stubDispatch(new Error(message))
    render(<ReplayPanel />)
    await waitForBotSelected()
    await userEvent.click(screen.getByRole('button', { name: '开始回放' }))

    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(message))
    // 失败时不留下半份旧报告（否则会误以为上次的结果就是这次的结果）。
    expect(screen.queryByLabelText('回放统计')).toBeNull()
  })
})