/**
 * Agent 编辑落到编辑器：hunk → 行级编辑 → Monaco 编辑操作。
 *
 * 这里只有纯逻辑（不 import monaco、不 import store），三件事：
 * 1. **按缓冲区校验**：hunk 的行号与上下文行以**用户屏幕上的缓冲区**为准（不是磁盘）。
 *    对不上就拒绝该文件，绝不静默改写用户正在编辑的内容（docs/06-ide.md §5.2）。
 * 2. **一次应用 = 一个 undo 组**：`applyEditsAsOneUndoGroup` 只做
 *    `pushStackElement()` → `pushEditOperations(整批)` → `pushStackElement()`。
 * 3. 拒绝时产出可直接并进问题面板的诊断条目。
 *
 * 行号一律 1-based（与 `generated/DiffHunk` 的 `oldStart`/`oldLines` 一致）。
 * 不用 `setValue` 整文件替换：那会清空该文件整个 undo 历史，还丢光标与折叠状态。
 */

import type { DiffHunk, EditFile } from '../generated'
import type { Diagnostic } from './ipc'

/** 一次「整行替换」：把第 `startLine..endLine` 行换成 `lines`；`endLine < startLine` 表示纯插入。 */
export interface LineEdit {
  startLine: number
  endLine: number
  lines: string[]
}

/**
 * Monaco `IIdentifiedSingleEditOperation` 的结构子集。
 *
 * 自己不 import monaco：真实模型由 `lib/editor-model-bridge.ts` 注入，单测用假模型。
 * 形状与 `monaco.editor.IIdentifiedSingleEditOperation` 兼容，适配器里不用强转。
 */
export interface ModelEditOperation {
  range: {
    startLineNumber: number
    startColumn: number
    endLineNumber: number
    endColumn: number
  }
  text: string
  forceMoveMarkers: boolean
}

/** 一个文件在这次应用里的判定结果。 */
export type FileDecision =
  | {
      path: string
      action: 'apply'
      /** 应用前缓冲区文本。 */
      before: string
      /** 应用后缓冲区文本（= 用户屏幕上应该看到的内容）。 */
      after: string
      /** 应用后磁盘文本（脏文件才有区别：磁盘按 hunk 叠在磁盘版本上）。 */
      diskAfter: string
      dirty: boolean
      edits: LineEdit[]
    }
  /** 文件没在编辑器里打开：没有缓冲区可校验，只交给核心落盘。 */
  | { path: string; action: 'skip'; dirty: false }
  /** 拒绝该文件：上下文对不上缓冲区（用户有未保存改动时优先按这条拒）。 */
  | {
      path: string
      action: 'reject'
      dirty: boolean
      reason: 'context-mismatch'
      hunkIndex: number | null
      message: string
    }

export interface ApplyContext {
  /** 缓冲区当前文本（未打开的文件返回 `undefined`）。 */
  bufferOf: (path: string) => string | undefined
  /** 上一次读/写成功的磁盘文本。 */
  diskOf: (path: string) => string | undefined
  /** tab 上的脏标记（有未保存改动）。 */
  dirtyOf: (path: string) => boolean
}

export interface EditApplyPlan {
  decisions: FileDecision[]
  /** 拒绝类问题，可直接 `workspaceStore.reportProblems()` 进问题面板。 */
  problems: Diagnostic[]
}

/** 诊断条目的 `code`：前端产生的"应用被拒"（不是核心的编译/校验诊断）。 */
export const EDIT_APPLY_CONFLICT_CODE = 'web.edit-apply-conflict'

const HUMAN_HINT_DIRTY = '这个文件有未保存改动，Agent 的改动对不上缓冲区，先保存再应用。'
const HUMAN_HINT_DISK = 'Agent 的改动对不上当前内容，文件可能已被外部修改；先刷新再应用。'

/**
 * 文档的行尾符（写回时用它拼，别把 CRLF 文件改成混合行尾）。
 *
 * Monaco 会把自己的 EOL 规整掉，所以这里只影响**写得回磁盘/缓冲区文本**的那一侧。
 */
