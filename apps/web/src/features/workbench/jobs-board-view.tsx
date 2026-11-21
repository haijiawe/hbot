/**
 * 作业看板：把 `bot_jobs` 的只读快照按 排队 / 运行中 / 已结束 三列铺开。
 *
 * 为什么单开一个"面"而不是塞进运行态面板：作业的规模一多，列表读不出"卡在哪一列"；
 * 分列之后"排队堆着不动"和"全在运行"是两种完全不同的故障，一眼能分开。
 * 读的是与运行态面同一条只读命令（`bot_jobs`），不新增契约、不改状态。
 */

import { useCallback, useEffect, useState } from 'react'
import type { JobView } from '../../generated'
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

/** 侧栏那份紧凑摘要：只有三列的条数和一个刷新按钮。 */
export function JobsBoardSide() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const botId = useRunStore((state) => state.botId)
  const [jobs, setJobs] = useState<JobView[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    if (botId.length === 0) {
      setJobs([])
      return
    }
    try {
      setJobs(await dispatch({ type: 'bot_jobs', botId, limit: BOARD_LIMIT }))
      setError(null)
    } catch (cause) {
      setError('读取作业失败：' + errorMessage(cause))
    }
  }, [botId])

  useEffect(() => {
    if (projectRoot === null) return
    void load()
  }, [projectRoot, load])

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
      </ul>
      <p className="wb-note">完整字段见底部「作业」页签；这里是只读快照，不改任何状态。</p>
    </div>
  )
}

/** 主区域那份宽版看板。 */
export function JobsBoardView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const setBottomPanelTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const botId = useRunStore((state) => state.botId)
  const bots = useRunStore((state) => state.bots)
  const [jobs, setJobs] = useState<JobView[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const load = useCallback(async () => {
    if (botId.length === 0) {
      setJobs([])
      return
    }
    setLoading(true)
    try {
      setJobs(await dispatch({ type: 'bot_jobs', botId, limit: BOARD_LIMIT }))
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

  const group = groupJobs(jobs)
  const bot = bots.find((item) => item.botId === botId)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">JOBS BOARD</div>
        <h2 className="wb-title">{bot ? bot.name : '作业看板'}</h2>
        <span className="wb-pill">{jobs.length} 条 · {inFlightCount(jobs)} 在途</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void load()} disabled={loading || botId.length === 0}>
            {loading ? '刷新中…' : '刷新'}
          </button>
          <button className="wb-btn" type="button" onClick={() => setBottomPanelTab('jobs')}>
            作业页签
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : botId.length === 0 ? (
        <p className="wb-empty">还没有选中的 bot：先在「Bot」面里选一个。</p>
      ) : error !== null ? (
        <p className="wb-error">{error}</p>
      ) : jobs.length === 0 ? (
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
    </div>
  )
}
