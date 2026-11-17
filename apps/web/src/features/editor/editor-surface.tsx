/**
 * Monaco 编辑器（lazy chunk）。
 *
 * 一个文件一个 model：切 tab 只 `setModel`，不重建成字符串；关 tab 由 openKey 的
 * 变化统一回收 model。模型里的文本是唯一真源，store 只保存副本用于脏标记与保存。
 *
 * 断点：行槽位（glyph margin）左键点击 → `debugStore.toggleBreakpoint`；右键 → 行内
 * 菜单（编辑条件… / 清除条件 / 删除断点）。条件输入走**内联**输入框而不是
 * `window.prompt`：prompt 是阻塞式的，会把整条事件循环按住（这个窗口里还有正在跑的
 * bot 事件与日志流），而且样式不可控。断点点由这里打装饰；暂停行另有一层整行高亮。
 * 调试面板本身在底部面板的"调试"页签里，这里只管"让断点与当前执行行在代码上看得见"。
 */

import * as monaco from 'monaco-editor'
import { useEffect, useRef, useState } from 'react'
import type { DebugBreakpoint } from '../../generated'
import { languageForPath } from '../../lib/paths'
import { setupMonaco } from '../../lib/monaco/setup'
import {
  MAX_CONDITION_CHARS,
  conditionAt,
  hasBreakpointAt,
  useDebugStore,
} from '../../stores/debug-store'
import { breakpointGlyphs } from './breakpoint-decorations'
import { useWorkspaceStore } from '../../stores/workspace-store'

const MODEL_SCHEME = 'hbot'

function modelUri(path: string): monaco.Uri {
  return monaco.Uri.from({ scheme: MODEL_SCHEME, path: `/${path}` })
}

function pathFromUri(uri: monaco.Uri): string {
  return uri.path.startsWith('/') ? uri.path.slice(1) : uri.path
}

/** 断点条目 → glyph margin 上的装饰（行号越界的丢掉，见 `breakpointGlyphs`）。 */
function breakpointDecorations(
  entries: readonly DebugBreakpoint[] | undefined,
  lineCount: number,
): monaco.editor.IModelDeltaDecoration[] {
  return breakpointGlyphs(entries, lineCount).map((glyph) => ({
    range: new monaco.Range(glyph.line, 1, glyph.line, 1),
    // `hoverMessage` 要 `IMarkdownString`（`{ value }`），不是裸 string。
    options: {
      glyphMarginClassName: glyph.className,
      hoverMessage: { value: glyph.hoverMessage },
    },
  }))
}

/** 行内菜单/条件输入框的位置与目标行。 */
interface GutterMenu {
  file: string
  line: number
  /** 相对 `.editor-surface` 的左上角（像素）。 */
  left: number
  top: number
  /** `menu` = 显示三个动作；`edit` = 显示条件输入框。 */
  mode: 'menu' | 'edit'
}