export function documentEol(content: string): '\n' | '\r\n' {
  return content.includes('\r\n') ? '\r\n' : '\n'
}

/**
 * 按行拆文本，并去掉行尾的 `\r`。
 *
 * 必须归一化：磁盘上的 CRLF 文件拆出来每行末尾都带 `\r`，而核心给的 hunk 上下文行
 * 不带 —— 不归一化就会把每个 CRLF 文件的每个 hunk 都判成"上下文对不上"，用户什么都应用不了。
 */
export function splitDocumentLines(content: string): string[] {
  return content.split('\n').map((line) => (line.endsWith('\r') ? line.slice(0, -1) : line))
}

/** 统一 diff 的一行拆成 old/new 两侧文本（`+` 只在 new、`-` 只在 old、空格两边都在）。 */
export function splitHunkLines(lines: readonly string[]): { oldLines: string[]; newLines: string[] } {
  const oldText: string[] = []
  const newText: string[] = []
  for (const line of lines) {
    const marker = line.slice(0, 1)
    const body = line.slice(1)
    if (marker === '+') {
      newText.push(body)
    } else if (marker === '-') {
      oldText.push(body)
    } else {
      // ' ' 是上下文；空串（有些 diff 会省掉行首空格）也当上下文。
      oldText.push(line.length === 1 ? '' : body)
      newText.push(line.length === 1 ? '' : body)
    }
  }
  return { oldLines: oldText, newLines: newText }
}

/**
 * 校验一个 hunk 能否落到 `lines` 上（`lines` 是被应用内容的逐行数组）。
 *
 * 只比 old 侧的上下文行与删除行：前导 `+` 行在原文里不存在，不参与比对。
 * 返回 `null` = 对得上；否则返回人话原因。
 */
export function verifyHunkAgainstLines(hunk: DiffHunk, lines: readonly string[]): string | null {
  const { oldLines } = splitHunkLines(hunk.lines)
  const start = hunk.oldStart
  if (!Number.isInteger(start) || start < 1) return `行号 ${start} 不合法`
  if (oldLines.length > 0) {
    for (let index = 0; index < oldLines.length; index += 1) {
      const actual = lines[start - 1 + index]
      if (actual === undefined) {
        return `第 ${start + index} 行超出文件末尾（文件共 ${lines.length} 行）`
      }
      if (actual !== oldLines[index]) return `第 ${start + index} 行对不上`
    }
  }
  return null
}

/** 把 hunk 变成一条行级替换（不做校验，调用方保证已过 `verifyHunkAgainstLines`）。 */
export function hunkToLineEdit(hunk: DiffHunk): LineEdit {
  const { oldLines, newLines } = splitHunkLines(hunk.lines)
  const startLine = hunk.oldStart
  return {
    startLine,
    // 0 行 old 侧 = 纯插入：endLine 比 startLine 小一行，表示"不替换任何原有行"。
    endLine: oldLines.length === 0 ? startLine - 1 : startLine + oldLines.length - 1,
    lines: newLines,
  }
}

/**
 * 按行替换算出新文本（纯函数；测试里就是"应用之后应该看到什么"）。
 *
 * `edits` 的行号以 `content` 的原始行为坐标（同一次应用里的多条编辑不互相偏移）。
 */
export function applyLineEdits(content: string, edits: readonly LineEdit[]): string {
  if (edits.length === 0) return content
  const eol = documentEol(content)
  const lines = splitDocumentLines(content)
  const sorted = [...edits].sort((left, right) => left.startLine - right.startLine)
  const out: string[] = []
  let cursor = 0
  for (const edit of sorted) {
    const startIndex = edit.startLine - 1
    if (startIndex > cursor) {
      for (; cursor < startIndex && cursor < lines.length; cursor += 1) {
        const line = lines[cursor]
        if (line === undefined) break
        out.push(line)
      }
    }
    // 替换掉 [startLine..endLine] 这些原有行（endLine < startLine 时一行都不吞）。
    const consumeTo = edit.endLine >= edit.startLine ? edit.endLine : cursor
    if (consumeTo > cursor) cursor = Math.min(consumeTo, lines.length)
    out.push(...edit.lines)
  }
  for (; cursor < lines.length; cursor += 1) {
    const line = lines[cursor]
    if (line === undefined) break
    out.push(line)
  }
  return out.join(eol)
}

