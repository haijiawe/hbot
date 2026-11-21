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

  // 浅色主题（Mino 的纸面）：与 styles/theme.css 的浅色调色板同源。
  // base 用 'vs' 而不是自定义全量规则 —— 语法高亮覆盖不全时至少还有可读的默认色。
  monaco.editor.defineTheme('hbot-light', {
    base: 'vs',
    inherit: true,
    rules: [
      { token: 'comment', foreground: '8a7a6b', fontStyle: 'italic' },
      { token: 'string', foreground: '1f7f60' },
      { token: 'keyword', foreground: 'b34f15' },
      { token: 'number', foreground: '8a5f0d' },
      { token: 'type.identifier', foreground: '3f72c9' },
    ],
    colors: {
      'editor.background': '#fffdf9',
      'editor.foreground': '#211a14',
      'editorLineNumber.foreground': '#a99a8b',
      'editorLineNumber.activeForeground': '#211a14',
      'editor.selectionBackground': '#f7e3d2',
      'editor.lineHighlightBackground': '#f6f1ea',
      'editorIndentGuide.background1': '#e7ded2',
      'editorCursor.foreground': '#b34f15',
      'editorWidget.background': '#fffdf9',
      'editorWidget.border': '#e6dbcd',
      'editorSuggestWidget.background': '#fffdf9',
      'editorSuggestWidget.selectedBackground': '#f7e3d2',
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

/** 主题名：Monaco 只认自己注册过的名字，所以这里做一次映射而不是把字符串散在组件里。 */
export function monacoTheme(theme: 'light' | 'dark'): string {
  return theme === 'dark' ? 'hbot-dark' : 'hbot-light'
}
