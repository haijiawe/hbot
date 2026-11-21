/**
 * 概览面：项目身份 + 当前 bot 的活跃态 + 上手清单。
 *
 * 它替代的是"打开 IDE 看到一片空白"：不用先知道命令面板在哪，也能看出
 * 这个项目声明了什么、bot 在不在跑、下一步该点哪里。
 *
 * 只做只读展示与两个按钮（启停、刷新）；不发新命令，数据都来自既有命令。
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
  const canStart = selected !== null && status !== "running" && status !== "starting"
  const canStop = selected !== null && status !== "stopped" && status !== "stopping"

  return (
    <div className="wb">
      <header className="wb__header">
        <h2 className="wb__title">概览</h2>
        <button className="button" type="button" onClick={() => void refresh()} disabled={projectRoot === null}>
          刷新
        </button>
      </header>

      {projectRoot === null ? (
        <p className="panel-hint">
          还没有打开项目。按 Ctrl+Shift+P 选「打开项目…」或「新建项目…」。
        </p>
      ) : (
        <>
          <section className="wb__section" aria-label="项目">
            <div className="wb__eyebrow">项目</div>
            <div className="wb__name" title={projectName ?? ""}>{projectName}</div>
            <div className="wb__meta">
              {manifest === null ? "清单未读" : manifest.platform.length > 0 ? "平台 " + manifest.platform : "未声明平台"}
              {manifest === null ? "" : " · " + manifest.declaredCount + " 项能力"}
            </div>
            {manifest !== null && manifest.entry.length > 0 ? (
              <div className="wb__meta">入口 {manifest.entry}</div>
            ) : null}
            {manifestError !== null ? <p className="wb__error">{manifestError}</p> : null}
          </section>

          <section className="wb__section" aria-label="运行状态">
            <div className="wb__eyebrow">运行状态</div>
            {selected === null ? (
              <p className="panel-hint">这个项目还没有可启动的 bot：检查 bots.json 里的声明。</p>
            ) : (
              <>
                <div className="wb__row">
                  <span className={statusDotClass(status)} aria-hidden="true" />
                  <span className="wb__value">{selected.name}</span>
                  <span className="wb__meta">{BOT_STATUS_LABEL[status]}</span>
                </div>
                <div className="wb__buttons">
                  <button
                    className="button button--primary"
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
                    className="button"
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

          <section className="wb__section" aria-label="清单摘要">
            <div className="wb__eyebrow">清单摘要</div>
            <div className="wb__meta">{manifest === null ? "—" : limitsSummary(manifest) || "没有声明限额"}</div>
            <div className="wb__meta">
              {manifest === null ? "—" : manifest.httpHosts.length === 0 ? "没有授权任何 HTTP 源" : "授权 " + manifest.httpHosts.length + " 个 HTTP 源"}
            </div>
          </section>

          <section className="wb__section" aria-label="上手清单">
            <div className="wb__eyebrow">从这里开始</div>
            <ol className="wb__checklist">
              <li className={bots.length > 0 ? "wb__check wb__check--done" : "wb__check"}>项目里有可启动的 bot</li>
              <li className={status === "running" ? "wb__check wb__check--done" : "wb__check"}>启动 bot（本地平台不需要账号）</li>
              <li className="wb__check">切到「会话」，发一条模拟消息</li>
              <li className={manifest !== null && manifest.declaredCount > 0 ? "wb__check wb__check--done" : "wb__check"}>在 hbot.json 里声明要用的能力</li>
            </ol>
          </section>
        </>
      )}
    </div>
  )
}
