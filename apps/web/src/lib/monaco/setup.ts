/**
 * Monaco 初始化：只在 lazy chunk 里执行，首屏不加载。
 *
 * 用本地 `monaco-editor` 而不是 CDN：桌面端离线必须可用（AGENTS.md §6 动态 import）。
 * worker 用 Vite 的 `?worker` 打包成本地文件，避免运行时再去取远程脚本。
 */

import { loader } from '@monaco-editor/react'
import * as monaco from 'monaco-editor'
// 子路径走 monaco-editor 0.57 的 exports 映射（"./*" → "./esm/vs/*.js"）。
import EditorWorker from 'monaco-editor/editor/editor.worker?worker'
import JsonWorker from 'monaco-editor/languages/features/json/json.worker?worker'
import TsWorker from 'monaco-editor/languages/features/typescript/ts.worker?worker'

interface MonacoEnvironmentLike {
  getWorker(workerId: string, label: string): Worker
}

let configured = false

export function setupMonaco(): void {
  if (configured) return
  configured = true

  const environment: MonacoEnvironmentLike = {
    getWorker(_workerId: string, label: string): Worker {
      if (label === 'typescript' || label === 'javascript') return new TsWorker()
      if (label === 'json') return new JsonWorker()
      return new EditorWorker()
    },
  }
  // monaco-editor 的全局声明依赖加载顺序，这里显式收窄一次（无 any）。
  ;(self as unknown as { MonacoEnvironment: MonacoEnvironmentLike }).MonacoEnvironment = environment

  loader.config({ monaco })

  monaco.editor.defineTheme('hbot-dark', {
    base: 'vs-dark',
    inherit: true,
    rules: [
      { token: 'comment', foreground: '7f848e', fontStyle: 'italic' },
      { token: 'string', foreground: '9ece6a' },
      { token: 'keyword', foreground: '7aa2f7' },
      { token: 'number', foreground: 'ff9e64' },
      { token: 'type.identifier', foreground: '2ac3de' },
    ],
    colors: {
      'editor.background': '#1b1d22',
      'editor.foreground': '#d7dae0',
      'editorLineNumber.foreground': '#5a616e',
      'editorLineNumber.activeForeground': '#c8ccd4',
      'editor.selectionBackground': '#2f4770',
      'editor.lineHighlightBackground': '#24262c',
      'editorIndentGuide.background1': '#2c2f36',
      'editorCursor.foreground': '#4f8cff',
      'editorWidget.background': '#24262c',
      'editorWidget.border': '#34373d',
      'editorSuggestWidget.background': '#24262c',
      'editorSuggestWidget.selectedBackground': '#2f4770',
    },
  })

  // 0.57 把语言服务命名空间提到顶层：monaco.typescript / monaco.json 等。
  monaco.typescript.typescriptDefaults.setCompilerOptions({
    target: monaco.typescript.ScriptTarget.ESNext,
    module: monaco.typescript.ModuleKind.ESNext,
    moduleResolution: monaco.typescript.ModuleResolutionKind.NodeJs,
    strict: true,
    noEmit: true,
    allowNonTsExtensions: true,
  })
  monaco.typescript.typescriptDefaults.setDiagnosticsOptions({
    noSemanticValidation: false,
    noSyntaxValidation: false,
  })
}

/** Monaco 的 model 是全局资源：tab 关闭时由编辑器层显式 dispose。 */
export function disposeModel(uri: string): void {
  monaco.editor.getModel(monaco.Uri.parse(uri))?.dispose()
}