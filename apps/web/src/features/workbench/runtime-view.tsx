/**
 * 运行态面：作业与等待的活跃数量 + 最近几条（完整列表在底部「作业」页签）。
 *
 * 为什么要一个面：作业在后台跑，用户看不到"它到底在不在动"。这里只给数：
 * 待跑 / 在跑 / 最近失败，以及下一次派发的时刻。
 */

import { useCallback, useEffect, useState } from 'react'
import { errorMessage } from '../../lib/format'
import type { JobView, WaitView } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

const VIEW_LIMIT = 20
const ACTIVE_JOB_STATES = new Set(["pending", "running"])
const PENDING_WAIT_STATES = new Set(["pending"])

function stateText(job: JobView): string {
  if (job.lastError !== null && job.state === "failed") return job.lastError
  return job.state
}

export function RuntimeView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const botId = useRunStore((state) => state.botId)
  const bots = useRunStore((state) => state.bots)
  const refresh = useRunStore((state) => state.refresh)
  const setBottomPanelTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const [jobs, setJobs] = useState<JobView[]>([])
  const [waits, setWaits] = useState<WaitView[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    if (botId.length === 0) {
      setJobs([])
      setWaits([])
      return
    }
    try {
      const [nextJobs, nextWaits] = await Promise.all([
        dispatch({ type: 'bot_jobs', botId, limit: VIEW_LIMIT }),
        dispatch({ type: 'bot_waits', botId, limit: VIEW_LIMIT }),
      ])
      setJobs(nextJobs)
      setWaits(nextWaits)
      setError(null)
    } catch (cause) {
      setError(`读取运行态失败：${errorMessage(cause)}`)
    }
  }, [botId])

  useEffect(() => {
    if (projectRoot === null) return
    if (bots.length === 0) void refresh()
    void load()
  }, [projectRoot, bots.length, refresh, load])

  const activeJobs = jobs.filter((job) => ACTIVE_JOB_STATES.has(job.state)).length
  const pendingWaits = waits.filter((wait) => PENDING_WAIT_STATES.has(wait.state)).length

  return (
    <div className="wb">
      <header className="wb__header">
        <h2 className="wb__title">运行态</h2>
        <button className="button" type="button" onClick={() => void load()} disabled={botId.length === 0}>
          刷新
        </button>
      </header>
      {projectRoot === null ? (
        <p className="panel-hint">先打开一个项目。</p>
      ) : error !== null ? (
        <p className="wb__error">{error}</p>
      ) : (
        <>
          <div className="wb__stats">
            <div className="wb__stat"><span className="wb__stat-num">{activeJobs}</span><span className="wb__meta">作业在途</span></div>
            <div className="wb__stat"><span className="wb__stat-num">{pendingWaits}</span><span className="wb__meta">等待中</span></div>
            <div className="wb__stat"><span className="wb__stat-num">{jobs.length}</span><span className="wb__meta">最近作业</span></div>
          </div>
          {jobs.length === 0 && waits.length === 0 ? (
            <p className="panel-hint">还没有作业或等待。声明 capabilities.jobs / 用 ctx.session.prompt 之后会出现。</p>
          ) : null}
          {jobs.length > 0 ? (
            <section className="wb__section" aria-label="最近作业">
              <div className="wb__eyebrow">最近作业</div>
              <ul className="wb__list">
                {jobs.slice(0, 6).map((job) => (
                  <li key={job.id} className="wb__item">
                    <div className="wb__item-main wb__item-main--static">
                      <span className="wb__value">{job.name}</span>
                      <span className="wb__meta">{stateText(job)} · 第 {job.attempts} 次</span>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          {waits.length > 0 ? (
            <section className="wb__section" aria-label="等待">
              <div className="wb__eyebrow">等待</div>
              <ul className="wb__list">
                {waits.slice(0, 6).map((wait) => (
                  <li key={wait.id} className="wb__item">
                    <div className="wb__item-main wb__item-main--static">
                      <span className="wb__value">{wait.key}</span>
                      <span className="wb__meta">{wait.state}</span>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          <button className="button" type="button" onClick={() => setBottomPanelTab("jobs")}>
            打开底部「作业」页签
          </button>
        </>
      )}
    </div>
  )
}
