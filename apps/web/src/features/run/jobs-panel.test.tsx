// @vitest-environment jsdom
/**
 * 作业与等待面板（`features/run/jobs-panel.tsx`）。
 *
 * 这组测试守的是五条**用户可见**的规则：
 * 1. 两段表格画的是 `bot_jobs` / `bot_waits` 的快照（字段、camelCase、状态中文标签）；
 * 2. 状态是**开放字符串**：不认识的值显示成「未知（原值）」，绝不 throw —— 主机比界面
 *    晚发布时多一个状态不能让面板白屏；
 * 3. 「只看未完成的」真的过滤（终态行消失，空态文案说清怎么找回历史）；
 * 4. 空数组是**合法结果**（不是错误），显示空态而不是红字；
 * 5. 刷新失败**不清空**上一批：错误用 `role="alert"` 显示原文，旧数据仍在屏上。
 */

import { cleanup, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BotSummary, JobView, WaitView } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import {
  JobsPanel,
  formatStamp,
  isActiveJob,
  isPendingWait,
  jobStateLabel,
  waitScopeLabel,
  waitStateLabel,
} from './jobs-panel'

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

const AT = 1_735_689_600_000

function jobOf(overrides: Partial<JobView> = {}): JobView {
  return {
    id: 'job-1',
    name: 'digest',
    state: 'pending',
    attempts: 0,
    budgetMs: 60_000,
    conversation: 'local:default',
    nextAt: AT,
    lastError: null,
    createdAt: AT - 1_000,
    ...overrides,
  }
}

function waitOf(overrides: Partial<WaitView> = {}): WaitView {
  return {
    id: 'wait-1',
    key: 'order-no',
    prompt: '请把订单号发给我',
    scope: 'conversation',
    state: 'pending',
    conversation: 'local:default',
    deadline: AT + 60_000,
    remindersLeft: 1,
    timeoutMs: 60_000,
    createdAt: AT,
    ...overrides,
  }
}

/** 每个用例自己摆数据：`failWith` 非空时两条查询都抛这条原文。 */
const source = {
  jobs: [] as JobView[],
  waits: [] as WaitView[],
  failWith: null as string | null,
}

beforeEach(() => {
  useRunStore.setState({ botId: '', status: 'stopped', logs: [], busy: false, error: null })
  source.jobs = []
  source.waits = []
  source.failWith = null
  dispatchMock.mockClear()
  dispatchMock.mockImplementation(async (raw: unknown) => {
    const command = raw as { type: string }
    if (command.type === 'bot.list') return [BOT]
    if (command.type === 'bot_jobs') {
      if (source.failWith !== null) throw new Error(source.failWith)
      return source.jobs
    }
    if (command.type === 'bot_waits') {
      if (source.failWith !== null) throw new Error(source.failWith)
      return source.waits
    }
    return null
  })
})

afterEach(cleanup)

