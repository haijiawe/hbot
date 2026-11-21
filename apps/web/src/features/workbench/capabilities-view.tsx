/**
 * 能力面：清单里声明了什么能力、限额多少。
 *
 * 只显示**声明层**。装配（宿主有没有配模型/设备）与授权（用户批没批）在运行时
 * 才成立，面板底部明确写出这一点 —— 声明了不等于能用（ADR-0005）。
 */

import { useEffect } from 'react'
import { useWorkspaceStore } from '../../stores/workspace-store'

export function CapabilitiesView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const manifest = useWorkspaceStore((state) => state.manifest)
  const manifestError = useWorkspaceStore((state) => state.manifestError)
  const refreshManifest = useWorkspaceStore((state) => state.refreshManifest)

  useEffect(() => {
    if (projectRoot === null) return
    void refreshManifest()
  }, [projectRoot, refreshManifest])

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">MANIFEST</div>
        <h2 className="wb-title">能力</h2>
        <span className="wb-pill">
          {manifest === null ? '清单未读' : manifest.declaredCount + ' / ' + manifest.capabilities.length + ' 已声明'}
        </span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void refreshManifest()} disabled={projectRoot === null}>
            重读清单
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : manifestError !== null ? (
        <p className="wb-error">{manifestError}</p>
      ) : manifest === null ? (
        <p className="wb-empty">正在读 hbot.json…</p>
      ) : (
        <>
          <section className="wb-card" aria-label="能力声明">
            {manifest.capabilities.map((row) => (
              <div key={row.id} className={row.declared ? 'wb-cap wb-cap--on' : 'wb-cap'}>
                <span className="wb-cap__name">{row.label}</span>
                <span className={row.declared ? 'wb-tag wb-tag--on' : 'wb-tag'}>
                  {row.declared ? '声明' : '未声明'}
                </span>
                <span className="wb-cap__detail">{row.declared ? row.detail : 'hbot.json 里没有这一项'}</span>
              </div>
            ))}
          </section>
          <section className="wb-card" aria-label="授权源">
            <div className="wb-card__title">HTTP 源</div>
            <p className="wb-note">
              {manifest.httpHosts.length === 0 ? '没有授权任何 HTTP 源' : manifest.httpHosts.join('、')}
            </p>
          </section>
          <p className="wb-note">
            这里只读 hbot.json 的声明。装配（宿主有没有接上模型/设备）与授权（你有没有批）要到运行时才成立；声明不等于可用。
          </p>
        </>
      )}
    </div>
  )
}
