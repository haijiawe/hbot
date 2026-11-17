/**
 * 调试面板（暂停现场）：调用栈 + scopes 变量 + 断点清单。
 *
 * 只展示事件送来的东西 —— **不求值任何表达式**，没有 watch / console。
 * 条件断点是支持的，但条件在**核心的 isolate 里**求值（那里才有作用域），这里只显示
 * 两样东西：暂停时"命中的条件"（`debug.paused` 的 `condition` 字段），以及断点清单里
 * 每一行挂的条件原文。两者都是文本，面板不做任何解析。
 *
 * 变量值按 JSON 显示，序列化不了就 `<unserializable>`（见 `lib/debug-format.ts`）。
 *
 * 面板尺寸：分栏宽度取 `lib/layout.ts` 的 `DEBUG_STACK_WIDTH`，整体高度/开合仍由
 * 底部面板的 `--bottom-panel-height` 决定（`resolveLayout` 是唯一真源）。
 */

import type { DebugBreakpoint, StackFrame } from '../../generated'
import { UNSERIALIZABLE, formatDebugValue } from '../../lib/debug-format'
import { DEBUG_STACK_WIDTH } from '../../lib/layout'
import { useDebugStore } from '../../stores/debug-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { DebugToolbar } from './debug-toolbar'

/** 调用栈一行：点一下选中该帧，并把编辑器跳到它的文件行。 */
function frameRows(
  callStack: readonly StackFrame[],
  selectedFrame: number,
  onSelect: (index: number, frame: StackFrame) => void,
) {
  if (callStack.length === 0) return <p className="panel-hint">调用栈为空。</p>
  return callStack.map((frame, index) => (
    <button
      key={`${frame.file}:${frame.line}:${frame.column}:${index}`}
      type="button"
      role="option"
      aria-selected={index === selectedFrame}
      className={`debug__frame${index === selectedFrame ? ' debug__frame--active' : ''}`}
      onClick={() => onSelect(index, frame)}
    >
      <span className="debug__frame-name">{frame.name}</span>
      <span className="debug__frame-location">
        {frame.file}:{frame.line}
      </span>
    </button>
  ))
}

/**
 * 断点清单：按文件分组，一行一个断点（行号 + 条件原文）。
 *
 * 条件为空显示「无条件」而不是留白：留白会被读成"这一行没加载出来"。
 * 点一行跳到那个位置 —— 清单的价值一半在"我还设过哪些断点"。
 */
function breakpointRows(
  breakpoints: Record<string, DebugBreakpoint[]>,
  onOpen: (file: string, line: number) => void,
) {
  const files = Object.keys(breakpoints).sort()
  if (files.length === 0) {
    return <p className="panel-hint">还没有断点。点编辑器行号左侧的槽位添加，右键可以写条件。</p>
  }
  return files.map((file) => (
    <section className="debug__bp-file" key={file} aria-label={file}>
      <h3 className="debug__bp-title" title={file}>
        {file}
      </h3>
      <ul className="debug__bp-list">
        {(breakpoints[file] ?? []).map((entry) => (
          <li key={entry.line}>
            <button
              type="button"
              className="debug__bp"
              onClick={() => onOpen(file, entry.line)}
              title={entry.condition === null ? '无条件断点' : `条件：${entry.condition}`}
            >
              <span className="debug__bp-line">第 {entry.line} 行</span>
              <span
                className={`debug__bp-condition${entry.condition === null ? ' debug__bp-condition--none' : ''}`}
              >
                {entry.condition === null ? '无条件' : entry.condition}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </section>
  ))
}

export function DebugPanel() {
  const paused = useDebugStore((state) => state.paused)
  const selectedFrame = useDebugStore((state) => state.selectedFrame)
  const selectFrame = useDebugStore((state) => state.selectFrame)
  const error = useDebugStore((state) => state.error)
  const dismissError = useDebugStore((state) => state.dismissError)
  const breakpoints = useDebugStore((state) => state.breakpoints)
  const openAt = useWorkspaceStore((state) => state.openAt)

  return (
    <section className="debug" aria-label="调试">
      <DebugToolbar />

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

      {paused === null ? (
        <div className="debug__idle">
          <p className="panel-empty" role="status">
            未在调试。点编辑器行号左侧的槽位加断点，右键可以写条件；启动 bot
            命中后这里显示调用栈与变量。
          </p>
          <p className="panel-hint">
            调试模式用源码插桩执行，比正常运行慢 2–5 倍；不支持 eval / watch 任意表达式。
          </p>
        </div>
      ) : (
        <>
          {/* 命中的条件：核心把"为什么停在这"一起送过来（无条件断点与单步是 null）。 */}
          {paused.condition === null ? null : (
            <p className="debug__condition" role="status">
              <span className="debug__condition-label">命中的条件</span>
              <code className="debug__condition-text">{paused.condition}</code>
            </p>
          )}

          <div className="debug__body">
            <div
              className="debug__stack"
              style={{ width: DEBUG_STACK_WIDTH }}
              role="listbox"
              aria-label="调用栈"
            >
              {frameRows(paused.callStack, selectedFrame, (index, frame) => {
                selectFrame(index)
                void openAt(frame.file, frame.line)
              })}
            </div>

            <div className="debug__scopes" aria-label="变量">
              {paused.scopes.length === 0 ? (
                <p className="panel-hint">没有可显示的局部变量。</p>
              ) : (
                paused.scopes.map((scope) => (
                  <section className="debug__scope" key={scope.name} aria-label={scope.name}>
                    <h3 className="debug__scope-title">{scope.name}</h3>
                    {Object.keys(scope.variables).length === 0 ? (
                      <p className="panel-hint">（空）</p>
                    ) : (
                      <dl className="debug__vars">
                        {Object.entries(scope.variables).map(([name, value]) => (
                          <div className="debug__var" key={name}>
                            <dt className="debug__var-name">{name}</dt>
                            <dd className="debug__var-value">{formatDebugValue(value)}</dd>
                          </div>
                        ))}
                      </dl>
                    )}
                  </section>
                ))
              )}
            </div>
          </div>

          <p className="debug__note">
            调试模式：源码插桩执行，比正常运行慢 2–5 倍；值为函数 / 循环引用 / 超大对象时显示{' '}
            {UNSERIALIZABLE}；断点条件只支持同步 JS 表达式（写 await 会编译失败、按不命中处理），
            不支持 eval / watch 任意表达式。
          </p>
        </>
      )}

      <section className="debug__breakpoints" aria-label="断点">
        <h3 className="debug__bp-heading">断点</h3>
        {breakpointRows(breakpoints, (file, line) => void openAt(file, line))}
      </section>
    </section>
  )
}