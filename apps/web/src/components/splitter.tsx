/**
 * 分隔条：拖拽 + 键盘调整，数值全部经 `lib/layout.ts` 的 clamp 后再交给 store。
 *
 * role="separator" 让读屏能读到当前尺寸；方向键 / Home / End 是键盘等价操作（a11y §11）。
 */

import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { RESIZE_STEP, clamp } from '../lib/layout'

export interface SplitterProps {
  /** vertical = 左右分栏（拖宽度）；horizontal = 上下分栏（拖高度）。 */
  orientation: 'vertical' | 'horizontal'
  value: number
  min: number
  max: number
  /** 拖拽/按键方向与数值变化方向的关系：侧栏向右变宽为 1，Agent 栏向左变宽为 -1。 */
  direction?: 1 | -1
  label: string
  onChange: (value: number) => void
}

export function Splitter({
  orientation,
  value,
  min,
  max,
  direction = 1,
  label,
  onChange,
}: SplitterProps) {
  function handlePointerDown(event: ReactPointerEvent<HTMLDivElement>): void {
    if (event.button !== 0) return
    event.preventDefault()

    const target = event.currentTarget
    const pointerId = event.pointerId
    const startPosition = orientation === 'vertical' ? event.clientX : event.clientY
    const startValue = value
    target.setPointerCapture(pointerId)
    document.body.classList.add('is-resizing')

    function handleMove(moveEvent: PointerEvent): void {
      const position = orientation === 'vertical' ? moveEvent.clientX : moveEvent.clientY
      onChange(clamp(startValue + (position - startPosition) * direction, min, max))
    }

    function handleUp(): void {
      document.body.classList.remove('is-resizing')
      window.removeEventListener('pointermove', handleMove)
      window.removeEventListener('pointerup', handleUp)
      window.removeEventListener('pointercancel', handleUp)
      if (target.hasPointerCapture(pointerId)) target.releasePointerCapture(pointerId)
    }

    window.addEventListener('pointermove', handleMove)
    window.addEventListener('pointerup', handleUp)
    window.addEventListener('pointercancel', handleUp)
  }

  function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>): void {
    const decreaseKey = orientation === 'vertical' ? 'ArrowLeft' : 'ArrowUp'
    const increaseKey = orientation === 'vertical' ? 'ArrowRight' : 'ArrowDown'

    if (event.key === decreaseKey) {
      event.preventDefault()
      onChange(clamp(value - RESIZE_STEP * direction, min, max))
    } else if (event.key === increaseKey) {
      event.preventDefault()
      onChange(clamp(value + RESIZE_STEP * direction, min, max))
    } else if (event.key === 'Home') {
      event.preventDefault()
      onChange(min)
    } else if (event.key === 'End') {
      event.preventDefault()
      onChange(max)
    }
  }

  return (
    <div
      className={`splitter splitter--${orientation}`}
      role="separator"
      tabIndex={0}
      aria-label={label}
      aria-orientation={orientation}
      aria-valuenow={value}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuetext={`${value}px`}
      onPointerDown={handlePointerDown}
      onKeyDown={handleKeyDown}
    />
  )
}