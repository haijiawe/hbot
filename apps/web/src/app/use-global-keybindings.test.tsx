// @vitest-environment jsdom
/**
 * 全局快捷键里新增的**行内编辑**（`app/use-global-keybindings.ts` + `lib/keybindings.ts`）。
 *
 * 快捷键的唯一真源是 `lib/keybindings.ts`：这里不自己写 `event.key === 'k'`，而是
 * 通过真实的 `keydown` 事件验证"表里的那一行真的接到了动作上"。历史教训是快捷键表与
 * 分发器各写一份，改一处忘一处 —— `switch` 里的 `never` 检查只能在编译期挡新增 id。
 */

import { cleanup, fireEvent, render } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  KEYBINDINGS,
  formatKeybinding,
  keybindingById,
  keybindingFromEvent,
} from '../lib/keybindings'
import { registerInlineEditOpener, requestInlineEdit } from '../lib/inline-edit'
import { useGlobalKeybindings, type ShortcutActions } from './use-global-keybindings'

function Harness({ actions }: { actions: ShortcutActions }): null {
  useGlobalKeybindings(actions)
  return null
}

function actions(patch: Partial<ShortcutActions> = {}): ShortcutActions {
  // 默认每个动作都是**各自的** spy：用例里既能断言"命中一个"，也能断言"其它都没被打到"。
  const spy = (): (() => void) => vi.fn()
  return {
    openPalette: spy(),
    openQuickOpen: spy(),
    save: spy(),
    saveAll: spy(),
    inlineEdit: spy(),
    run: spy(),
    stop: spy(),
    toggleAgent: spy(),
    toggleBottomPanel: spy(),
    showProblems: spy(),
    showSearch: spy(),
    debugContinue: spy(),
    debugStepOver: spy(),
    debugStepInto: spy(),
    debugStepOut: spy(),
    ...patch,
  }
}

afterEach(cleanup)

describe('行内编辑快捷键', () => {
  it('表里有一项 inline-edit，键是 primary+K（唯一真源）', () => {
    const binding = keybindingById('inline-edit')
    expect(binding.chord).toEqual({ key: 'k', primary: true })
    expect(binding.title).toContain('行内编辑')
    // 反查路径也要能命中（分发器就是走这条）。
    expect(
      keybindingFromEvent(
        { key: 'k', ctrlKey: true, metaKey: false, shiftKey: false, altKey: false },
        false,
      )?.id,
    ).toBe('inline-edit')
  })

  it('Ctrl+K 把动作发给 inlineEdit，且只发给它', () => {
    const inlineEdit = vi.fn()
    const others = actions({ inlineEdit })
    const spies = Object.entries(others)
      .filter(([key]) => key !== 'inlineEdit')
      .map(([, action]) => action as () => void)
    render(<Harness actions={others} />)

    fireEvent.keyDown(window, { key: 'k', ctrlKey: true })

    expect(inlineEdit).toHaveBeenCalledTimes(1)
    for (const spy of spies) expect(spy).not.toHaveBeenCalled()
  })

  it('没按修饰键的 k 不触发（编辑代码里打 k 不能被抢）', () => {
    const inlineEdit = vi.fn()
    render(<Harness actions={actions({ inlineEdit })} />)

    fireEvent.keyDown(window, { key: 'k' })

    expect(inlineEdit).not.toHaveBeenCalled()
  })

  it('多按一个 Shift 也不触发（修饰键必须完全一致）', () => {
    const inlineEdit = vi.fn()
    render(<Harness actions={actions({ inlineEdit })} />)

    fireEvent.keyDown(window, { key: 'k', ctrlKey: true, shiftKey: true })

    expect(inlineEdit).not.toHaveBeenCalled()
  })

  it('新增这一项没有挤掉既有快捷键：Ctrl+S 仍然保存', () => {
    const save = vi.fn()
    const inlineEdit = vi.fn()
    render(<Harness actions={actions({ save, inlineEdit })} />)

    fireEvent.keyDown(window, { key: 's', ctrlKey: true })

    expect(save).toHaveBeenCalledTimes(1)
    expect(inlineEdit).not.toHaveBeenCalled()
  })

  it('文案与真源一致（非 mac 平台渲染成 Ctrl+K）', () => {
    expect(formatKeybinding(keybindingById('inline-edit'), false)).toBe('Ctrl+K')
  })

  it('表里没有重复的 id（新增一项不能靠覆盖旧项生效）', () => {
    const ids = KEYBINDINGS.map((binding) => binding.id)
    expect(new Set(ids).size).toBe(ids.length)
  })

  it('没有两行绑同一个键（否则分发器只会命中排在前面的那个）', () => {
    const signatures = KEYBINDINGS.map((binding) => {
      const { key, primary, ctrl, meta, shift, alt } = binding.chord
      return JSON.stringify([key.toLowerCase(), primary ?? false, ctrl ?? false, meta ?? false, shift ?? false, alt ?? false])
    })
    expect(new Set(signatures).size).toBe(signatures.length)
  })

  it('Ctrl+K 一路打到"打开浮层"：快捷键 → requestInlineEdit → 编辑器注册的 opener', () => {
    // 这条是端到端形状：编辑器与快捷键之间只有 `registerInlineEditOpener` 这一条桥，
    // 中间没有再存第二份选区状态。
    const opener = vi.fn(() => true)
    const unregister = registerInlineEditOpener(opener)
    try {
      render(
        <Harness
          actions={actions({
            inlineEdit: () => {
              requestInlineEdit()
            },
          })}
        />,
      )

      fireEvent.keyDown(window, { key: 'k', ctrlKey: true })

      expect(opener).toHaveBeenCalledTimes(1)
    } finally {
      unregister()
    }
  })

  it('没有编辑器（没打开文件）时 requestInlineEdit 返回 false，让调用方去报提示', () => {
    expect(requestInlineEdit()).toBe(false)
  })
})