export default function EditorSurface() {
  const activePath = useWorkspaceStore((state) => state.activePath)
  const openKey = useWorkspaceStore((state) => state.tabs.map((tab) => tab.path).join('\n'))
  const updateContent = useWorkspaceStore((state) => state.updateContent)
  const reveal = useWorkspaceStore((state) => state.reveal)
  const breakpoints = useDebugStore((state) => state.breakpoints)
  const paused = useDebugStore((state) => state.paused)
  const containerRef = useRef<HTMLDivElement | null>(null)
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null)
  const subscriptionsRef = useRef(new Map<string, monaco.IDisposable>())
  /** 每个 model 上一次打的装饰 id：`deltaDecorations` 按 model 覆盖，不叠加。 */
  const decorationsRef = useRef(new Map<string, string[]>())
  const [menu, setMenu] = useState<GutterMenu | null>(null)
  /** 条件输入框的受控值（打开时填当前条件，提交时写回 store）。 */
  const [draft, setDraft] = useState('')
  const menuRef = useRef<GutterMenu | null>(null)
  menuRef.current = menu

  // 只创建一次编辑器实例；真正的 model 在下一个 effect 里挂载。
  useEffect(() => {
    const container = containerRef.current
    if (!container) return
    setupMonaco()

    const editor = monaco.editor.create(container, {
      theme: 'hbot-dark',
      automaticLayout: true,
      fontFamily:
        "'JetBrains Mono', 'Cascadia Code', 'SFMono-Regular', Consolas, 'Liberation Mono', monospace",
      fontSize: 13,
      lineHeight: 20,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      smoothScrolling: false,
      renderLineHighlight: 'line',
      // 断点圆点画在行号左侧的槽位里；宽度由 Monaco 按字形定，不是布局常量。
      glyphMargin: true,
      tabSize: 2,
      insertSpaces: true,
      padding: { top: 8, bottom: 8 },
      ariaLabel: '代码编辑器',
    })
    editorRef.current = editor

    // 行槽位左键 = 切换该行断点。行号区域的点击仍是正常选行，不做处理。
    const gutterClick = editor.onMouseDown((event) => {
      if (event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) return
      const line = event.target.position?.lineNumber
      const path = useWorkspaceStore.getState().activePath
      if (line === undefined || path === null) return
      setMenu(null)
      useDebugStore.getState().toggleBreakpoint(path, line)
    })

    // 行槽位右键 = 该行断点的菜单（只在已有断点时出现：没有断点时右键的意图不明确，
    // 而"加断点"本来左键一下就够了）。
    const gutterMenu = editor.onContextMenu((event) => {
      if (event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) return
      const line = event.target.position?.lineNumber
      const path = useWorkspaceStore.getState().activePath
      if (line === undefined || path === null) return
      const entries = useDebugStore.getState().breakpoints[path]
      if (!hasBreakpointAt(entries, line)) return
      // 拦掉 Monaco 自己的右键菜单：这一行的上下文就是断点，不是编辑器命令。
      event.event.preventDefault()
      event.event.stopPropagation()
      const rect = container.getBoundingClientRect()
      setMenu({
        file: path,
        line,
        left: event.event.posx - rect.left,
        top: event.event.posy - rect.top,
        mode: 'menu',
      })
      setDraft(conditionAt(entries, line) ?? '')
    })

    return () => {
      gutterClick.dispose()
      gutterMenu.dispose()
      for (const subscription of subscriptionsRef.current.values()) subscription.dispose()
      subscriptionsRef.current.clear()
      decorationsRef.current.clear()
      editor.dispose()
      editorRef.current = null
      // 卸载时回收本文件创建的所有 model，避免 HMR / 重挂载泄漏。
      for (const model of monaco.editor.getModels()) {
        if (model.uri.scheme === MODEL_SCHEME) model.dispose()
      }
    }
  }, [])

  // 切换 tab：复用已有 model，没有就按 store 里的文本创建。
  useEffect(() => {
    const editor = editorRef.current
    if (!editor) return
    setMenu(null)
    if (!activePath) {
      editor.setModel(null)
      return
    }

    const uri = modelUri(activePath)
    let model = monaco.editor.getModel(uri)
    if (!model) {
      const initial = useWorkspaceStore.getState().contents[activePath] ?? ''
      model = monaco.editor.createModel(initial, languageForPath(activePath), uri)
      const created = model
      const subscription = created.onDidChangeContent(() => {
        updateContent(pathFromUri(created.uri), created.getValue())
      })
      subscriptionsRef.current.set(activePath, subscription)
    }
    editor.setModel(model)
    editor.focus()
  }, [activePath, updateContent])

  // 断点圆点 + 暂停行高亮：按 model 记装饰 id，切 tab 回来不会重复叠加。
  // 用 `deltaDecorations`（@deprecated）而不是 `createDecorationsCollection`：装饰集合
  // 绑在 editor 实例上，切 model 后要重建一遍，反而不如按 model 存 id 直观。
  useEffect(() => {
    const editor = editorRef.current
    const model = editor?.getModel() ?? null
    if (!editor || !model) return

    const lineCount = model.getLineCount()
    const entries = activePath === null ? [] : (breakpoints[activePath] ?? [])
    const next = breakpointDecorations(entries, lineCount)
    if (paused !== null && paused.file === activePath) {
      const line = Math.min(Math.max(paused.line, 1), lineCount)
      next.push({
        range: new monaco.Range(line, 1, line, 1),
        options: { isWholeLine: true, className: 'debug-paused-line' },
      })
    }

    const key = model.uri.toString()
    const previous = decorationsRef.current.get(key) ?? []
    decorationsRef.current.set(key, editor.deltaDecorations(previous, next))
  }, [activePath, breakpoints, paused])

  // 菜单打开时按 Esc 关闭（输入框自己也处理 Esc，这里是"点了菜单但没输入"的兜底）。
  useEffect(() => {
    if (menu === null) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setMenu(null)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [menu])

  // 关闭 tab：回收对应 model 与监听。
  useEffect(() => {
    const stillOpen = new Set(openKey.split('\n').filter((path) => path.length > 0))
    for (const model of monaco.editor.getModels()) {
      if (model.uri.scheme !== MODEL_SCHEME) continue
      const path = pathFromUri(model.uri)
      if (stillOpen.has(path)) continue
      subscriptionsRef.current.get(path)?.dispose()
      subscriptionsRef.current.delete(path)
      decorationsRef.current.delete(model.uri.toString())
      model.dispose()
    }
  }, [openKey])

  // 跳行：搜索结果点击后 `workspace-store.openAt` 置 `reveal`，这里消费。
  // 声明在"切 model"effect 之后 —— 同一个 commit 里先挂 model 再定位。
  useEffect(() => {
    const editor = editorRef.current
    if (!editor || !reveal || reveal.path !== activePath) return
    const lineCount = editor.getModel()?.getLineCount() ?? 0
    if (lineCount === 0) return
    // 结果可能来自搜索之后的旧行号（文件已改短）：夹到有效范围，别越界。
    const line = Math.min(Math.max(reveal.line, 1), lineCount)
    editor.setPosition({ lineNumber: line, column: 1 })
    editor.revealLineInCenter(line)
    editor.focus()
  }, [reveal, activePath])

  // 暂停：把当前执行行滚到视野中间。不抢焦点 —— 用户可能正在点调用栈。
  useEffect(() => {
    const editor = editorRef.current
    if (!editor || paused === null || paused.file !== activePath) return
    const lineCount = editor.getModel()?.getLineCount() ?? 0
    if (lineCount === 0) return
    const line = Math.min(Math.max(paused.line, 1), lineCount)
    editor.setPosition({ lineNumber: line, column: 1 })
    editor.revealLineInCenter(line)
  }, [paused, activePath])

  const submitCondition = () => {
    const target = menuRef.current
    if (target === null) return
    useDebugStore.getState().setBreakpointCondition(target.file, target.line, draft)
    setMenu(null)
  }

  return (
    <div className="editor-surface" ref={containerRef}>
      {menu !== null && (
        <>
          {/* 点空白处关菜单：透明遮罩，不吃编辑器的键盘事件。 */}
          <div
            className="breakpoint-menu__backdrop"
            onPointerDown={() => setMenu(null)}
            aria-hidden="true"
          />
          <div
            className="breakpoint-menu"
            style={{ left: `${menu.left}px`, top: `${menu.top}px` }}
            role="menu"
            aria-label={`第 ${menu.line} 行的断点`}
          >
            {menu.mode === 'menu' ? (
              <>
                <button
                  type="button"
                  role="menuitem"
                  className="breakpoint-menu__item"
                  onClick={() => {
                    setDraft(conditionAt(breakpoints[menu.file], menu.line) ?? '')
                    setMenu({ ...menu, mode: 'edit' })
                  }}
                >
                  编辑条件…
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="breakpoint-menu__item"
                  disabled={conditionAt(breakpoints[menu.file], menu.line) === null}
                  onClick={() => {
                    useDebugStore.getState().setBreakpointCondition(menu.file, menu.line, null)
                    setMenu(null)
                  }}
                >
                  清除条件
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="breakpoint-menu__item"
                  onClick={() => {
                    useDebugStore.getState().removeBreakpoint(menu.file, menu.line)
                    setMenu(null)
                  }}
                >
                  删除断点
                </button>
              </>
            ) : (
              <form
                className="breakpoint-menu__form"
                onSubmit={(event) => {
                  event.preventDefault()
                  submitCondition()
                }}
              >
                <label className="breakpoint-menu__label" htmlFor="breakpoint-condition">
                  第 {menu.line} 行的条件（同步 JS 表达式，留空 = 无条件）
                </label>
                <input
                  id="breakpoint-condition"
                  className="breakpoint-menu__input"
                  // 自动聚焦：右键之后直接用键盘输入是主路径，不必再点一下输入框。
                  autoFocus
                  value={draft}
                  maxLength={MAX_CONDITION_CHARS}
                  placeholder="例如 count > 3 && state.ready"
                  onChange={(event) => setDraft(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === 'Escape') {
                      event.preventDefault()
                      setMenu(null)
                    }
                  }}
                />
                <div className="breakpoint-menu__actions">
                  <button type="submit" className="button button--primary">
                    保存
                  </button>
                  <button type="button" className="button" onClick={() => setMenu(null)}>
                    取消
                  </button>
                </div>
                <p className="breakpoint-menu__hint">
                  条件在 isolate 里求值：抛错或编译失败一律当作没命中（每行只提示一次），
                  不支持 await。
                </p>
              </form>
            )}
          </div>
        </>
      )}
    </div>
  )
}