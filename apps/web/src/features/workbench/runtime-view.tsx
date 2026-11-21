/**
 * 运行态面：只给数字（在途作业 / 待回应等待 / 最近作业条数）+ 一个跳到看板的按钮。
 *
 * 为什么不在这里再列一遍作业与等待（2026-11-21 UI 收敛）：这些行在
 * 「作业看板」整页面里已经有一份完整、分列的版本（`jobs-board-view.tsx`）。
 * 同一份数据在两个面上各渲染一次，用户既读不出哪份更全，也要两处都看才敢下判断。
 * 面之间的分工是：**运行态回答「现在有多少」，看板回答「具体是哪几条」**。
 */

import { useCallback, useEffect, useState } from 'react'
import { errorMessage } from '../../lib/format'
import type { JobView, WaitView } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 统计窗口：够覆盖一个 bot 的全部在途（`MAX_JOBS_PER_BOT` 256 / 等待 64），又不拉全表。 */
const COUNT_LIMIT = 200
const ACTIVE_JOB_STATES = new Set(['pending', 'running'])
const PENDING_WAIT_STATES = new Set(['pending'])

export function RuntimeView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const setSidebarView = useWorkspaceStore((state) => state.setSidebarView)
  const botId = useRunStore((state) => state.botId)
  const bots = useRunStore((state) => state.bots)
  const refresh = useRunStore((state) => state.refresh)
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
        dispatch({ type: 'bot_jobs', botId, limit: COUNT_LIMIT }),
        dispatch({ type: 'bot_waits', botId, limit: COUNT_LIMIT }),
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
  const failed = jobs.filter((job) => job.state === 'failed').length

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
          <button className="wb-btn" type="button" onClick={() => setSidebarView('board')}>
            打开看板
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
              <div className="wb-card__title">失败</div>
              <div className="wb-stat__num">{failed}</div>
              <div className="wb-stat__label">本轮已放弃</div>
            </div>
          </div>
          {activeJobs === 0 && pendingWaits === 0 && failed === 0 ? (
            <p className="wb-empty">
              现在没有在途作业或等待。声明 capabilities.jobs / 用 ctx.session.prompt 之后会出现。
            </p>
          ) : (
            <p className="wb-note">具体是哪几条、卡在哪一列，在「作业看板」里看。</p>
          )}
        </>
      )}
    </div>
  )
}
