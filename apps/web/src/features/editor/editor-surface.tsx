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
 *
 * 行内编辑（docs/06-ide.md §3.2）：Cmd/Ctrl+K 把当前选区（空选区 = 光标所在行）连同
 * 一句自然语言指令发给 Agent，回来仍是既有的 diff-card 逐块审阅 —— 这里只负责"取选区、
 * 摆浮层、发请求"，不碰应用修改的逻辑。选区是**一次性快照**：Monaco 的 editor 实例仍是
 * 光标/选区的唯一真源，store 里不存第二份。
 */

import * as monaco from 'monaco-editor'
import { useEffect, useRef, useState } from 'react'
import type { DebugBreakpoint } from '../../generated'
import { languageForPath } from '../../lib/paths'
import { setupMonaco } from '../../lib/monaco/setup'
import {
  MAX_CONDITION_CHARS,
  MAX_LOG_CHARS,
  conditionAt,
  hasBreakpointAt,
  hasLogpointAt,
  logpointAt,
  useDebugStore,
} from '../../stores/debug-store'
import { breakpointGlyphs } from './breakpoint-decorations'
import { gutterMenuItems, type GutterMenuAction } from './breakpoint-menu'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { useAgentStore } from '../../stores/agent-store'
import { inlineEditPrompt } from '../../lib/agent-requests'
import {
  registerInlineEditOpener,
  selectionSnapshot,
  type InlineEditSelection,
} from '../../lib/inline-edit'
import { InlineEditPopover } from './inline-edit-popover'

const MODEL_SCHEME = 'hbot'

/** 浮层按"最坏情况"预留的尺寸，只用于把位置夹进容器（真实尺寸由 CSS 决定）。 */
const INLINE_EDIT_WIDTH = 360
const INLINE_EDIT_HEIGHT = 160

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max)
}

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
  /** `menu` = 动作列表；`edit` = 断点条件输入框；`log` = 日志点模板输入框。 */
  mode: 'menu' | 'edit' | 'log'
}

/** 行内编辑浮层的现场：选区快照 + 脏标记 + 位置。关闭即丢弃（不进 store）。 */
interface InlineEditState {
  selection: InlineEditSelection
  /** 打开浮层那一刻该文件是否有未保存改动（给用户看的，也是请求里的提示）。 */
  dirty: boolean
  left: number
  top: number
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
  const [inlineEdit, setInlineEdit] = useState<InlineEditState | null>(null)
  const [inlineError, setInlineError] = useState<string | null>(null)
  const [inlineBusy, setInlineBusy] = useState(false)
  const inlineEditRef = useRef<InlineEditState | null>(null)
  inlineEditRef.current = inlineEdit

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

