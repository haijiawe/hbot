/**
 * 作业与等待面板（底部面板的「作业」页签，lazy chunk）。
 *
 * 两条能力在核心侧早就落地了（`C4.jobs` 的 `ctx.jobs.enqueue`、`C2.session` 的
 * `ctx.session.prompt`），但此前**界面完全看不到**：作业只写库，等待只写库，用户只能
 * 从 run log 里猜。这个面板把 `bot_jobs` / `bot_waits` 两张表各拉一段最近的行画出来。
 *
 * 三条界面纪律：
 *
 * 1. **只读**：`bot_jobs` / `bot_waits` 是快照查询，不派发作业、不改状态、不结算超时、
 *    也不重发提示。所以面板可以放心轮询 —— 轮询一份只读快照不会让 bot 多跑一次。
 * 2. **只在挂载时轮询**：底部面板切页签时 `panelBody()` 只渲染当前页签，切走即卸载、
 *    定时器跟着清掉。别把轮询搬到 store 里，那会让「没在看」也一直打库。
 * 3. **状态是开放字符串**：`JobView.state` / `WaitView.state` / `WaitView.scope` 都不是枚举
 *    （状态集合还会长）。不认识的值显示成「未知（原值）」，**绝不能 throw** ——
 *    主机比界面晚发布时，界面不能因为多了一个状态就白屏。
 */

import { useEffect, useMemo, useState } from 'react'
import { errorMessage, formatClock, formatCount } from '../../lib/format'
import type { BotSummary, JobView, WaitView } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'

/**
 * 轮询间隔（毫秒）。
 *
 * 作业与等待的推进节奏由核心的 ticker 决定（默认 1 秒一拍），面板比它慢一点即可：
 * 画得快也不会让作业跑得快，只会多打两次库。
 */
export const POLL_INTERVAL_MS = 2_000

/**
 * 每次取多少行。
 *
 * 核心的上限是 `MAX_VIEW_LIMIT = 200`（超了按上限算、不报错）；这里取 50 是因为面板
 * 是「最近发生了什么」的窗口，不是历史归档 —— 真要看更多，日志面板更合适。
 */
export const VIEW_LIMIT = 50

const BOT_SELECT_ID = 'jobs-bot'
const ACTIVE_ONLY_ID = 'jobs-active-only'

/**
 * 作业状态 → 中文标签。**不是穷举开关**：不认识的键走 `unknownLabel()`。
 *
 * 键与 `hbot_core::JobState::as_str()` 一一对应（`crates/hbot-core/src/bot_jobs.rs`）。
 */
const JOB_STATE_LABEL: Readonly<Record<string, string>> = {
  pending: '排队中',
  running: '执行中',
  done: '已完成',
  failed: '失败',
  cancelled: '已取消',
}

/** 等待状态 → 中文标签；键与 `WaitState::as_str()` 对应。 */
const WAIT_STATE_LABEL: Readonly<Record<string, string>> = {
  pending: '等待中',
  fulfilled: '已收到回答',
  expired: '已超时',
  cancelled: '已取消',
}

/** 等待范围 → 中文标签；键与 `WaitScope::as_str()` 对应。 */
const WAIT_SCOPE_LABEL: Readonly<Record<string, string>> = {
  sender: '仅本条消息的发送者',
  conversation: '会话里任何人',
}

/** 不认识的开放字符串怎么显示：既说「我不认识」，又把原值原样留给用户。 */
function unknownLabel(raw: string): string {
  return `未知（${raw}）`
}

export function jobStateLabel(state: string): string {
  return JOB_STATE_LABEL[state] ?? unknownLabel(state)
}

export function waitStateLabel(state: string): string {
  return WAIT_STATE_LABEL[state] ?? unknownLabel(state)
}

export function waitScopeLabel(scope: string): string {
  return WAIT_SCOPE_LABEL[scope] ?? unknownLabel(scope)
}

/** 看到这三个字就说明它不会自己再动了。未知值**不在**这里 —— 见 `isActiveJob`。 */
const JOB_TERMINAL_STATES: ReadonlySet<string> = new Set(['done', 'failed', 'cancelled'])

/** 等待的终态；`pending` 之外的已知值。 */
const WAIT_TERMINAL_STATES: ReadonlySet<string> = new Set(['fulfilled', 'expired', 'cancelled'])

/**
 * 「这个作业还在推进吗」。
 *
 * `pending`（排队等下一拍）与 `running`（正在 isolate 里跑）都算；其余三个是终态。
 * 未知状态一律**当成未完成**：把它藏进「已完成」里会让人以为那条作业跑完了。
 */
export function isActiveJob(state: string): boolean {
  return !JOB_TERMINAL_STATES.has(state)
}

/** 等待还没结案（`pending`）；其余都是终态，未知值同样按未完成算。 */
export function isPendingWait(state: string): boolean {
  return !WAIT_TERMINAL_STATES.has(state)
}

