/**
 * 全局快捷键：唯一入口。组件不再各自 `addEventListener('keydown')`（docs/06 §10）。
 *
 * 用捕获阶段拦截，确保焦点在 Monaco / 输入框里时快捷键仍然生效；命中即
 * `preventDefault + stopPropagation`，避免 Monaco 与浏览器默认行为重复触发。
 */

import { useEffect, useRef } from 'react'
import { keybindingFromEvent } from '../lib/keybindings'

export interface ShortcutActions {
  openPalette: () => void
  openQuickOpen: () => void
  save: () => void
  saveAll: () => void
  /**
   * 行内编辑（docs/06-ide.md §3.2）：让编辑器打开就地浮层。没有可编辑的选区时由
   * 调用方自己给可见反馈（状态栏 toast），这里不假设一定成功。
   */
  inlineEdit: () => void
  run: () => void
  stop: () => void
  toggleAgent: () => void
  toggleBottomPanel: () => void
  showProblems: () => void
  /** 打开全局搜索面板（底部的"搜索"页签 + 输入框抢焦点）。 */
  showSearch: () => void
  /** 调试：继续（未暂停时由 store 自己忽略，组件不必判）。 */
  debugContinue: () => void
  debugStepOver: () => void
  debugStepInto: () => void
  debugStepOut: () => void
}

export function useGlobalKeybindings(actions: ShortcutActions): void {
  const actionsRef = useRef(actions)

  useEffect(() => {
    actionsRef.current = actions
  })

  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent): void {
      if (event.repeat) return
      const binding = keybindingFromEvent(event)
      if (!binding) return

      event.preventDefault()
      event.stopPropagation()

      const current = actionsRef.current
      switch (binding.id) {
        case 'command-palette':
          current.openPalette()
          break
        case 'quick-open':
          current.openQuickOpen()
          break
        case 'save':
          current.save()
          break
        case 'save-all':
          current.saveAll()
          break
        case 'inline-edit':
          current.inlineEdit()
          break
        case 'run-bot':
          current.run()
          break
        case 'stop-bot':
          current.stop()
          break
        case 'toggle-agent':
          current.toggleAgent()
          break
        case 'toggle-bottom-panel':
          current.toggleBottomPanel()
          break
        case 'problems-panel':
          current.showProblems()
          break
        case 'global-search':
          current.showSearch()
          break
        case 'debug-continue':
          current.debugContinue()
          break
        case 'debug-step-over':
          current.debugStepOver()
          break
        case 'debug-step-into':
          current.debugStepInto()
          break
        case 'debug-step-out':
          current.debugStepOut()
          break
        default: {
          const exhaustive: never = binding.id
          throw new Error(`未处理的快捷键：${String(exhaustive)}`)
        }
      }
    }

    window.addEventListener('keydown', handleKeyDown, true)
    return () => window.removeEventListener('keydown', handleKeyDown, true)
  }, [])
}