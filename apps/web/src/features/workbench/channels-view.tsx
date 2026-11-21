/**
 * 渠道面：bots.json 里声明的平台入口（不含任何凭据）。
 *
 * 为什么读原文而不是新加命令：bots.json 是项目文件，`fs.read` 已经能读。
 * 只列 id / name / platform / ws —— 足以判断「接的是哪个平台、WebSocket 指向哪里」，
 * 而 token / clientSecret 不显示（解析在 lib/bots-file.ts，那里压根不读这些字段）。
 */

import { useEffect } from 'react'
import { parseChannels } from '../../lib/bots-file'
import { useWorkspaceStore } from '../../stores/workspace-store'

export function ChannelsView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const botsFile = useWorkspaceStore((state) => state.botsFile)
  const botsFileError = useWorkspaceStore((state) => state.botsFileError)
  const refreshBotsFile = useWorkspaceStore((state) => state.refreshBotsFile)

  useEffect(() => {
    if (projectRoot === null) return
    void refreshBotsFile()
  }, [projectRoot, refreshBotsFile])

  const channels = botsFile === null ? [] : parseChannels(botsFile)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">CHANNELS</div>
        <h2 className="wb-title">渠道</h2>
        <span className="wb-pill">{channels.length} 条声明</span>
        <div className="wb-actions">
          <button className="wb-btn" type="button" onClick={() => void refreshBotsFile()} disabled={projectRoot === null}>
            重读 bots.json
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : botsFileError !== null ? (
        <p className="wb-error">{botsFileError}</p>
      ) : channels.length === 0 ? (
        <p className="wb-empty">
          bots.json 里没有可读的 bot 声明。新建项目会自动写一条 local 平台条目；接真平台时把 platform 改成 onebot11 或 qq_official。
        </p>
      ) : (
        <>
          <ul className="wb-list">
            {channels.map((row, index) => (
              <li key={row.id.length > 0 ? row.id : String(index)} className="wb-item">
                <div className="wb-item__main">
                  <span className="wb-item__name">{row.name.length > 0 ? row.name : row.id}</span>
                  <span className="wb-item__meta">
                    {row.platform.length > 0 ? row.platform : '未声明平台'}
                    {row.ws.length > 0 ? ' · ' + row.ws : ''}
                  </span>
                </div>
                {row.platform.length > 0 ? <span className="wb-tag">{row.platform}</span> : null}
              </li>
            ))}
          </ul>
          <p className="wb-note">
            凭据（token / clientSecret）不在界面上显示；它们只在核心与 keyring 之间流动。
          </p>
        </>
      )}
    </div>
  )
}