/**
 * 时间戳 → `MM-DD HH:mm:ss`。
 *
 * 不直接用 `toLocaleString`：长度不定会让表格列宽乱跳，而且一行里要塞两三个时间戳。
 * 跨年时信息不足是可接受的取舍 —— 这里的窗口是最近几十条。
 */
export function formatStamp(at: number): string {
  const date = new Date(at)
  const month = String(date.getMonth() + 1).padStart(2, '0')
  const day = String(date.getDate()).padStart(2, '0')
  return `${month}-${day} ${formatClock(at)}`
}

export function JobsPanel() {
  // run-store 的 `botId` 是「当前选中的 bot」：清单读不到时拿它兜底（与回放面板同一条）。
  const currentBotId = useRunStore((state) => state.botId)

  const [bots, setBots] = useState<BotSummary[]>([])
  const [botId, setBotId] = useState(currentBotId)
  const [jobs, setJobs] = useState<JobView[]>([])
  const [waits, setWaits] = useState<WaitView[]>([])
  const [activeOnly, setActiveOnly] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [refreshedAt, setRefreshedAt] = useState<number | null>(null)
  // 手动刷新只改这个数：让下面的 effect 重跑一次（比再写一份 load 逻辑干净）。
  const [reloadToken, setReloadToken] = useState(0)

  // 只读一次清单；组件卸载后到达的响应丢掉（避免对已卸载组件 setState）。
  useEffect(() => {
    let disposed = false
    void dispatch({ type: 'bot.list' })
      .then((list) => {
        if (!disposed) setBots(list)
      })
      .catch(() => {
        // 清单读不到不算致命：下面退回 run-store 的当前 bot，面板照常能查。
        if (!disposed) setBots([])
      })
    return () => {
      disposed = true
    }
  }, []);

  // 默认选中：先等清单（第一个 bot），清单为空再用 run-store 的当前 bot。
  useEffect(() => {
    if (botId !== '') return
    const first = bots[0]
    if (first !== undefined) {
      setBotId(first.botId)
      return
    }
    if (currentBotId !== '') setBotId(currentBotId)
  }, [bots, botId, currentBotId])

  // 轮询：挂载即拉一次，之后每 `POLL_INTERVAL_MS` 一次；`botId` 变了立刻换数据源。
  useEffect(() => {
    if (botId === '') {
      // 还没选中 bot：清掉上一批，别让 A 的作业挂在 B 的名字下面。
      setJobs([])
      setWaits([])
      return
    }
    let disposed = false
    let inFlight = false
    async function load(): Promise<void> {
      // 慢查询时不要叠加：上一拍还没回来就跳过这一拍（面板不需要补课）。
      if (inFlight) return
      inFlight = true
      try {
        const [nextJobs, nextWaits] = await Promise.all([
          dispatch({ type: 'bot_jobs', botId, limit: VIEW_LIMIT }),
          dispatch({ type: 'bot_waits', botId, limit: VIEW_LIMIT }),
        ])
        if (disposed) return
        setJobs(nextJobs)
        setWaits(nextWaits)
        setError(null)
        setRefreshedAt(Date.now())
      } catch (failure) {
        // 失败**不清空**上一批：一屏「最后一次成功的数据 + 一条错误」比空屏有用；
        // 但也不假装成功 —— `refreshedAt` 不动，用户能看出这是旧数据。
        if (!disposed) setError(errorMessage(failure))
      } finally {
        inFlight = false
      }
    }
    void load()
    const timer = setInterval(() => {
      void load()
    }, POLL_INTERVAL_MS)
    return () => {
      disposed = true
      clearInterval(timer)
    }
  }, [botId, reloadToken])

  const options = useMemo(() => {
    if (bots.length > 0) {
      return bots.map((bot) => ({ id: bot.botId, label: `${bot.name} · ${bot.platform}` }))
    }
    // 清单不可用：把兜底的 bot 也列成一个选项，否则 `<select>` 的 value 会指向空。
    return botId === '' ? [] : [{ id: botId, label: botId }]
  }, [bots, botId])

  const shownJobs = activeOnly ? jobs.filter((job) => isActiveJob(job.state)) : jobs
  const shownWaits = activeOnly ? waits.filter((wait) => isPendingWait(wait.state)) : waits
  const activeJobs = jobs.filter((job) => isActiveJob(job.state)).length
  const pendingWaits = waits.filter((wait) => isPendingWait(wait.state)).length

  return (
    <section className="jobs-panel" aria-label="作业与等待">
      <p className="panel-hint jobs-panel__hint">
        只读快照：这里不会派发作业、不会结算超时、也不会重发提示。每 
        {POLL_INTERVAL_MS / 1000} 秒刷新一次。
      </p>

      <div className="jobs-panel__toolbar">
        <div className="jobs-panel__field">
          <label className="jobs-panel__label" htmlFor={BOT_SELECT_ID}>
            bot
          </label>
          <select
            id={BOT_SELECT_ID}
            className="jobs-panel__select"
            aria-label="选择要查看的 bot"
            value={botId}
            onChange={(event) => setBotId(event.target.value)}
          >
            {options.length === 0 ? <option value="">（还没有 bot）</option> : null}
            {options.map((option) => (
              <option key={option.id} value={option.id}>
                {option.label}
              </option>
            ))}
          </select>
        </div>

        <label className="jobs-panel__check" htmlFor={ACTIVE_ONLY_ID}>
          <input
            id={ACTIVE_ONLY_ID}
            type="checkbox"
            checked={activeOnly}
            onChange={(event) => setActiveOnly(event.target.checked)}
          />
          只看未完成的
        </label>

        <button
          type="button"
          className="button"
          onClick={() => setReloadToken((token) => token + 1)}
          aria-label="立即刷新"
        >
          刷新
        </button>

        <span className="jobs-panel__meta" role="status" aria-label="快照统计">
          作业 {formatCount(jobs.length)}（进行中 {formatCount(activeJobs)}）· 等待{' '}
          {formatCount(waits.length)}（等待中 {formatCount(pendingWaits)}）
          {refreshedAt === null ? null : ` · ${formatStamp(refreshedAt)} 刷新`}
        </span>
      </div>

      {error === null ? null : (
        <p className="inline-error jobs-panel__error" role="alert">
          {error}
        </p>
      )}

      <section className="jobs-panel__section" aria-label="后台作业">
        <h3 className="jobs-panel__title">后台作业</h3>
        {shownJobs.length === 0 ? (
          <p className="panel-empty" role="status">
            {jobs.length === 0
              ? '这个 bot 还没有作业记录：清单里声明 capabilities.jobs 之后，ctx.jobs.enqueue 才会写行。'
              : '没有未完成的作业（取消「只看未完成的」可看历史）。'}
          </p>
        ) : (
          <table className="jobs-panel__table" aria-label="作业列表">
            <thead>
              <tr>
                <th scope="col">名称</th>
                <th scope="col">状态</th>
                <th scope="col">尝试</th>
                <th scope="col">预算</th>
                <th scope="col">目标会话</th>
                <th scope="col">下次执行</th>
                <th scope="col">最近错误</th>
                <th scope="col">入队</th>
              </tr>
            </thead>
            <tbody>
              {shownJobs.map((job) => (
                <tr
                  key={job.id}
                  className={`jobs-panel__row jobs-panel__row--${
                    isActiveJob(job.state) ? 'active' : 'settled'
                  }`}
                >
                  <td className="jobs-panel__name">{job.name}</td>
                  <td className="jobs-panel__state">{jobStateLabel(job.state)}</td>
                  {/* `attempts` 含当前这一次：排队的作业是 0，跑过一次的是 1。 */}
                  <td>{formatCount(job.attempts)}</td>
                  <td>{`${formatCount(job.budgetMs)} ms`}</td>
                  <td className="jobs-panel__mono">{job.conversation ?? '—'}</td>
                  <td>{isActiveJob(job.state) ? formatStamp(job.nextAt) : '—'}</td>
                  <td className="jobs-panel__error-cell">{job.lastError ?? '—'}</td>
                  <td>{formatStamp(job.createdAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="jobs-panel__section" aria-label="会话等待">
        <h3 className="jobs-panel__title">会话等待</h3>
        {shownWaits.length === 0 ? (
          <p className="panel-empty" role="status">
            {waits.length === 0
              ? '这个 bot 还没有等待记录：ctx.session.prompt 会写一行，收到回答后变「已收到回答」。'
              : '没有等待中的会话（取消「只看未完成的」可看历史）。'}
          </p>
        ) : (
          <table className="jobs-panel__table" aria-label="等待列表">
            <thead>
              <tr>
                <th scope="col">键</th>
                <th scope="col">状态</th>
                <th scope="col">谁可以回答</th>
                <th scope="col">提示</th>
                <th scope="col">目标会话</th>
                <th scope="col">截止</th>
                <th scope="col">剩余重发</th>
                <th scope="col">注册</th>
              </tr>
            </thead>
            <tbody>
              {shownWaits.map((wait) => (
                <tr
                  key={wait.id}
                  className={`jobs-panel__row jobs-panel__row--${
                    isPendingWait(wait.state) ? 'active' : 'settled'
                  }`}
                >
                  <td className="jobs-panel__mono">{wait.key}</td>
                  <td className="jobs-panel__state">{waitStateLabel(wait.state)}</td>
                  <td>{waitScopeLabel(wait.scope)}</td>
                  <td className="jobs-panel__prompt">{wait.prompt}</td>
                  <td className="jobs-panel__mono">{wait.conversation}</td>
                  <td>{isPendingWait(wait.state) ? formatStamp(wait.deadline) : '—'}</td>
                  <td>
                    {wait.remindersLeft === 0
                      ? '0'
                      : `${formatCount(wait.remindersLeft)}（每轮 ${formatCount(wait.timeoutMs)} ms）`}
                  </td>
                  <td>{formatStamp(wait.createdAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </section>
  )
}
