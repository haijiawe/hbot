/**
 * Problems：M0 占位。
 *
 * TS 诊断在 M2 接入（docs/06-ide.md：诊断走核心 LSP/tsc，不在渲染进程跑 full typecheck），
 * 现在只提供空状态与结构，不伪造诊断数据。
 */

export function ProblemsPanel() {
  return (
    <section className="problems" aria-label="问题">
      <p className="panel-empty" role="status">
        暂无问题。
      </p>
      <p className="panel-hint">
        类型与语法诊断将在 M2 由核心侧提供（渲染进程不做全量 typecheck）。当前面板只保留结构与空状态。
      </p>
    </section>
  )
}