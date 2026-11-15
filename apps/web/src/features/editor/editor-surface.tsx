/**
 * Monaco 编辑器（lazy chunk）。
 *
 * 一个文件一个 model：切 tab 只 `setModel`，不重建成字符串；关 tab 由 openKey 的
 * 变化统一回收 model。模型里的文本是唯一真源，store 只保存副本用于脏标记与保存。
 */

import * as monaco from 'monaco-editor'
import { useEffect, useRef } from 'react'
import { languageForPath } from '../../lib/paths'
import { setupMonaco } from '../../lib/monaco/setup'
import { useWorkspaceStore } from '../../stores/workspace-store'

const MODEL_SCHEME = 'hbot'

function modelUri(path: string): monaco.Uri {
  return monaco.Uri.from({ scheme: MODEL_SCHEME, path: `/${path}` })
}

function pathFromUri(uri: monaco.Uri): string {
  return uri.path.startsWith('/') ? uri.path.slice(1) : uri.path
}

export default function EditorSurface() {
  const activePath = useWorkspaceStore((state) => state.activePath)
  const openKey = useWorkspaceStore((state) => state.tabs.map((tab) => tab.path).join('\n'))
  const updateContent = useWorkspaceStore((state) => state.updateContent)
  const containerRef = useRef<HTMLDivElement | null>(null)
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null)
  const subscriptionsRef = useRef(new Map<string, monaco.IDisposable>())

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
      tabSize: 2,
      insertSpaces: true,
      padding: { top: 8, bottom: 8 },
      ariaLabel: '代码编辑器',
    })
    editorRef.current = editor

    return () => {
      for (const subscription of subscriptionsRef.current.values()) subscription.dispose()
      subscriptionsRef.current.clear()
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

  // 关闭 tab：回收对应 model 与监听。
  useEffect(() => {
    const stillOpen = new Set(openKey.split('\n').filter((path) => path.length > 0))
    for (const model of monaco.editor.getModels()) {
      if (model.uri.scheme !== MODEL_SCHEME) continue
      const path = pathFromUri(model.uri)
      if (stillOpen.has(path)) continue
      subscriptionsRef.current.get(path)?.dispose()
      subscriptionsRef.current.delete(path)
      model.dispose()
    }
  }, [openKey])

  return <div className="editor-surface" ref={containerRef} />
}