/**
 * 行级编辑 → Monaco 编辑操作（坐标全部相对**应用前**的文本，同一批操作同时生效）。
 *
 * 空模型、纯插入、插到文件末尾三种边界都单独处理：插入到末尾时 Monaco 要求
 * 把 `\n` 放在插入文本前面（否则会粘到最后一行上）。
 */
export function buildEditOperations(
  edits: readonly LineEdit[],
  content: string,
): ModelEditOperation[] {
  if (edits.length === 0) return []
  const lines = splitDocumentLines(content)
  const lineCount = lines.length
  const operations: ModelEditOperation[] = []

  for (const edit of [...edits].sort((left, right) => left.startLine - right.startLine)) {
    const text = edit.lines.join('\n')
    if (edit.endLine >= edit.startLine) {
      const endLineText = lines[edit.endLine - 1] ?? ''
      operations.push({
        range: {
          startLineNumber: edit.startLine,
          startColumn: 1,
          endLineNumber: edit.endLine,
          endColumn: endLineText.length + 1,
        },
        text,
        forceMoveMarkers: true,
      })
      continue
    }
    // 纯插入：old 侧 0 行。
    if (edit.startLine > lineCount) {
      const lastLine = lines[lineCount - 1] ?? ''
      const lineNumber = Math.max(lineCount, 1)
      const column = lastLine.length + 1
      operations.push({
        range: {
          startLineNumber: lineNumber,
          startColumn: column,
          endLineNumber: lineNumber,
          endColumn: column,
        },
        text: lineCount === 0 ? text : `\n${text}`,
        forceMoveMarkers: true,
      })
      continue
    }
    operations.push({
      range: {
        startLineNumber: edit.startLine,
        startColumn: 1,
        endLineNumber: edit.startLine,
        endColumn: 1,
      },
      // 在 startLine 之前插入：插入文本自己带一个换行，才顶成一整行。
      text: `${text}\n`,
      forceMoveMarkers: true,
    })
  }
  return operations
}

/**
 * 带 undo 边界的最小模型接口（真实实现由 `editor-model-bridge.ts` 适配 Monaco）。
 * 只列用到的三个方法 —— 单测的假模型也只有这三个。
 */
export interface UndoableModel {
  getValue: () => string
  pushEditOperations: (operations: readonly ModelEditOperation[]) => void
  pushStackElement: () => void
}

/**
 * 把一批行级编辑应用到模型，**整批只产生一个 undo 元素**。
 *
 * Monaco 的 undo 元素语义（`node_modules/monaco-editor/esm/vs/editor/common/model/editStack.js`
 * 的 `EditStack._getOrCreateEditStackElement` / `SingleModelEditStackElement.canAppend`）：
 * - `pushEditOperations` 会往**当前未闭合**的元素里追加；闭合过的元素不可再追加；
 * - `pushStackElement()` 闭合当前元素（`EditStack.pushStackElement` → `lastElement.close()`）。
 *
 * 所以顺序必须是「先闭合上一个（用户刚打的字 / 上一次应用）→ 一次 pushEditOperations
 * 带上全部 hunk → 再闭合」。一次应用 = 一个元素；连续两次应用 = 两个元素。
 * 返回实际产生的编辑操作条数（0 = 没有可应用的编辑）。
 */
export function applyEditsAsOneUndoGroup(
  model: UndoableModel,
  edits: readonly LineEdit[],
): number {
  if (edits.length === 0) return 0
  const operations = buildEditOperations(edits, model.getValue())
  if (operations.length === 0) return 0
  model.pushStackElement()
  model.pushEditOperations(operations)
  model.pushStackElement()
  return operations.length
}

function conflictDiagnostic(
  path: string,
  hunk: DiffHunk | undefined,
  message: string,
): Diagnostic {
  return {
    path,
    line: hunk && hunk.oldStart > 0 ? hunk.oldStart : 0,
    column: 1,
    severity: 'error',
    message,
    code: EDIT_APPLY_CONFLICT_CODE,
  }
}

