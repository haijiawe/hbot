/** Diff 卡片：逐文件展示待审阅编辑；接受/拒绝都由核心执行（AGENTS.md §0.10）。 */

import type { PendingEdit } from '../../stores/agent-store'
import { editStats } from '../../stores/agent-store'

const STATUS_LABEL: Record<PendingEdit['status'], string> = {
  pending: '待审阅',
  accepted: '已接受',
  rejected: '已拒绝',
  undone: '已撤销',
}

interface DiffCardProps {
  edit: PendingEdit
  /**
   * 这些路径在编辑器里**有未保存改动**（缓冲区 ≠ 磁盘）。
   *
   * 不是拒绝理由，是提示理由：文档 §06-ide「未保存缓冲区默认参与 Diff 并标注」
   * —— 用户必须知道"我手上还有没存的字"，否则接受之后会以为自己刚才的编辑丢了。
   */
  dirtyPaths: readonly string[]
  onAccept: () => void
  onReject: () => void
  /**
   * 撤销这张卡片已经应用过的改动（回到应用前）。
   *
   * 只在 `accepted` 时可点：`pending` 该点拒绝（核心还没写盘），`undone` / `rejected`
   * 没有可退的东西。效果与 Ctrl+Z 一致，但范围是这次应用碰过的全部文件。
   */
  onUndo: () => void
}

export function DiffCard({ edit, dirtyPaths, onAccept, onReject, onUndo }: DiffCardProps) {
  const pending = edit.status === 'pending'
  const accepted = edit.status === 'accepted'
  const dirty = new Set(dirtyPaths)
  const stats = editStats(edit.files)
  const title = edit.files.map((file) => file.path).join('、') || '（空编辑）'
  // 提示里必须带文件名：用户看到"有未保存改动"却不知道是哪个文件，就可能直接把
  // 卡片点掉（那正是本条不变量要防的"以为字丢了"）。只列这张卡片里的文件。
  const dirtyFiles = edit.files.filter((file) => dirty.has(file.path)).map((file) => file.path)

  return (
    <article className={`diff-card diff-card--${edit.status}`} aria-label={`改动待审阅：${title}`}>
      <header className="diff-card__header">
        <span className="diff-card__path" title={title}>
          {title}
        </span>
        <span className="diff-card__status">{STATUS_LABEL[edit.status]}</span>
      </header>

      <ul className="diff-card__files" aria-label="改动的文件">
        {edit.files.map((file) => (
          <li key={file.path} className="diff-card__file">
            <span className="diff-card__path" title={file.path}>
              {file.path}
            </span>
            {dirty.has(file.path) ? (
              <span
                className="diff-card__dirty"
                role="note"
                aria-label={`${file.path} 有未保存改动`}
              >
                有未保存改动
              </span>
            ) : null}
            <span className="diff-card__file-hunks">{file.hunks.length} 块</span>
          </li>
        ))}
      </ul>

      <p className="diff-card__stats">
        <span className="diff-card__added">+{stats.added}</span>{' '}
        <span className="diff-card__removed">−{stats.removed}</span>
      </p>

      <div className="diff-card__actions">
        <button
          type="button"
          className="button button--primary"
          disabled={!pending}
          onClick={onAccept}
          aria-label={`接受对 ${title} 的改动`}
        >
          接受
        </button>
        <button
          type="button"
          className="button"
          disabled={!pending}
          onClick={onReject}
          aria-label={`拒绝对 ${title} 的改动`}
        >
          拒绝
        </button>
        <button
          type="button"
          className="button"
          disabled={!accepted}
          onClick={onUndo}
          aria-label={`撤销这次应用：${title}`}
        >
          撤销这次应用
        </button>
      </div>

      <p className="diff-card__note">
        {edit.status === 'undone'
          ? '已撤销这次应用：缓冲区回到应用前的文本。磁盘上仍是应用后的内容 —— 保存一次就能把磁盘也退回；本次新建的文件仍留在磁盘上（编辑器里不会再自动打开它）。'
          : dirtyFiles.length > 0
            ? `标了「有未保存改动」的文件（${dirtyFiles.join('、')}）：接受时会先按你手上的缓冲区校验，冲突的块会被拒并留在这张卡片上，不会覆盖你没存的字。`
            : '接受后由核心按块落盘（`edit_apply`，哈希过期会拒绝并保留这张卡片）；拒绝则丢弃这次编辑。'}
      </p>
    </article>
  )
}