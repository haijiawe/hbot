/**
 * 调试工具条：继续 / 单步跳过 / 单步进入 / 单步跳出 / 停止。
 *
 * 只在 `debug.paused` 之后可点（暂停期间核心阻塞在 DebugSink 里等这三条命令之一），
 * 所以 `paused === null` 时全部禁用并显示"未在调试"。键位提示从
 * `lib/keybindings.ts` 取（唯一真源），这里不写 F5 之类的字面量。
 */

import type { KeybindingId } from '../../lib/keybindings'
import { formatKeybinding, keybindingById } from '../../lib/keybindings'
import { useDebugStore } from '../../stores/debug-store'
import type { StepKind } from '../../generated'

interface StepButton {
  kind: StepKind
  id: KeybindingId
  label: string
}

const STEP_BUTTONS: readonly StepButton[] = [
  { kind: 'over', id: 'debug-step-over', label: '跳过' },
  { kind: 'into', id: 'debug-step-into', label: '进入' },
  { kind: 'out', id: 'debug-step-out', label: '跳出' },
]

function chordOf(id: KeybindingId): string {
  return formatKeybinding(keybindingById(id))
}

export function DebugToolbar() {
  const paused = useDebugStore((state) => state.paused)
  const busy = useDebugStore((state) => state.busy)
  const resume = useDebugStore((state) => state.resume)
  const step = useDebugStore((state) => state.step)
  const stopDebugging = useDebugStore((state) => state.stopDebugging)

  const disabled = paused === null || busy
  /** 暂停态给状态文字加修饰类；拆出来是为了不把模板字面量写成超长行。 */
  const statusModifier = paused === null ? '' : ' debug-toolbar__status--paused'

  return (
    <div className="debug-toolbar" role="toolbar" aria-label="调试">
      <button
        type="button"
        className="button button--primary"
        disabled={disabled}
        title={`继续（${chordOf('debug-continue')}）`}
        onClick={() => void resume()}
      >
        继续
      </button>
      {STEP_BUTTONS.map((item) => (
        <button
          key={item.kind}
          type="button"
          className="button"
          disabled={disabled}
          title={`单步${item.label}（${chordOf(item.id)}）`}
          onClick={() => void step(item.kind)}
        >
          单步{item.label}
        </button>
      ))}
      <button
        type="button"
        className="button"
        disabled={disabled}
        title="停止调试：清除断点并继续运行到下一次停止"
        onClick={() => void stopDebugging()}
      >
        停止
      </button>
      <span className={`debug-toolbar__status${statusModifier}`} role="status">
        {paused === null ? '未在调试' : `${paused.file}:${paused.line}`}
      </span>
    </div>
  )
}