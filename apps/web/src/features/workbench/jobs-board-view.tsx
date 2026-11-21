/**
 * 作业看板：把 `bot_jobs` 的只读快照按 排队 / 运行中 / 已结束 三列铺开，下面再列会话等待。
 *
 * **这是作业与等待唯一的完整面**（2026-11-21 UI 收敛）：此前底部还有一个「作业」页签，
 * 两处各查一次 `bot_jobs`、各有一套轮询与列表样式，用户要在两处才能拼出同一件事的全貌。
 * 现在侧栏与主区域都只读这一处；运行态面只留数字并跳到这里。
 *
 * 为什么单开一个"面"而不是塞进运行态面板：作业的规模一多，列表读不出"卡在哪一列"；
 * 分列之后"排队堆着不动"和"全在运行"是两种完全不同的故障，一眼能分开。
 * 读的是与运行态面同一条只读命令（`bot_jobs` / `bot_waits`），不新增契约、不改状态。
 */

import { useCallback, useEffect, useState } from 'react'
import type { JobView, WaitView } from '../../generated'
import { errorMessage } from '../../lib/format'
import { dispatch } from '../../lib/ipc'
import {
  BOARD_COLUMNS,
  clockOf,
  groupJobs,
  inFlightCount,
  jobOutcome,
  jobStateLabel,
} from '../../lib/job-board'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 看板一次取多少行：够看清最近一轮，又不至于把几百条终态行铺满屏幕。 */
const BOARD_LIMIT = 60
/** 等待一次取多少行：一个 bot 最多 64 条（`MAX_WAITS_PER_BOT`），够全量。 */
const WAIT_LIMIT = 64

function useBoardData() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const botId = useRunStore((state) => state.botId)
  const [jobs, setJobs] = useState<JobView[]>([])
  const [waits, setWaits] = useState<WaitView[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const load = useCallback(async () => {
    if (botId.length === 0) {
      setJobs([])
      setWaits([])
      return
    }
    setLoading(true)
    try {
      const [nextJobs, nextWaits] = await Promise.all([
        dispatch({ type: 'bot_jobs', botId, limit: BOARD_LIMIT }),
        dispatch({ type: 'bot_waits', botId, limit: WAIT_LIMIT }),
      ])
      setJobs(nextJobs)
      setWaits(nextWaits)
      setError(null)
    } catch (cause) {
      setError('读取作业失败：' + errorMessage(cause))
    } finally {
      setLoading(false)
    }
  }, [botId])

  useEffect(() => {
    if (projectRoot === null) return
    void load()
  }, [projectRoot, load])

  return { projectRoot, botId, jobs, waits, error, loading, load }
}

/** 侧栏那份紧凑摘要：三列条数、等待条数和一个刷新按钮。 */
export function JobsBoardSide() {
  const { botId, jobs, waits, error, load } = useBoardData()
  const group = groupJobs(jobs)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">JOBS</div>
        <h2 className="wb-title">作业看板</h2>
        <span className="wb-pill">{inFlightCount(jobs)} 在途</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void load()} disabled={botId.length === 0}>
            刷新
          </button>
        </div>
      </header>
      {error !== null ? <p className="wb-error">{error}</p> : null}
      <ul className="wb-list" aria-label="作业列统计">
        {BOARD_COLUMNS.map((column) => (
          <li key={column.id} className="wb-item">
            <div className="wb-item__main">
              <span className="wb-item__name">{column.label}</span>
              <span className="wb-item__meta">最新在最上</span>
            </div>
            <span className="wb-tag">{group[column.id].length}</span>
          </li>
        ))}
        <li className="wb-item">
          <div className="wb-item__main">
            <span className="wb-item__name">等待回应</span>
            <span className="wb-item__meta">ctx.session.prompt</span>
          </div>
          <span className="wb-tag">{waits.length}</span>
        </li>
      </ul>
      <p className="wb-note">这里是作业与等待的唯一面；只读快照，不改任何状态。</p>
    </div>
  )
}

/** 主区域那份宽版看板。 */
export function JobsBoardView() {
  const { projectRoot, botId, jobs, waits, error, loading, load } = useBoardData()
  const bots = useRunStore((state) => state.bots)
  const group = groupJobs(jobs)
  const bot = bots.find((item) => item.botId === botId)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">JOBS BOARD</div>
        <h2 className="wb-title">{bot ? bot.name : '作业看板'}</h2>
        <span className="wb-pill">
          {jobs.length} 条 · {inFlightCount(jobs)} 在途 · {waits.length} 等待
        </span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void load()} disabled={loading || botId.length === 0}>
            {loading ? '刷新中…' : '刷新'}
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : botId.length === 0 ? (
        <p className="wb-empty">还没有选中的 bot：先在「Bot」面里选一个。</p>
      ) : error !== null ? (
        <p className="wb-error">{error}</p>
      ) : (
        <>
          {jobs.length === 0 ? (
            <p className="wb-empty">
              还没有作业。在 hbot.json 里声明 capabilities.jobs，并在 handler 里调 ctx.jobs.enqueue。
            </p>
          ) : (
            <div className="wb-board">
              {BOARD_COLUMNS.map((column) => (
                <section key={column.id} className="wb-column" aria-label={column.label}>
                  <h3 className="wb-column__title">
                    {column.label}
                    <span className="wb-column__count">{group[column.id].length}</span>
                  </h3>
                  {group[column.id].length === 0 ? (
                    <p className="wb-column__empty">空</p>
                  ) : (
                    group[column.id].map((job) => (
                      <article key={job.id} className="wb-task">
                        <div className="wb-task__title">{job.name}</div>
                        <div className="wb-task__meta">
                          <span className={'wb-tag wb-tag--' + jobOutcome(job.state)}>{jobStateLabel(job.state)}</span>
                          <span>第 {job.attempts} 次</span>
                          <span>{clockOf(job.createdAt)}</span>
                        </div>
                        <div className="wb-task__meta">
                          <span>预算 {Math.round(job.budgetMs / 1000)}s</span>
                          <span>{job.conversation ?? '只记日志'}</span>
                        </div>
                        {job.lastError !== null ? <p className="wb-task__error">{job.lastError}</p> : null}
                      </article>
                    ))
                  )}
                </section>
              ))}
            </div>
          )}
          <section className="wb-card" aria-label="等待回应">
            <div className="wb-card__title">等待回应（ctx.session.prompt）</div>
            {waits.length === 0 ? (
              <p className="wb-column__empty">没有挂起的等待。</p>
            ) : (
              <ul className="wb-list">
                {waits.map((wait) => (
                  <li key={wait.id} className="wb-item">
                    <div className="wb-item__main">
                      <span className="wb-item__name">{wait.key}</span>
                      <span className="wb-item__meta">
                        {wait.state} · {wait.scope} · {wait.conversation}
                      </span>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </>
      )}
    </div>
  )
}
