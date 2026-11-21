/**
 * Problems：诊断列表（docs/06-ide.md §3.1「问题面板可点击跳转」、§5.3「每条诊断带
 * Fix / Ask Agent to fix」、§13 验收）。
 *
 * 数据来自核心：`dispatch({ type: 'diagnostics', path: null })` 的返回值 + 同一批
 * `diagnostics` 事件（`stores/workspace-store.ts` 的 `refreshDiagnostics` / `ingestEvent`）。
 * 渲染进程不做 full typecheck —— 这是文档写死的边界，面板只负责展示、跳转与两个动作。
 *
 * 两类条目**不能跳转**，用 `disabled` 明说而不是"点了没反应"：
 * - `path === ''`：项目级诊断（没有具体文件）；
 * - `line <= 0`：核心不知道位置（`files.rs` 里 line=0 表示 unknown）。
 * 但「请求 Agent 修复」对它们仍然可用：模型拿到的是消息原文，比人更能从文案里推断位置。
 *
 * 三个动作的边界（任务 C）：
 * - **Fix** 只对机械可修的 code 出现（判据在 `lib/quick-fix.ts`，穷举 + 默认不可修），
 *   它只改缓冲区、绝不写盘；
 * - **请求 Agent 修复**复用既有 `agent.send` 通道，回来后仍是 `diff-card` 逐块审阅；
 * - **全部请求 Agent 修复**只在"当次列表全部是 error"时可点（混进 warning 时禁用）。
 */

import { useEffect, useMemo, useState } from 'react'
import type { Diagnostic } from '../../lib/ipc'
import { SEVERITY_TEXT, displayLocation } from '../../lib/diagnostic-text'
import { canFixMechanically } from '../../lib/quick-fix'
import { useWorkspaceStore } from '../../stores/workspace-store'
import {
  applyMechanicalFixToBuffer,
  askAgentToFix,
  askAgentToFixAll,
  canAskAgentToFixAll,
  dirtyPathsOf,
} from './problem-actions'

function canNavigate(item: Diagnostic): boolean {
  return item.path !== '' && item.line > 0
}

/** 动作按钮的定位串：**不以「）」结尾**（面板既有用例用 `/src\/main\.ts）/` 找主按钮）。 */
function actionLocation(item: Diagnostic): string {
  if (item.path === '') return '项目级'
  if (item.line <= 0) return item.path
  return `${item.path}:${item.line}`
}

