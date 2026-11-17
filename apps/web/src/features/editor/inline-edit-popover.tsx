/**
 * 行内编辑浮层（docs/06-ide.md §3.2 / §5.3）：**就地浮层，不是模态对话框**。
 *
 * 它只做三件事：说清改哪一段（文件 + 行范围）、收一句自然语言、交给 `onSubmit`。
 * 不认识 Monaco、不认识 store —— 所以单测可以直接渲染它；真机里由
 * `editor-surface.tsx` 按选区位置绝对定位挂在 `.editor-surface` 内，并且**不拦**
 * 编辑器本身的交互（没有遮罩、没有 `aria-modal`：用户还能滚代码、改选区）。
 *
 * 失败（发送报错）留在浮层里用 `role="alert"` 显示，不自动关：关掉等于让用户以为
 * 请求已经发出去了。取消就是关掉浮层本身（可见反馈），不发任何请求。
 */

import { useState } from 'react'
import { formatKeybinding, keybindingById } from '../../lib/keybindings'
import type { InlineEditSelection } from '../../lib/inline-edit'

interface InlineEditPopoverProps {
  selection: InlineEditSelection
  /** 该文件有未保存改动：提交前必须让用户看见（docs/06-ide.md §5.2）。 */
  dirty: boolean
  /** 上一次提交的失败原因；`null` = 没有错误。 */
  error: string | null
  /** 正在发送：按钮禁用，避免连点发两条。 */
  busy: boolean
  /** 相对 `.editor-surface` 的左上角（像素）。 */
  left: number
  top: number
  onSubmit: (instruction: string) => void
  onCancel: () => void
}

export function InlineEditPopover({
  selection,
  dirty,
  error,
  busy,
  left,
  top,
  onSubmit,
  onCancel,
}: InlineEditPopoverProps) {
  const [instruction, setInstruction] = useState('')
  const range =
    selection.startLine === selection.endLine
      ? `第 ${selection.startLine} 行`
      : `第 ${selection.startLine}–${selection.endLine} 行`
  const trimmed = instruction.trim()

  return (
    <div
      className="inline-edit"
      style={{ left: `${left}px`, top: `${top}px` }}
      role="dialog"
      aria-label="行内编辑"
      onKeyDown={(event) => {
        if (event.key !== 'Escape') return
        event.preventDefault()
        onCancel()
      }}
    >
      <header className="inline-edit__header">
        <span className="inline-edit__title" title={`${selection.path} ${range}`}>
          行内编辑 · {selection.path} {range}
        </span>
        <button
          type="button"
          className="icon-button"
          onClick={onCancel}
          aria-label="关闭行内编辑浮层"
          title="关闭"
        >
          ×
        </button>
      </header>

      <form
        className="inline-edit__form"
        onSubmit={(event) => {
          event.preventDefault()
          if (trimmed.length === 0 || busy) return
          onSubmit(trimmed)
        }}
      >
        <input
          className="inline-edit__input"
          // 自动聚焦：快捷键之后直接敲指令是主路径，不必再点一下输入框。
          autoFocus
          value={instruction}
          placeholder="用一句话说要怎么改，例如：改成早返回"
          aria-label="行内编辑指令"
          onChange={(event) => setInstruction(event.target.value)}
        />

        {dirty ? (
          <p className="inline-edit__dirty" role="note">
            这个文件有未保存改动：Agent 按磁盘内容生成 Diff，接受时会先按你手上的缓冲区校验。
          </p>
        ) : null}

        {error === null ? null : (
          <p className="inline-edit__error" role="alert">
            {error}
          </p>
        )}

        <div className="inline-edit__actions">
          <button
            type="submit"
            className="button button--primary"
            disabled={busy || trimmed.length === 0}
            aria-label="发送行内编辑请求"
          >
            {busy ? '发送中…' : '发送'}
          </button>
          <button type="button" className="button" onClick={onCancel} aria-label="取消行内编辑">
            取消
          </button>
        </div>
      </form>

      <p className="inline-edit__hint">
        Esc 取消 · {formatKeybinding(keybindingById('inline-edit'))} 重新打开 · 改动会以 Diff
        卡片回来，逐块审阅后才写盘
      </p>
    </div>
  )
}