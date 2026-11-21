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
const ACTIVE_JOB_STATES = new Set(['pending', 'running'])
const PENDING_WAIT_STATES = new Set(['pending'])

function stateText(job: JobView): string {
  if (job.lastError !== null && job.state === 'failed') return job.lastError
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
      setError('读取运行态失败：' + errorMessage(cause))
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
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">RUNTIME</div>
        <h2 className="wb-title">运行态</h2>
        <span className="wb-pill">{activeJobs} 在途 · {pendingWaits} 等待</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void load()} disabled={botId.length === 0}>
            刷新
          </button>
          <button className="wb-btn" type="button" onClick={() => setBottomPanelTab('jobs')}>
            作业页签
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : error !== null ? (
        <p className="wb-error">{error}</p>
      ) : (
        <>
          <div className="wb-cards">
            <div className="wb-card">
              <div className="wb-card__title">作业</div>
              <div className="wb-stat__num">{activeJobs}</div>
              <div className="wb-stat__label">在途</div>
            </div>
            <div className="wb-card">
              <div className="wb-card__title">等待</div>
              <div className="wb-stat__num">{pendingWaits}</div>
              <div className="wb-stat__label">待回应</div>
            </div>
            <div className="wb-card">
              <div className="wb-card__title">近期</div>
              <div className="wb-stat__num">{jobs.length}</div>
              <div className="wb-stat__label">条作业</div>
            </div>
          </div>
          {jobs.length === 0 && waits.length === 0 ? (
            <p className="wb-empty">
              还没有作业或等待。声明 capabilities.jobs / 用 ctx.session.prompt 之后会出现。
            </p>
          ) : null}
          {jobs.length > 0 ? (
            <section className="wb-card" aria-label="最近作业">
              <div className="wb-card__title">最近作业</div>
              <ul className="wb-list">
                {jobs.slice(0, 6).map((job) => (
                  <li key={job.id} className="wb-item">
                    <div className="wb-item__main">
                      <span className="wb-item__name">{job.name}</span>
                      <span className="wb-item__meta">{stateText(job)} · 第 {job.attempts} 次</span>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          {waits.length > 0 ? (
            <section className="wb-card" aria-label="等待">
              <div className="wb-card__title">等待</div>
              <ul className="wb-list">
                {waits.slice(0, 6).map((wait) => (
                  <li key={wait.id} className="wb-item">
                    <div className="wb-item__main">
                      <span className="wb-item__name">{wait.key}</span>
                      <span className="wb-item__meta">{wait.state}</span>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
        </>
      )}
    </div>
  )
}
