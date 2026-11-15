/** Diff 卡片占位：只展示摘要与决策，真正落盘必须由核心逐块应用（AGENTS.md §0.10）。 */

import type { AgentDiff, AgentDiffStatus } from '../../stores/agent-store'

const STATUS_LABEL: Record<AgentDiffStatus, string> = {
  pending: '待审阅',
  accepted: '已接受',
  rejected: '已拒绝',
}

interface DiffCardProps {
  diff: AgentDiff
  onResolve: (status: 'accepted' | 'rejected') => void
}

export function DiffCard({ diff, onResolve }: DiffCardProps) {
  const pending = diff.status === 'pending'

  return (
    <article className={`diff-card diff-card--${diff.status}`} aria-label={`改动待审阅：${diff.path}`}>
      <header className="diff-card__header">
        <span className="diff-card__path" title={diff.path}>
          {diff.path}
        </span>
        <span className="diff-card__status">{STATUS_LABEL[diff.status]}</span>
      </header>
      <p className="diff-card__summary">{diff.summary}</p>
      <p className="diff-card__stats">
        <span className="diff-card__added">+{diff.added}</span>{' '}
        <span className="diff-card__removed">−{diff.removed}</span>
      </p>
      <div className="diff-card__actions">
        <button
          type="button"
          className="button button--primary"
          disabled={!pending}
          onClick={() => onResolve('accepted')}
          aria-label={`接受对 ${diff.path} 的改动`}
        >
          接受
        </button>
        <button
          type="button"
          className="button"
          disabled={!pending}
          onClick={() => onResolve('rejected')}
          aria-label={`拒绝对 ${diff.path} 的改动`}
        >
          拒绝
        </button>
      </div>
      <p className="diff-card__note">M0 占位：接受 / 拒绝只改本地状态，写入将由核心执行。</p>
    </article>
  )
}