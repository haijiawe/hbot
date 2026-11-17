/**
 * 回放结果的单条事件（「回放」页签里的一行）。
 *
 * 布局刻意做成**左右两栏**：回放的全部意义是"历史怎么说的 / 现在怎么说的"，
 * 上下堆叠会让读者来回滚动对不齐。差异标记单独一段，避免颜色成为唯一信号
 * （每个标记都有中文标签：多出 / 缺少 / 历史第 N 条 ↔ 重跑第 M 条）。
 */

import type { ReplayEntry } from '../../generated'
import { formatClock } from '../../lib/format'

/** 状态的展示口径：契约里 `status` 是开放字符串，未知值一律按「有差异」画，不许抛错。 */
interface StatusView {
  /** 中文标签（颜色之外的第二信号，a11y 要求）。 */
  label: string
  /** CSS 修饰符，`'match'` / `'diff'` / `'error'` 三档。 */
  kind: 'match' | 'diff' | 'error'
}

export function replayStatusView(status: string): StatusView {
  if (status === 'match') return { label: '一致', kind: 'match' }
  if (status === 'error') return { label: '出错', kind: 'error' }
  // `diff` 以及**任何没见过的值**都按有差异处理：新版本核心多出一种状态时，
  // 面板的正确行为是"提醒我这儿要人看"，而不是整块面板炸掉。
  return { label: '有差异', kind: 'diff' }
}

/** 一栏回复的正文；空数组给一句占位，避免"空白栏"看起来像渲染失败。 */
function ReplyColumn(props: { title: string; replies: readonly string[] }) {
  return (
    <section className="replay-entry__column" aria-label={props.title}>
      <h4 className="replay-entry__column-title">{props.title}</h4>
      {props.replies.length === 0 ? (
        <p className="replay-entry__empty">（没有回复）</p>
      ) : (
        <ol className="replay-entry__replies">
          {props.replies.map((text, index) => (
            <li className="replay-entry__reply" key={`${index}-${text}`}>
              {text}
            </li>
          ))}
        </ol>
      )}
    </section>
  )
}

export function ReplayEntryRow(props: { entry: ReplayEntry; index: number }) {
  const { entry, index } = props
  const status = replayStatusView(entry.status)
  const reordered = entry.reordered

  return (
    <li className={`replay-entry replay-entry--${status.kind}`}>
      <header className="replay-entry__header">
        <span className="replay-entry__seq">#{index + 1}</span>
        <span className={`replay-entry__status replay-entry__status--${status.kind}`}>
          {status.label}
        </span>
        <span className="replay-entry__time">{formatClock(entry.receivedAt)}</span>
        <span className="replay-entry__external" title={entry.conversationExternalId}>
          {entry.externalId}
        </span>
      </header>

      {entry.error === null ? null : (
        // 逐条错误是这条事件自己跑挂了（handler 抛异常），原文照抄给用户看。
        <p className="inline-error replay-entry__error" role="alert">
          {entry.error}
        </p>
      )}

      <div className="replay-entry__columns">
        <ReplyColumn title="历史回复" replies={entry.historical} />
        <ReplyColumn title="重跑回复" replies={entry.replayed} />
      </div>

      {entry.extra.length === 0 && entry.missing.length === 0 && reordered.length === 0 ? null : (
        <ul className="replay-entry__marks" aria-label="回复差异">
          {entry.extra.map((text) => (
            <li className="replay-entry__mark replay-entry__mark--extra" key={`extra-${text}`}>
              <span className="replay-entry__mark-label">多出</span>
              <span className="replay-entry__mark-text">{text}</span>
            </li>
          ))}
          {entry.missing.map((text) => (
            <li className="replay-entry__mark replay-entry__mark--missing" key={`missing-${text}`}>
              <span className="replay-entry__mark-label">缺少</span>
              <span className="replay-entry__mark-text">{text}</span>
            </li>
          ))}
          {reordered.map((change) => (
            <li
              className="replay-entry__mark replay-entry__mark--reordered"
              key={`reordered-${change.historicalIndex}-${change.replayedIndex}`}
            >
              <span className="replay-entry__mark-label">
                历史第 {change.historicalIndex + 1} 条 ↔ 重跑第 {change.replayedIndex + 1} 条
              </span>
              <span className="replay-entry__mark-text">{change.text}</span>
            </li>
          ))}
        </ul>
      )}
    </li>
  )
}