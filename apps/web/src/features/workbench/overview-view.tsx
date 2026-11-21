/**
 * 概览面：项目身份 + 当前 bot 的活跃态 + 上手清单。
 *
 * 它替代的是"打开 IDE 看到一片空白"：不用先知道命令面板在哪，也能看出
 * 这个项目声明了什么、bot 在不在跑、下一步该点哪里。
 *
 * 只做只读展示与两个按钮（启停、刷新）；不发新命令，数据都来自既有命令。
 * 形态照 Mino 的「宽字距小标题 + 大标题 + 卡片 + 列表」，尺寸压到侧栏宽度。
 */

import { useEffect } from 'react'
import { limitsSummary } from '../../lib/bot-manifest'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { BOT_STATUS_LABEL, statusDotClass } from './bot-status'

export function OverviewView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const manifest = useWorkspaceStore((state) => state.manifest)
  const manifestError = useWorkspaceStore((state) => state.manifestError)
  const refreshManifest = useWorkspaceStore((state) => state.refreshManifest)
  const bots = useRunStore((state) => state.bots)
  const botId = useRunStore((state) => state.botId)
  const status = useRunStore((state) => state.status)
  const busy = useRunStore((state) => state.busy)
  const refresh = useRunStore((state) => state.refresh)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)
  const selectBot = useRunStore((state) => state.selectBot)

  useEffect(() => {
    if (projectRoot === null) return
    void refreshManifest()
    void refresh()
  }, [projectRoot, refreshManifest, refresh])

  const projectName = manifest?.name ?? projectRoot ?? null
  const selected = bots.find((bot) => bot.botId === botId) ?? null
  const canStart = selected !== null && status !== 'running' && status !== 'starting'
  const canStop = selected !== null && status !== 'stopped' && status !== 'stopping'

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">{projectRoot === null ? 'NO PROJECT' : 'BOT WORKBENCH'}</div>
        <h2 className="wb-title">{projectName ?? '概览'}</h2>
        <span className="wb-pill">
          {selected === null ? '没有可启动的 bot' : BOT_STATUS_LABEL[status]}
        </span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void refresh()} disabled={projectRoot === null}>
            刷新
          </button>
        </div>
      </header>

      {projectRoot === null ? (
        <p className="wb-empty">
          还没有打开项目。按 Ctrl+Shift+P 选「打开项目…」或「新建项目…」。
        </p>
      ) : (
        <>
          <section className="wb-card" aria-label="运行状态">
            <div className="wb-card__title">运行状态</div>
            {selected === null ? (
              <p className="wb-note">这个项目还没有可启动的 bot：检查 bots.json 里的声明。</p>
            ) : (
              <>
                <div className="wb-item wb-item--active">
                  <span className={statusDotClass(status)} aria-hidden="true" />
                  <div className="wb-item__main">
                    <span className="wb-item__name">{selected.name}</span>
                    <span className="wb-item__meta">
                      {BOT_STATUS_LABEL[status]} · {selected.platform}
                    </span>
                  </div>
                </div>
                <div className="wb-actions">
                  <button
                    className="wb-btn wb-btn--primary"
                    type="button"
                    disabled={busy || !canStart}
                    onClick={() => {
                      selectBot(selected.botId)
                      void start()
                    }}
                  >
                    启动
                  </button>
                  <button
                    className="wb-btn"
                    type="button"
                    disabled={busy || !canStop}
                    onClick={() => void stop()}
                  >
                    停止
                  </button>
                </div>
              </>
            )}
          </section>

          <section className="wb-card" aria-label="清单摘要">
            <div className="wb-card__title">清单摘要</div>
            <div className="wb-kv">
              <span className="wb-kv__key">平台</span>
              <span className="wb-kv__value">
                {manifest === null || manifest.platform.length === 0 ? '未声明' : manifest.platform}
              </span>
            </div>
            <div className="wb-kv">
              <span className="wb-kv__key">入口</span>
              <span className="wb-kv__value">{manifest !== null && manifest.entry.length > 0 ? manifest.entry : '—'}</span>
            </div>
            <div className="wb-kv">
              <span className="wb-kv__key">限额</span>
              <span className="wb-kv__value">
                {manifest === null ? '—' : limitsSummary(manifest) || '没有声明限额'}
              </span>
            </div>
            {/* 能力与 HTTP 的明细只在「能力」面出现一次（2026-11-21 UI 收敛）：
                这里只报一个数并指路，不再重复渲染同一个列表。 */}
            <div className="wb-kv">
              <span className="wb-kv__key">能力</span>
              <span className="wb-kv__value">
                {manifest === null
                  ? '—'
                  : manifest.declaredCount + ' 项已声明 · ' + manifest.httpHosts.length + ' 个 HTTP 源（明细见「能力」面）'}
              </span>
            </div>
            {manifestError !== null ? <p className="wb-error">{manifestError}</p> : null}
          </section>

          <section className="wb-card" aria-label="上手清单">
            <div className="wb-card__title">从这里开始</div>
            <ol className="wb-list">
              <li className={bots.length > 0 ? 'wb-check wb-check--done' : 'wb-check'}>
                <span className="wb-check__mark" aria-hidden="true">{bots.length > 0 ? '✓' : '○'}</span>
                项目里有可启动的 bot
              </li>
              <li className={status === 'running' ? 'wb-check wb-check--done' : 'wb-check'}>
                <span className="wb-check__mark" aria-hidden="true">{status === 'running' ? '✓' : '○'}</span>
                启动 bot（本地平台不需要账号）
              </li>
              <li className="wb-check">
                <span className="wb-check__mark" aria-hidden="true">○</span>
                切到「会话」，发一条模拟消息
              </li>
              <li className={manifest !== null && manifest.declaredCount > 0 ? 'wb-check wb-check--done' : 'wb-check'}>
                <span className="wb-check__mark" aria-hidden="true">
                  {manifest !== null && manifest.declaredCount > 0 ? '✓' : '○'}
                </span>
                在 hbot.json 里声明要用的能力
              </li>
            </ol>
          </section>
        </>
      )}
    </div>
  )
}
