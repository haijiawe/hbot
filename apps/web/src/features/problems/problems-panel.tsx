/**
 * Problems：诊断列表（docs/06-ide.md §3.1「问题面板可点击跳转」、§13 验收）。
 *
 * 数据来自核心：`dispatch({ type: 'diagnostics', path: null })` 的返回值 + 同一批
 * `diagnostics` 事件（`stores/workspace-store.ts` 的 `refreshDiagnostics` / `ingestEvent`）。
 * 渲染进程不做 full typecheck —— 这是文档写死的边界，面板只负责展示与跳转。
 *
 * 两类条目**不能跳转**，用 `disabled` 明说而不是"点了没反应"：
 * - `path === ''`：项目级诊断（没有具体文件）；
 * - `line <= 0`：核心不知道位置（`files.rs` 里 line=0 表示 unknown）。
 */

import { useEffect } from 'react'
import type { Diagnostic } from '../../lib/ipc'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 严重级 → 中文标签；键是全集，漏一个 `tsc` 就会红（不给默认值兜底）。 */
const SEVERITY_TEXT: Record<Diagnostic['severity'], string> = {
  error: '错误',
  warning: '警告',
  info: '信息',
  hint: '提示',
}

function canNavigate(item: Diagnostic): boolean {
  return item.path !== '' && item.line > 0
}

/** `path:line` 或 `path`（无行号）/「（项目级）」。 */
function locationOf(item: Diagnostic): string {
  if (item.path === '') return '（项目级）'
  if (item.line <= 0) return item.path
  return `${item.path}:${item.line}`
}

export function ProblemsPanel() {
  const diagnostics = useWorkspaceStore((state) => state.diagnostics)
  const openAt = useWorkspaceStore((state) => state.openAt)
  const error = useWorkspaceStore((state) => state.error)
  const dismissError = useWorkspaceStore((state) => state.dismissError)

  useEffect(() => {
    // 切到问题页签（或面板首次挂载）就拉一次：诊断只在按下按钮后才新鲜，
    // 核心不会主动推（`crates/hbot-rpc/src/app/commands/files.rs`）。
    void useWorkspaceStore.getState().refreshDiagnostics()
  }, [])

  return (
    <section className="problems" aria-label="问题">
      {error === null ? null : (
        <div className="inline-error" role="alert">
          <span>{error}</span>
          <button
            type="button"
            className="icon-button"
            onClick={dismissError}
            aria-label="关闭错误提示"
          >
            ×
          </button>
        </div>
      )}

      {diagnostics.length === 0 ? (
        <p className="panel-empty" role="status">
          暂无问题。
        </p>
      ) : (
        <ul className="problems__list" aria-label="诊断列表">
          {diagnostics.map((item, index) => (
            <li key={`${item.path}:${item.line}:${item.column}:${item.code ?? ''}:${index}`}>
              <button
                type="button"
                className={`problem problem--${item.severity}`}
                disabled={!canNavigate(item)}
                title={item.message}
                aria-label={`${SEVERITY_TEXT[item.severity]}：${item.message}（${locationOf(item)}）`}
                onClick={() => {
                  if (!canNavigate(item)) return
                  void openAt(item.path, item.line)
                }}
              >
                <span className={`problem__severity problem__severity--${item.severity}`}>
                  {SEVERITY_TEXT[item.severity]}
                </span>
                <span className="problem__message">{item.message}</span>
                <span className="problem__location">{locationOf(item)}</span>
                {item.code === null ? null : <span className="problem__code">{item.code}</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}