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
    <div className="wb">
      <header className="wb__header">
        <h2 className="wb__title">能力</h2>
        <button className="button" type="button" onClick={() => void refreshManifest()} disabled={projectRoot === null}>
          重读清单
        </button>
      </header>
      {projectRoot === null ? (
        <p className="panel-hint">先打开一个项目。</p>
      ) : manifestError !== null ? (
        <p className="wb__error">{manifestError}</p>
      ) : manifest === null ? (
        <p className="panel-hint">正在读 hbot.json…</p>
      ) : (
        <>
          <ul className="cap">
            {manifest.capabilities.map((row) => (
              <li key={row.id} className={row.declared ? "cap__row cap__row--on" : "cap__row"}>
                <span className="cap__mark" aria-hidden="true">{row.declared ? "✓" : "·"}</span>
                <span className="cap__label">{row.label}</span>
                <span className="cap__detail">{row.declared ? row.detail : "未声明"}</span>
              </li>
            ))}
          </ul>
          <p className="wb__note">
            这里只读 hbot.json 的声明。装配（宿主有没有接上模型/设备）与授权（你有没有批）
            要到运行时才成立；声明不等于可用。
          </p>
          <p className="wb__note">
            HTTP 源：{manifest.httpHosts.length === 0 ? "没有" : manifest.httpHosts.join("、")}
          </p>
        </>
      )}
    </div>
  )
}
