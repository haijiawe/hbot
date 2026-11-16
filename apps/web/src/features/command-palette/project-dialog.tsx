/**
 * 「打开项目…」/「新建项目…」的极简对话框（命令面板的两条命令弹它）。
 *
 * 为什么是**路径文本输入**而不是文件选择器：web 侧还没有 Tauri 的 dialog 插件
 * （那是另一批），先把"能打开/新建项目"这条链路接通比画一个假的选择器重要。
 * 输入框里明写了"粘贴绝对路径"，避免用户以为可以点选。
 *
 * 状态在 `stores/workspace-store.ts` 的 `projectDialog`（命令面板只负责"发起"，
 * 面板执行完命令就卸载了，真正的渲染在 `app/App.tsx`）。
 */

import { useEffect, useRef, useState } from 'react'
import { errorMessage } from '../../lib/format'
import { createProject, openProject } from '../../lib/project-actions'
import { useWorkspaceStore } from '../../stores/workspace-store'

interface ProjectDialogProps {
  /** 成功后的提示（状态栏的 `notice` 行）；不传就只在状态栏留错误。 */
  onNotice?: (message: string) => void
}

export function ProjectDialog({ onNotice }: ProjectDialogProps) {
  const mode = useWorkspaceStore((state) => state.projectDialog)
  const close = useWorkspaceStore((state) => state.closeProjectDialog)
  const [path, setPath] = useState('')
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const pathRef = useRef<HTMLInputElement | null>(null)

  // 每次换模式重新开始（对话框也可能在打开状态下被另一条命令改成另一种模式）。
  useEffect(() => {
    setPath('')
    setName('')
    setError(null)
    setBusy(false)
  }, [mode])

  useEffect(() => {
    pathRef.current?.focus()
  }, [mode])

  if (mode === null) return null

  const creating = mode === 'create'
  const title = creating ? '新建项目' : '打开项目'
  const submitLabel = creating ? '创建' : '打开'

  async function submit(): Promise<void> {
    if (busy) return
    if (path.trim().length === 0) {
      setError('请粘贴项目绝对路径（例如 C:\\work\\my-bot）。')
      return
    }
    if (creating && name.trim().length === 0) {
      setError('请填写项目名称。')
      return
    }
    setBusy(true)
    setError(null)
    try {
      const root = creating ? await createProject(path, name) : await openProject(path)
      close()
      onNotice?.(creating ? `已新建项目：${root}` : `已打开项目：${root}`)
    } catch (failure) {
      // 动作层已经把原因写进状态栏（`reportError`），这里同步显示在对话框里，
      // 用户可以就地改路径重试，不用去状态栏找原因。
      setError(errorMessage(failure))
      setBusy(false)
    }
  }

  return (
    <div className="palette-backdrop">
      <form
        className="palette project-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(event) => {
          event.preventDefault()
          void submit()
        }}
        onKeyDown={(event) => {
          if (event.key === 'Escape') {
            event.stopPropagation()
            close()
          }
        }}
      >
        <h2 className="project-dialog__title">{title}</h2>

        <label className="project-dialog__field">
          <span className="project-dialog__label">项目路径</span>
          <input
            ref={pathRef}
            className="project-dialog__input"
            value={path}
            spellCheck={false}
            autoComplete="off"
            placeholder="C:\\work\\my-bot"
            aria-label="项目路径"
            onChange={(event) => setPath(event.target.value)}
          />
          <span className="project-dialog__hint">
            当前没有原生文件选择器：请粘贴项目根的绝对路径（例如 C:\work\my-bot）。
          </span>
        </label>

        {creating ? (
          <label className="project-dialog__field">
            <span className="project-dialog__label">项目名称</span>
            <input
              className="project-dialog__input"
              value={name}
              spellCheck={false}
              autoComplete="off"
              placeholder="my-bot"
              aria-label="项目名称"
              onChange={(event) => setName(event.target.value)}
            />
            <span className="project-dialog__hint">名称写进 hbot.json，目录为空或不存在时由核心创建。</span>
          </label>
        ) : null}

        {error === null ? null : (
          <p className="inline-error" role="alert">
            {error}
          </p>
        )}

        <div className="project-dialog__actions">
          <button type="button" className="button" onClick={close} disabled={busy}>
            取消
          </button>
          <button type="submit" className="button button--primary" disabled={busy}>
            {busy ? '正在处理…' : submitLabel}
          </button>
        </div>
      </form>
    </div>
  )
}