/**
 * 行内编辑（Cmd/Ctrl+K）的两块拼图：**选区快照** 与 **桥**。
 *
 * 1. 选区唯一真源是 Monaco 编辑器实例（`features/editor/editor-surface.tsx` 里的
 *    `editor.getSelection()`）。这里不做第二份状态：`selectionSnapshot` 只在按下快捷键
 *    的那一刻把选区**转成一次性的纯数据**交出去，存的地方是组件 state，随浮层关闭即丢弃。
 * 2. 全局快捷键（`app/use-global-keybindings.ts`）手里没有编辑器实例，只能通过这条桥
 *    问一句"现在有没有编辑器、能不能开浮层"。编辑器还没挂载（主编辑区懒加载未完成 /
 *    没有打开的文件）时 `requestInlineEdit()` 返回 `false`，调用方（`App.tsx`）用既有的
 *    状态栏 toast 说明原因 —— 快捷键按下去什么都不发生是最糟的反馈。
 */

export interface InlineEditSelection {
  /** 文件相对路径。 */
  path: string
  /** 1-based，闭区间。 */
  startLine: number
  endLine: number
  /** 选区原文（整行切片，行间 `\n`）。 */
  text: string
}

/**
 * 把"某几行"变成快照；越界/空文件返回 `null`。
 *
 * 行号会被夹到 `[1, lines.length]`：快捷键可能命中一个刚被外部改短的文件（核心写盘后
 * 编辑器还没换 model），越界在这里挡住，别把 `undefined` 拼进请求里。
 */
export function selectionSnapshot(
  path: string,
  lines: readonly string[],
  startLine: number,
  endLine: number,
): InlineEditSelection | null {
  if (path.length === 0 || lines.length === 0) return null
  const rawFirst = Math.min(startLine, endLine)
  const rawLast = Math.max(startLine, endLine)
  // 两端都夹进 [1, 行数]：否则"文件被写短 + 旧行号"会得到 first > last 的空快照。
  const first = Math.min(Math.max(rawFirst, 1), lines.length)
  const last = Math.min(Math.max(rawLast, 1), lines.length)
  if (first > last) return null
  return {
    path,
    startLine: first,
    endLine: last,
    text: lines.slice(first - 1, last).join('\n'),
  }
}

/** 能开行内编辑浮层的组件（目前只有 `EditorSurface`）；返回 `true` = 浮层已打开。 */
export type InlineEditOpener = () => boolean

let opener: InlineEditOpener | null = null

/** 注册打开器，返回注销函数（编辑器卸载 / HMR 时用）。 */
export function registerInlineEditOpener(next: InlineEditOpener): () => void {
  opener = next
  return () => {
    if (opener === next) opener = null
  }
}

/** 请求打开行内编辑浮层；没有编辑器接收时返回 `false`。 */
export function requestInlineEdit(): boolean {
  return opener === null ? false : opener()
}