describe('JobsPanel', () => {
  it('把作业与等待画成两段表格', async () => {
    source.jobs = [
      jobOf(),
      jobOf({ id: 'job-2', name: 'daily-report', state: 'failed', attempts: 3, lastError: 'HBOT_JOBS_FAILED: onJob 缺钩子' }),
    ]
    source.waits = [waitOf(), waitOf({ id: 'wait-2', key: 'address', state: 'fulfilled' })]

    render(<JobsPanel />)

    const jobsTable = await screen.findByRole('table', { name: '作业列表' })
    const jobs = within(jobsTable).getAllByRole('row').slice(1)
    expect(jobs).toHaveLength(2)
    expect(within(jobs[0] as HTMLElement).getByText('digest')).toBeTruthy()
    // 状态是中文标签，不是原始的 `pending`。
    expect(within(jobs[0] as HTMLElement).getByText('排队中')).toBeTruthy()
    // 预算按毫秒原样带单位；`attempts` 不含任何加工。
    expect(within(jobs[0] as HTMLElement).getByText('60,000 ms')).toBeTruthy()
    expect(within(jobs[1] as HTMLElement).getByText('3')).toBeTruthy()
    expect(within(jobs[1] as HTMLElement).getByText('HBOT_JOBS_FAILED: onJob 缺钩子')).toBeTruthy()

    const waitsTable = await screen.findByRole('table', { name: '等待列表' })
    const waits = within(waitsTable).getAllByRole('row').slice(1)
    expect(waits).toHaveLength(2)
    expect(within(waits[0] as HTMLElement).getByText('order-no')).toBeTruthy()
    expect(within(waits[0] as HTMLElement).getByText('等待中')).toBeTruthy()
    expect(within(waits[0] as HTMLElement).getByText('会话里任何人')).toBeTruthy()
    // 剩余重发次数是「还剩几次 + 每轮多久」，两者都影响用户要不要再问一遍。
    expect(within(waits[0] as HTMLElement).getByText('1（每轮 60,000 ms）')).toBeTruthy()

    // 统计条说的是**全量**（不是过滤后）：进行中 1 / 等待中 1。
    expect(screen.getByRole('status', { name: '快照统计' }).textContent).toContain('进行中 1');
    expect(screen.getByRole('status', { name: '快照统计' }).textContent).toContain('等待中 1');
  });

  it('不认识的状态按原样显示而不是抛错', async () => {
    source.jobs = [jobOf({ state: 'paused-for-review' })]
    source.waits = [waitOf({ scope: 'everyone', state: 'deferred' })]

    render(<JobsPanel />)

    expect(await screen.findByText('未知（paused-for-review）')).toBeTruthy()
    expect(await screen.findByText('未知（everyone）')).toBeTruthy()
    expect(await screen.findByText('未知（deferred）')).toBeTruthy()
    // 未知状态仍算未完成 —— 把它归进「已完成」会让人以为那条作业跑完了。
    expect(jobStateLabel('paused-for-review')).toBe('未知（paused-for-review）');
  });

  it('只看未完成会过滤掉终态行', async () => {
    source.jobs = [jobOf(), jobOf({ id: 'job-2', state: 'done' })]
    // 等待那两条都给终态，才能同时验「过滤掉终态」与「过滤后空态」两句文案。
    source.waits = [waitOf({ state: 'expired' })]
    const user = userEvent.setup();

    render(<JobsPanel />)
    const jobsTable = await screen.findByRole('table', { name: '作业列表' })
    expect(within(jobsTable).getAllByRole('row')).toHaveLength(3);

    await user.click(screen.getByLabelText('只看未完成的'))

    // 过滤后只剩表头 + 一条排队中的；终态那条消失，但旁边有文案说清怎么找回。
    expect(within(screen.getByRole('table', { name: '作业列表' })).getAllByRole('row')).toHaveLength(2);
    // 等待那一段全被过滤掉，所以整张表换成一句「取消勾选可看历史」。
    expect(
      await screen.findByText('没有等待中的会话（取消「只看未完成的」可看历史）。'),
    ).toBeTruthy();
    expect(screen.queryByRole('table', { name: '等待列表' })).toBeNull();
  });

  it('空数组是合法结果：给空态文案而不是红字', async () => {
    render(<JobsPanel />)

    expect(
      await screen.findByText(
        '这个 bot 还没有作业记录：清单里声明 capabilities.jobs 之后，ctx.jobs.enqueue 才会写行。',
      ),
    ).toBeTruthy();
    expect(
      await screen.findByText(
        '这个 bot 还没有等待记录：ctx.session.prompt 会写一行，收到回答后变「已收到回答」。',
      ),
    ).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('刷新失败时保留上一批数据并显示原文错误', async () => {
    source.jobs = [jobOf({ name: 'digest' })]
    const user = userEvent.setup();

    render(<JobsPanel />)
    expect(await screen.findByText('digest')).toBeTruthy();

    source.failWith = 'HBOT_JOBS_UNAVAILABLE: 宿主没有装配作业队列';
    await user.click(screen.getByRole('button', { name: '立即刷新' }));

    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toContain('HBOT_JOBS_UNAVAILABLE');
    });
    // 旧数据仍在：一屏「最后一次成功 + 一条错误」比空屏有用。
    expect(screen.getByText('digest')).toBeTruthy();
  });
});

describe('纯函数', () => {
  it('isActiveJob 只认 pending / running，未知值按未完成算', () => {
    expect(isActiveJob('pending')).toBe(true);
    expect(isActiveJob('running')).toBe(true);
    expect(isActiveJob('done')).toBe(false);
    expect(isActiveJob('failed')).toBe(false);
    expect(isActiveJob('cancelled')).toBe(false);
    expect(isActiveJob('whatever')).toBe(true);
  });

  it('isPendingWait 只认 pending', () => {
    expect(isPendingWait('pending')).toBe(true);
    expect(isPendingWait('fulfilled')).toBe(false);
    expect(isPendingWait('expired')).toBe(false);
  });

  it('标签函数对已知值给中文，对未知值带原值', () => {
    expect(jobStateLabel('running')).toBe('执行中');
    expect(waitStateLabel('fulfilled')).toBe('已收到回答');
    expect(waitScopeLabel('sender')).toBe('仅本条消息的发送者');
    expect(jobStateLabel('nope')).toBe('未知（nope）');
  });

  it('formatStamp 是 MM-DD HH:mm:ss', () => {
    // 用本地时间的构造器，免得断言依赖运行时区。
    const at = new Date(2026, 0, 2, 3, 4, 5).getTime();
    expect(formatStamp(at)).toBe('01-02 03:04:05');
  });
});
