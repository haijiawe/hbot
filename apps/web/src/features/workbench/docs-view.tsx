/**
 * 文档面：左侧是文档清单（侧栏那份），右侧是就地编辑的正文。
 *
 * 用 `fs.read` / `fs.write` 直接读写项目里的 Markdown / 纯文本 / JSON —— 不另建"文档库"：
 * 代码是唯一真源（AGENTS.md §0.9），把笔记存进第二个地方就等于让用户读两份互不相干的真相。
 * 所以这里没有"新建文档"按钮：要新文档就在资源管理器里建文件，这里只负责改。
 */

import { useCallback, useEffect, useState } from 'react'
import { pickDocs, type DocEntry } from '../../lib/doc-list'
import { errorMessage } from '../../lib/format'
import { dispatch } from '../../lib/ipc'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 侧栏的文档清单：点一项就切换主区域的正文。 */
export function DocsSide() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const files = useWorkspaceStore((state) => state.files)
  const docPath = useWorkspaceStore((state) => state.docPath)
  const setDocPath = useWorkspaceStore((state) => state.setDocPath)
  const docs = pickDocs(files)

  return (
    <div className="wb-page">
      <header className="wb-head">
        <div className="wb-eyebrow">DOCS</div>
        <h2 className="wb-title">设计文档</h2>
        <span className="wb-pill">{docs.length} 份</span>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : docs.length === 0 ? (
        <p className="wb-empty">项目里没有 Markdown / 纯文本 / JSON 文件。</p>
      ) : (
        <ul className="wb-list" aria-label="文档清单">
          {docs.map((doc) => (
            <li key={doc.path}>
              <button
                type="button"
                className={'wb-item wb-item--button' + (doc.path === docPath ? ' wb-item--active' : '')}
                onClick={() => setDocPath(doc.path)}
                title={doc.path}
              >
                <span className="wb-item__main">
                  <span className="wb-item__name">{doc.name}</span>
                  <span className="wb-item__meta">{doc.dir.length === 0 ? '项目根' : doc.dir}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <p className="wb-note">这里只列 Markdown / 纯文本 / JSON；源码在资源管理器里改。</p>
    </div>
  )
}

/** 主区域：一个 textarea + 保存。 */
export function DocsView() {
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const files = useWorkspaceStore((state) => state.files)
  const docPath = useWorkspaceStore((state) => state.docPath)
  const setDocPath = useWorkspaceStore((state) => state.setDocPath)
  const [text, setText] = useState('')
  const [dirty, setDirty] = useState(false)
  const [loading, setLoading] = useState(false)
  const [saved, setSaved] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const docs: DocEntry[] = pickDocs(files)
  const current = docs.find((doc) => doc.path === docPath) ?? docs[0]

  // 选中的路径不存在了（换了项目 / 文件被删）：回落到第一份，别让主区域空着。
  useEffect(() => {
    if (current && current.path !== docPath) setDocPath(current.path)
  }, [current, docPath, setDocPath])

  const load = useCallback(async (path: string) => {
    setLoading(true)
    setError(null)
    try {
      const result = await dispatch({ type: 'fs.read', path })
      setText(result.content)
      setDirty(false)
      setSaved(null)
    } catch (cause) {
      setError('读取失败：' + errorMessage(cause))
      setText('')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    if (current === undefined) return
    void load(current.path)
  }, [current, load])

  const save = useCallback(async () => {
    if (current === undefined) return
    try {
      await dispatch({ type: 'fs.write', path: current.path, content: text })
      setDirty(false)
      setSaved(current.name)
      setError(null)
    } catch (cause) {
      setError('保存失败：' + errorMessage(cause))
    }
  }, [current, text])

  return (
    <div className="wb-page wb-page--editor">
      <header className="wb-head">
        <div className="wb-eyebrow">DOCS</div>
        <h2 className="wb-title">{current ? current.name : '设计文档'}</h2>
        {current ? <span className="wb-pill">{current.path}</span> : null}
        <div className="wb-actions">
          <button
            className="wb-btn wb-btn--primary"
            type="button"
            onClick={() => void save()}
            disabled={!dirty || current === undefined}
          >
            {dirty ? '保存' : '已保存'}
          </button>
        </div>
      </header>
      {projectRoot === null ? (
        <p className="wb-empty">先打开一个项目。</p>
      ) : error !== null ? (
        <p className="wb-error">{error}</p>
      ) : current === undefined ? (
        <p className="wb-empty">项目里没有可编辑的文档。</p>
      ) : loading ? (
        <p className="wb-empty" role="status">正在读取 {current.name}…</p>
      ) : (
        <>
          <textarea
            className="wb-textarea"
            aria-label="文档正文"
            value={text}
            onChange={(event) => {
              setText(event.target.value)
              setDirty(true)
              setSaved(null)
            }}
          />
          {saved !== null ? <p className="wb-note">已保存 {saved}。</p> : null}
        </>
      )}
    </div>
  )
}
