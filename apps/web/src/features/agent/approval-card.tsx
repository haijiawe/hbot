/**
 * 审批卡片：核心把工具调用拦在审批这一步，这里只问"允许还是拒绝"。
 *
 * 与提问（`ask_user_question`）分开：审批问是非、提问问内容（AGENTS.md §10 第 3 条）。
 * `always` 的含义由核心定义 —— 规则键含规范化参数与沙箱档，不是"以后同类工具都放行"。
 */

import { useState } from 'react'
import type { RiskLevel } from '../../generated'
import type { ApprovalDecisionInput, PendingApproval } from '../../stores/agent-store'

const RISK_TEXT: Record<RiskLevel, string> = {
  low: '低风险',
  medium: '中风险',
  high: '高风险',
}

/** 参数原样展示：形状未知，序列化失败也要说清楚是什么没渲染出来。 */
function argsText(args: unknown): string {
  try {
    return JSON.stringify(args, null, 2) ?? String(args)
  } catch (error) {
    return `（参数无法序列化：${String(error)}）`
  }
}

interface ApprovalCardProps {
  approval: PendingApproval
  onAnswer: (decision: ApprovalDecisionInput, always: boolean) => void
}

export function ApprovalCard({ approval, onAnswer }: ApprovalCardProps) {
  const [always, setAlways] = useState(false)
  const busy = approval.busy

  return (
    <article className="approval-card" aria-label={`审批请求：${approval.tool}`}>
      <header className="approval-card__header">
        <span className="approval-card__tool" title={approval.tool}>
          {approval.tool}
        </span>
        <span className="approval-card__risk">{RISK_TEXT[approval.risk]}</span>
      </header>

      <pre className="approval-card__args" aria-label="工具参数">
        {argsText(approval.args)}
      </pre>

      <label className="approval-card__always">
        <input
          type="checkbox"
          checked={always}
          disabled={busy}
          onChange={(event) => setAlways(event.target.checked)}
        />
        以后都按这次的动作放行
      </label>

      <div className="approval-card__actions">
        <button
          type="button"
          className="button button--primary"
          disabled={busy}
          onClick={() => onAnswer('approve', always)}
          aria-label={`批准 ${approval.tool}`}
        >
          批准
        </button>
        <button
          type="button"
          className="button"
          disabled={busy}
          onClick={() => onAnswer('deny', always)}
          aria-label={`拒绝 ${approval.tool}`}
        >
          拒绝
        </button>
      </div>
    </article>
  )
}