/**
 * 编辑器模型桥：把「某个 path 对应的 Monaco model」暴露给非 React 代码。
 *
 * 为什么需要桥：应用 Agent 编辑的逻辑（`lib/edit-apply.ts`）必须能在**不 import
 * monaco** 的前提下拿到模型 —— 否则 `features/agent/*` 会把 Monaco 拉进入口 chunk
 * （AGENTS.md §6：Monaco 只能动态引入）。真正的 model 由 lazy 的
 * `features/editor/editor-surface.tsx` 挂载时注册进来，单测注册假模型。
 */

import type { UndoableModel } from './edit-apply'

export type EditorModelProvider = (path: string) => UndoableModel | null

let provider: EditorModelProvider | null = null

/** 注册模型提供者，返回卸载函数（编辑器卸载 / HMR 时用）。 */
export function registerEditorModelProvider(next: EditorModelProvider): () => void {
  provider = next
  return () => {
    if (provider === next) provider = null
  }
}

/** 取某个 path 的模型；编辑器还没加载（或文件没打开）时返回 `null`。 */
export function editorModel(path: string): UndoableModel | null {
  return provider ? provider(path) : null
}

/** 有没有编辑器注册过（没有就是"纯磁盘模式"，应用只落盘、不碰编辑器）。 */
export function hasEditorModels(): boolean {
  return provider !== null
}