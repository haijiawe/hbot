/** Diff 卡片：逐文件展示待审阅编辑；接受/拒绝都由核心执行（AGENTS.md §0.10）。 */

import type { PendingEdit } from '../../stores/agent-store'
import { editStats } from '../../stores/agent-store'

const STATUS_LABEL: Record<PendingEdit['status'], string> = {
  pending: '待审阅',
  accepted: '已接受',
  rejected: '已拒绝',
}

interface DiffCardProps {
  edit: PendingEdit
  onAccept: () => void
  onReject: () => void
}

export function DiffCard({ edit, onAccept, onReject }: DiffCardProps) {
  const pending = edit.status === 'pending'
  const stats = editStats(edit.files)
  const title = edit.files.map((file) => file.path).join('、') || '（空编辑）'

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
      </div>

      <p className="diff-card__note">
        接受后由核心按块落盘（`edit_apply`，哈希过期会拒绝并保留这张卡片）；拒绝则丢弃这次编辑。
      </p>
    </article>
  )
}