/**
 * 规划一次 `EditApply`：逐文件判定「应用 / 跳过 / 拒绝」，并产出问题面板条目。
 *
 * 关键点：**校验和偏移都按缓冲区算**（`bufferOf`），磁盘文本只用来算
 * "应用后磁盘应该是什么"（决定脏标记），不参与行号定位 —— 否则用户有未保存改动时
 * hunk 的行号会整体错位。
 *
 * 一个文件被拒不等于整次应用失败：其余文件照常应用（`decisions` 里各拿各的结论）。
 */
export function planEditApply(
  files: readonly EditFile[],
  context: ApplyContext,
  acceptHunks?: readonly number[],
): EditApplyPlan {
  const decisions: FileDecision[] = []
  const problems: Diagnostic[] = []
  const accepted = acceptHunks === undefined ? null : new Set(acceptHunks)

  for (const file of files) {
    const hunks = accepted === null ? file.hunks : file.hunks.filter((hunk) => accepted.has(hunk.index))
    const dirty = context.dirtyOf(file.path)
    const buffer = context.bufferOf(file.path)
    const disk = context.diskOf(file.path)

    if (buffer === undefined) {
      // 没打开就没有缓冲区可校验：不碰编辑器，直接交给核心。
      decisions.push({ path: file.path, action: 'skip', dirty: false })
      continue
    }

    const bufferLines = splitDocumentLines(buffer)
    const edits: LineEdit[] = []
    let rejectedHunk: DiffHunk | undefined
    let rejectReason: string | null = null
    for (const hunk of hunks) {
      const mismatch = verifyHunkAgainstLines(hunk, bufferLines)
      if (mismatch !== null) {
        rejectedHunk = hunk
        rejectReason = mismatch
        break
      }
      edits.push(hunkToLineEdit(hunk))
    }

    if (rejectReason !== null) {
      const message = dirty
        ? `${HUMAN_HINT_DIRTY}（${file.path} 第 ${rejectedHunk?.oldStart ?? 0} 行：${rejectReason}）`
        : `${HUMAN_HINT_DISK}（${file.path} 第 ${rejectedHunk?.oldStart ?? 0} 行：${rejectReason}）`
      decisions.push({
        path: file.path,
        action: 'reject',
        dirty,
        reason: 'context-mismatch',
        hunkIndex: rejectedHunk?.index ?? null,
        message,
      })
      problems.push(conflictDiagnostic(file.path, rejectedHunk, message))
      continue
    }

    decisions.push({
      path: file.path,
      action: 'apply',
      before: buffer,
      after: applyLineEdits(buffer, edits),
      diskAfter: disk === undefined ? applyLineEdits(buffer, edits) : applyLineEdits(disk, edits),
      dirty,
      edits,
    })
  }

  return { decisions, problems }
}

/**
 * 「接受 Diff」的两处挂钩（由组件层注入，见 `features/agent/accept-edit.ts`）。
 *
 * 为什么要挂钩而不是让 `agent-store` 直接 import `workspace-store`：铁律是 store 之间
 * 不互相 import（AGENTS.md）。`agent-store.acceptEdit(hooks)` 只负责"调核心 + 改自身状态"，
 * 需要缓冲区/模型的事情由组件层通过这两个回调接进来；不传 hooks 时行为与从前完全一致
 * （纯 `edit.apply` 落盘），所以既有单测不用改。
 */
export interface EditApplyHooks {
  /**
   * 落盘前：按**缓冲区**校验 hunk，返回仍然要接受的 hunk 下标。
   * 被拒的文件其 hunk 全部剔除（核心就不会写它们），问题由这个回调自己报进面板。
   */
  precheck?: (input: {
    editId: string
    files: readonly EditFile[]
    acceptHunks: readonly number[]
  }) => { acceptHunks: number[] }
  /** 落盘成功后：把同一批 hunk 应用到编辑器模型（每个模型一个 undo 组）。 */
  afterApply?: (input: {
    editId: string
    files: readonly EditFile[]
    acceptHunks: readonly number[]
  }) => Promise<void> | void
}