    // 行槽位右键 = 该行断点/日志点的菜单（只在已有断点或日志点时出现：两样都没有时
    // 右键的意图不明确，而"加断点"本来左键一下就够了）。
    const gutterMenu = editor.onContextMenu((event) => {
      if (event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) return
      const line = event.target.position?.lineNumber
      const path = useWorkspaceStore.getState().activePath
      if (line === undefined || path === null) return
      const entries = useDebugStore.getState().breakpoints[path]
      if (!hasBreakpointAt(entries, line) && !hasLogpointAt(entries, line)) return
      // 拦掉 Monaco 自己的右键菜单：这一行的上下文就是断点/日志点，不是编辑器命令。
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
    // 换文件时浮层必须关掉：它记着上一个文件的选区，继续留着会发错请求。
    setInlineEdit(null)
    setInlineError(null)
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

  // 行内编辑浮层的 Esc：焦点可能已经落回编辑器（浮层不是模态的），所以也在 window 上兜一层。
  useEffect(() => {
    if (inlineEdit === null) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      setInlineEdit(null)
      setInlineError(null)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [inlineEdit])

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

  /**
   * 提交日志点模板。留空 = 删日志点（`setLogpoint` 那边把空串归一成 `null`），
   * 与"清空条件"一致：输入框清空是明确的删除动作，不该报错。
   */
  const submitLogpoint = () => {
    const target = menuRef.current
    if (target === null) return
    useDebugStore.getState().setLogpoint(target.file, target.line, draft)
    setMenu(null)
  }

  /** 菜单项 → 动作（`edit-*` 切成输入框，其余直接落到 store 并关菜单）。 */
  const runMenuAction = (action: GutterMenuAction): void => {
    const target = menuRef.current
    if (target === null) return
    const entries = useDebugStore.getState().breakpoints[target.file]
    switch (action) {
      case 'edit-condition':
        setDraft(conditionAt(entries, target.line) ?? '')
        setMenu({ ...target, mode: 'edit' })
        return
      case 'edit-logpoint':
        setDraft(logpointAt(entries, target.line) ?? '')
        setMenu({ ...target, mode: 'log' })
        return
      case 'add-logpoint':
        // 新日志点的模板默认给一个能直接用的示例？不给：用户要的是自己那行的变量，
        // 预填反而要先删掉。空输入框 + 占位符说明比预填好。
        setDraft('')
        setMenu({ ...target, mode: 'log' })
        return
      case 'clear-condition':
        useDebugStore.getState().setBreakpointCondition(target.file, target.line, null)
        break
      case 'remove-breakpoint':
        useDebugStore.getState().removeBreakpoint(target.file, target.line)
        break
      case 'remove-logpoint':
        useDebugStore.getState().removeLogpoint(target.file, target.line)
        break
    }
    setMenu(null)
  }

  /**
   * 打开行内编辑浮层：取当前选区（空选区 = 光标所在行）做成一次性快照。
   *
   * 返回 `false` = 这里没有可编辑的东西（编辑器未挂载 / 没开文件 / 选区取不到），
   * 由 App 走状态栏 toast 说明原因 —— 按了快捷键什么都不发生是最糟的反馈。
   */
  const openInlineEdit = (): boolean => {
    const editor = editorRef.current
    const container = containerRef.current
    const model = editor?.getModel() ?? null
    const path = useWorkspaceStore.getState().activePath
    if (!editor || !container || !model || path === null) return false

    const selection = editor.getSelection()
    const position = editor.getPosition()
    const anchor = selection?.positionLineNumber ?? position?.lineNumber
    if (anchor === undefined) return false
    const lines = Array.from({ length: model.getLineCount() }, (_, index) =>
      model.getLineContent(index + 1),
    )
    const snapshot = selectionSnapshot(
      path,
      lines,
      selection?.startLineNumber ?? anchor,
      selection?.endLineNumber ?? anchor,
    )
    if (snapshot === null) return false

    const state = useWorkspaceStore.getState()
    const dirty = state.tabs.some((tab) => tab.path === path && tab.dirty)
    const rect = container.getBoundingClientRect()
    const visible = editor.getScrolledVisiblePosition({ lineNumber: snapshot.startLine, column: 1 })
    const width = Math.min(INLINE_EDIT_WIDTH, Math.max(rect.width - 16, 200))
    const height = Math.min(INLINE_EDIT_HEIGHT, Math.max(rect.height - 16, 120))
    // 夹进容器：选区在最后几行时浮层不能掉到可视区外面去。
    const left = clamp(
      (visible?.left ?? 0) + 16,
      8,
      Math.max(8, rect.width - width - 8),
    )
    const top = clamp(
      (visible?.top ?? 0) + (visible?.height ?? 20) + 6,
      8,
      Math.max(8, rect.height - height - 8),
    )

    setInlineError(null)
    setInlineEdit({ selection: snapshot, dirty, left, top })
    return true
  }

  const openInlineEditRef = useRef(openInlineEdit)
  openInlineEditRef.current = openInlineEdit

  // 注册给全局快捷键（`app/use-global-keybindings.ts` 拿不到编辑器实例）。
  useEffect(() => registerInlineEditOpener(() => openInlineEditRef.current()), [])

  const cancelInlineEdit = (): void => {
    setInlineEdit(null)
    setInlineError(null)
  }

  /**
   * 提交：只发请求，不改任何文件。
   *
   * 失败**不关浮层**：关掉等于让用户以为请求已经发出去了。请求载荷里带
   * 「选区原文 + 文件相对路径 + 行范围 + 指令」，有未保存改动时再附一句提示
   * （docs/06-ide.md §5.2：Diff 以磁盘为基准）。
   */
  const submitInlineEdit = async (instruction: string): Promise<void> => {
    const target = inlineEditRef.current
    if (target === null) return
    const agent = useAgentStore.getState()
    // `send` 在 sending 为 true 时直接 return（不发也不报错），所以必须自己先拦。
    if (agent.sending) {
      setInlineError('Agent 正在处理上一条消息，等它回来再发。')
      return
    }
    if (agent.error !== null) agent.dismissError()
    setInlineError(null)
    setInlineBusy(true)
    try {
      await agent.send(
        inlineEditPrompt({ ...target.selection, instruction, dirty: target.dirty }),
      )
      const after = useAgentStore.getState()
      if (after.error !== null) {
        setInlineError(after.error)
        return
      }
      setInlineEdit(null)
      // 展开 Agent 栏：Diff 卡片回来时用户第一眼就能看到（docs/06-ide.md §5.2）。
      useWorkspaceStore.getState().toggleAgent(false)
    } finally {
      setInlineBusy(false)
    }
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
            aria-label={`第 ${menu.line} 行的断点与日志点`}
          >
            {menu.mode === 'menu' ? (
              gutterMenuItems({
                hasBreakpoint: hasBreakpointAt(breakpoints[menu.file], menu.line),
                condition: conditionAt(breakpoints[menu.file], menu.line),
                logpoint: logpointAt(breakpoints[menu.file], menu.line),
              }).map((item) => (
                <button
                  key={item.action}
                  type="button"
                  role="menuitem"
                  className="breakpoint-menu__item"
                  disabled={item.disabled}
                  onClick={() => runMenuAction(item.action)}
                >
                  {item.label}
                </button>
              ))
            ) : menu.mode === 'edit' ? (
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
            ) : (
              <form
                className="breakpoint-menu__form"
                onSubmit={(event) => {
                  event.preventDefault()
                  submitLogpoint()
                }}
              >
                <label className="breakpoint-menu__label" htmlFor="logpoint-template">
                  第 {menu.line} 行的日志点模板（{'{表达式}'} 在命中时展开，留空 = 删除日志点）
                </label>
                <input
                  id="logpoint-template"
                  className="breakpoint-menu__input"
                  autoFocus
                  value={draft}
                  maxLength={MAX_LOG_CHARS}
                  placeholder="例如 count={count}"
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
                  日志点不暂停：命中时进调试面板的日志列表，然后照常往下跑。
                  {'{表达式}'} 在 isolate 里求值，抛错就填 &lt;错误&gt;（每行只提示一次），
                  不支持嵌套花括号与 await。
                </p>
              </form>
            )}
          </div>
        </>
      )}
      {inlineEdit !== null && (
        <InlineEditPopover
          selection={inlineEdit.selection}
          dirty={inlineEdit.dirty}
          error={inlineError}
          busy={inlineBusy}
          left={inlineEdit.left}
          top={inlineEdit.top}
          onSubmit={(instruction) => void submitInlineEdit(instruction)}
          onCancel={cancelInlineEdit}
        />
      )}
    </div>
  )
}