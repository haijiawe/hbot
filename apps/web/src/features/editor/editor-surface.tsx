/**
 * Monaco 编辑器（lazy chunk）。
 *
 * 一个文件一个 model：切 tab 只 `setModel`，不重建成字符串；关 tab 由 openKey 的
 * 变化统一回收 model。模型里的文本是唯一真源，store 只保存副本用于脏标记与保存。
 *
 * 断点：行槽位（glyph margin）点击 → `debugStore.toggleBreakpoint`，断点点由这里
 * 打装饰；暂停行另有一层整行高亮。调试面板本身在底部面板的"调试"页签里，这里只管
 * "让断点与当前执行行在代码上看得见"。
 */

import * as monaco from 'monaco-editor'
import { useEffect, useRef } from 'react'
import { languageForPath } from '../../lib/paths'
import { setupMonaco } from '../../lib/monaco/setup'
import { useDebugStore } from '../../stores/debug-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

const MODEL_SCHEME = 'hbot'

function modelUri(path: string): monaco.Uri {
  return monaco.Uri.from({ scheme: MODEL_SCHEME, path: `/${path}` })
}

function pathFromUri(uri: monaco.Uri): string {
  return uri.path.startsWith('/') ? uri.path.slice(1) : uri.path
}

/** 断点行号 → glyph margin 上的圆点装饰（只给有效范围内的行）。 */
function breakpointDecorations(
  lines: readonly number[],
  lineCount: number,
): monaco.editor.IModelDeltaDecoration[] {
  return lines
    .filter((line) => line >= 1 && line <= lineCount)
    .map((line) => ({
      range: new monaco.Range(line, 1, line, 1),
      options: { glyphMarginClassName: 'debug-breakpoint' },
    }))
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

    // 行槽位点击 = 切换该行断点。行号区域的点击仍是正常选行，不做处理。
    const gutterClick = editor.onMouseDown((event) => {
      if (event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) return
      const line = event.target.position?.lineNumber
      const path = useWorkspaceStore.getState().activePath
      if (line === undefined || path === null) return
      useDebugStore.getState().toggleBreakpoint(path, line)
    })

    return () => {
      gutterClick.dispose()
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
    const lines = activePath === null ? [] : (breakpoints[activePath] ?? [])
    const next = breakpointDecorations(lines, lineCount)
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

  return <div className="editor-surface" ref={containerRef} />
}