export function ProblemsPanel() {
  const diagnostics = useWorkspaceStore((state) => state.diagnostics)
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)
  const openAt = useWorkspaceStore((state) => state.openAt)
  const error = useWorkspaceStore((state) => state.error)
  const dismissError = useWorkspaceStore((state) => state.dismissError)
  const reportError = useWorkspaceStore((state) => state.reportError)
  /** 机械修复成功后的回执（失败走既有的 `error` → `role="alert"`）。 */
  const [notice, setNotice] = useState<string | null>(null)
  /** 正在发请求：按钮禁用，避免连点发两条（`agent.send` 忙时会静默丢弃）。 */
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    // 切到问题页签（或面板首次挂载）就拉一次：诊断只在按下按钮后才新鲜，
    // 核心不会主动推（`crates/hbot-rpc/src/app/commands/files.rs`）。
    //
    // 没有打开项目时不发：核心会以 `-32002 没有打开的项目` 拒绝，而底部面板是常驻挂载的，
    // 这一条会变成首屏状态栏上的一行红字（用户什么都没做就报错）。
    if (projectRoot === null) return
    void useWorkspaceStore.getState().refreshDiagnostics()
  }, [projectRoot])

  // 订阅 `tabs`（唯一真源）而不是每次渲染都去 `getState()`：文件变脏/变干净时这行提示
  // 要跟着变，否则用户会对着过期的提示判断。
  const tabs = useWorkspaceStore((state) => state.tabs)
  const dirtyPaths = useMemo(
    () => dirtyPathsOf(tabs.filter((tab) => tab.dirty).map((tab) => tab.path)),
    [tabs],
  )
  const batchReady = canAskAgentToFixAll(diagnostics)

  const runFix = async (item: Diagnostic): Promise<void> => {
    setNotice(null)
    setBusy(true)
    try {
      const reason = await applyMechanicalFixToBuffer(item)
      if (reason !== null) {
        reportError(reason)
        return
      }
      // 只改缓冲区：文件变脏是正确结果（磁盘没动，用户自己按保存）。
      setNotice(`已修正 ${actionLocation(item)}（只改了编辑器缓冲区，保存才写盘；Ctrl+Z 可撤回）`)
    } finally {
      setBusy(false)
    }
  }

  const runAsk = async (item: Diagnostic): Promise<void> => {
    setNotice(null)
    setBusy(true)
    try {
      const reason = await askAgentToFix(item)
      if (reason !== null) reportError(reason)
    } finally {
      setBusy(false)
    }
  }

  const runAskAll = async (): Promise<void> => {
    setNotice(null)
    setBusy(true)
    try {
      const reason = await askAgentToFixAll(diagnostics)
      if (reason !== null) reportError(reason)
    } finally {
      setBusy(false)
    }
  }

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

      {dirtyPaths.length === 0 ? null : (
        <p className="problems__dirty" role="note">
          {`有 ${dirtyPaths.length} 个文件未保存（${dirtyPaths.join('、')}）：Agent 按磁盘内容生成 Diff，接受修改前建议先保存，否则你手上的改动会与 Diff 校验冲突。`}
        </p>
      )}

      {notice === null ? null : (
        <p className="problems__notice" role="status">
          {notice}
        </p>
      )}

      {diagnostics.length === 0 ? (
        <p className="panel-empty" role="status">
          暂无问题。
        </p>
      ) : (
        <>
          <div className="problems__actions">
            <button
              type="button"
              className="button"
              disabled={busy || !batchReady}
              aria-label={`全部请求 Agent 修复（${diagnostics.length} 条）`}
              title={
                batchReady
                  ? '把当次全部错误合成一条请求交给 Agent'
                  : '只有全部是错误级问题时才能一次性请求修复'
              }
              onClick={() => void runAskAll()}
            >
              全部请求 Agent 修复（{diagnostics.length} 条）
            </button>
          </div>

          <ul className="problems__list" aria-label="诊断列表">
            {diagnostics.map((item, index) => (
              <li
                key={`${item.path}:${item.line}:${item.column}:${item.code ?? ''}:${index}`}
                className="problems__item"
              >
                <button
                  type="button"
                  className={`problem problem--${item.severity}`}
                  disabled={!canNavigate(item)}
                  title={item.message}
                  aria-label={`${SEVERITY_TEXT[item.severity]}：${item.message}（${displayLocation(item)}）`}
                  onClick={() => {
                    if (!canNavigate(item)) return
                    void openAt(item.path, item.line)
                  }}
                >
                  <span className={`problem__severity problem__severity--${item.severity}`}>
                    {SEVERITY_TEXT[item.severity]}
                  </span>
                  <span className="problem__message">{item.message}</span>
                  <span className="problem__location">{displayLocation(item)}</span>
                  {item.code === null ? null : <span className="problem__code">{item.code}</span>}
                </button>

                <span className="problem__actions">
                  {/* 机械可修才出现：判据是穷举白名单，未知 code 一律不给按钮。 */}
                  {canFixMechanically(item.code) ? (
                    <button
                      type="button"
                      className="problem__action"
                      disabled={busy}
                      title="把裸模块名改成相对导入（只改缓冲区，不写盘）"
                      aria-label={`机械修复：${actionLocation(item)}`}
                      onClick={() => void runFix(item)}
                    >
                      Fix
                    </button>
                  ) : null}
                  <button
                    type="button"
                    className="problem__action"
                    disabled={busy}
                    title="让 Agent 自己判断怎么改，改动以 Diff 卡片回来"
                    aria-label={`请求 Agent 修复：${actionLocation(item)}`}
                    onClick={() => void runAsk(item)}
                  >
                    请求 Agent 修复
                  </button>
                </span>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  )
}