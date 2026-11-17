/**
 * 「一次 Diff 应用 = 一个可整组撤销的动作」的端到端证据（真 store + 真 hooks + 假 model）。
 *
 * 这里刻意不 mock `accept-edit.ts` 与两个 store：undo 的正确性就藏在它们的接缝上
 * —— 快照什么时候拍、模型与缓冲区谁先退、`saved` 动不动、新建文件的标签删不删。
 * 只有 `dispatch` 是假的（node 下没有核心），并且假 model 按 Monaco 的
 * `pushStackElement` 闭合语义记 undo 组，所以"一次 Ctrl+Z"是真的能数出来的。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { EditFile } from '../../generated'
import type { ModelEditOperation, UndoableModel } from '../../lib/edit-apply'
import { registerEditorModelProvider } from '../../lib/editor-model-bridge'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { acceptEditInEditor, undoEditInEditor } from './accept-edit'

/** 假核心：磁盘内容 + 记录下来的命令（`edit.apply` 会按测试预先登记的"落盘结果"改盘）。 */
const core = vi.hoisted(() => {
  const disk = new Map<string, string>()
  /** 核心落盘之后磁盘应该是什么（测试按预期登记；假核心照抄）。 */
  const applied = new Map<string, string>()
  const commands: { type: string; path?: string }[] = []
  return { disk, applied, commands }
})

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  return {
    ...actual,
    dispatch: async (command: { type: string; path?: string }) => {
      core.commands.push(command)
      if (command.type === 'fs.read') {
        const content = core.disk.get(command.path ?? '')
        if (content === undefined) throw new Error(`ENOENT: ${command.path}`)
        return { path: command.path ?? '', content, version: 1 }
      }
      if (command.type === 'edit.apply') {
        for (const [path, content] of core.applied) core.disk.set(path, content)
      }
      return null
    },
  }
})

interface UndoGroup {
  before: string
  after: string
}

/** 把 Monaco 坐标（1-based 行列）换算成字符串下标（测试文本一律用 `\n`）。 */
function offsetOf(text: string, line: number, column: number): number {
  const lines = text.split('\n')
  let offset = 0
  for (let index = 0; index < line - 1 && index < lines.length; index += 1) {
    offset += (lines[index] ?? '').length + 1
  }
  return offset + (column - 1)
}

/** 一批编辑操作同时生效：按位置倒序替换，避免前面的改动把后面的下标顶偏。 */
function applyOperations(text: string, operations: readonly ModelEditOperation[]): string {
  const spans = operations
    .map((operation) => ({
      start: offsetOf(text, operation.range.startLineNumber, operation.range.startColumn),
      end: offsetOf(text, operation.range.endLineNumber, operation.range.endColumn),
      text: operation.text,
    }))
    .sort((left, right) => right.start - left.start)
  let out = text
  for (const span of spans) {
    out = out.slice(0, span.start) + span.text + out.slice(span.end)
  }
  return out
}

/**
 * 按 Monaco 的分组语义记账的假模型。
 *
 * `pushStackElement` 闭合当前元素，`pushEditOperations` 往**未闭合**的元素里追加
 * —— 与 `applyEditsAsOneUndoGroup` 的约定一致。所以 `groups.length === 1`
 * 等价于"用户按一次 Ctrl+Z 就退掉整次应用"。
 */
class FakeModel implements UndoableModel {
  private text: string
  private open: UndoGroup | null = null
  readonly groups: UndoGroup[] = []

  constructor(
    text: string,
    private readonly onChange: (text: string) => void,
  ) {
    this.text = text
  }

  getValue(): string {
    return this.text
  }

  pushStackElement(): void {
    this.close()
  }

  pushEditOperations(operations: readonly ModelEditOperation[]): void {
    const before = this.text
    const next = applyOperations(this.text, operations)
    if (next !== this.text) {
      this.text = next
      this.onChange(next)
    }
    if (this.open === null) this.open = { before, after: this.text }
    else this.open.after = this.text
  }

  /** 模拟用户直接在编辑器里敲字（只改文本并回写 store，不造 undo 组）。 */
  type(text: string): void {
    this.text = text
    this.onChange(text)
  }

  /** 模拟一次 Ctrl+Z：闭合未闭合的元素，退掉最后一个元素。 */
  undo(): boolean {
    this.close()
    const group = this.groups.pop()
    if (group === undefined) return false
    this.text = group.before
    this.onChange(this.text)
    return true
  }

  private close(): void {
    if (this.open === null) return
    this.groups.push(this.open)
    this.open = null
  }
}

const models = new Map<string, FakeModel>()
let unregister: (() => void) | null = null

function installProvider(): void {
  unregister = registerEditorModelProvider((path) => models.get(path) ?? null)
}

/** 打开一个文件：标签 + 缓冲区（与磁盘同一份字符串引用，与 `openFile` 口径一致）。 */
function openInEditor(path: string, text: string): FakeModel {
  const model = new FakeModel(text, (next) =>
    useWorkspaceStore.getState().updateContent(path, next),
  )
  models.set(path, model)
  const state = useWorkspaceStore.getState()
  useWorkspaceStore.setState({
    tabs: [...state.tabs, { path, dirty: false, version: 1 }],
    contents: { ...state.contents, [path]: text },
    saved: { ...state.saved, [path]: text },
    activePath: path,
  })
  return model
}

function ingestDiff(editId: string, files: EditFile[]): void {
  useAgentStore.getState().ingestEvent({ type: 'agent.diff', editId, files })
}

function hunk(
  index: number,
  start: number,
  oldLine: string,
  newLine: string,
): EditFile['hunks'][number] {
  return {
    index,
    oldStart: start,
    oldLines: 1,
    newStart: start,
    newLines: 1,
    lines: [`-${oldLine}`, `+${newLine}`],
  }
}

beforeEach(() => {
  models.clear()
  core.disk.clear()
  core.applied.clear()
  core.commands.length = 0
  useWorkspaceStore.setState({
    tabs: [],
    contents: {},
    saved: {},
    cursors: {},
    activePath: null,
    lastApply: null,
    diagnostics: [],
  })
  useAgentStore.setState({
    sessionId: null,
    messages: [],
    draft: '',
    sending: false,
    step: 0,
    tool: null,
    edit: null,
    approval: null,
    error: null,
    streamId: null,
  })
  installProvider()
})

afterEach(() => {
  unregister?.()
  unregister = null
})

describe('一次应用 = 一个 undo 组', () => {
  it('① 单文件两块：应用只产生一个 undo 组，Ctrl+Z 一次文本回到应用前', async () => {
    const before = 'a\nb\nc\nd'
    const after = 'a\nB\nc\nD'
    const model = openInEditor('src/a.ts', before)
    core.disk.set('src/a.ts', before)
    core.applied.set('src/a.ts', after)
    ingestDiff('edit-1', [
      { path: 'src/a.ts', hunks: [hunk(0, 2, 'b', 'B'), hunk(1, 4, 'd', 'D')] },
    ])

    await acceptEditInEditor()

    expect(model.getValue()).toBe(after)
    expect(useWorkspaceStore.getState().contents['src/a.ts']).toBe(after)
    // 两块 hunk 挤在同一个 undo 元素里：不是"每块一个组"，否则用户要按两次 Ctrl+Z。
    expect(model.groups).toHaveLength(1)

    expect(model.undo()).toBe(true)
    expect(model.getValue()).toBe(before)
    expect(useWorkspaceStore.getState().contents['src/a.ts']).toBe(before)
    expect(useWorkspaceStore.getState().lastApply?.editId).toBe('edit-1')
  })

  it('② 跨两个文件：一次「撤销这次应用」两个文件都回退', async () => {
    const modelA = openInEditor('src/a.ts', 'a1\na2')
    const modelB = openInEditor('src/b.ts', 'b1\nb2')
    core.disk.set('src/a.ts', 'a1\na2')
    core.disk.set('src/b.ts', 'b1\nb2')
    core.applied.set('src/a.ts', 'a1\nA2')
    core.applied.set('src/b.ts', 'B1\nb2')
    ingestDiff('edit-2', [
      { path: 'src/a.ts', hunks: [hunk(0, 2, 'a2', 'A2')] },
      { path: 'src/b.ts', hunks: [hunk(1, 1, 'b1', 'B1')] },
    ])

    await acceptEditInEditor()
    expect(modelA.getValue()).toBe('a1\nA2')
    expect(modelB.getValue()).toBe('B1\nb2')
    expect(useWorkspaceStore.getState().contents['src/a.ts']).toBe('a1\nA2')
    expect(useWorkspaceStore.getState().contents['src/b.ts']).toBe('B1\nb2')

    await undoEditInEditor()

    // Monaco 的撤销栈按 model 独立，所以这里必须两个都退回去 —— 这正是快照存在的理由。
    expect(modelA.getValue()).toBe('a1\na2')
    expect(modelB.getValue()).toBe('b1\nb2')
    expect(useWorkspaceStore.getState().contents['src/a.ts']).toBe('a1\na2')
    expect(useWorkspaceStore.getState().contents['src/b.ts']).toBe('b1\nb2')
    expect(useAgentStore.getState().edit?.status).toBe('undone')
  })

  it('③ 撤销只退缓冲区：saved（磁盘基线）不动，dirty 判断仍然正确', async () => {
    openInEditor('src/a.ts', 'x\ny')
    core.disk.set('src/a.ts', 'x\ny')
    core.applied.set('src/a.ts', 'x\nY')
    ingestDiff('edit-3', [{ path: 'src/a.ts', hunks: [hunk(0, 2, 'y', 'Y')] }])

    await acceptEditInEditor()
    const appliedState = useWorkspaceStore.getState()
    expect(appliedState.saved['src/a.ts']).toBe('x\nY')
    expect(appliedState.tabs[0]?.dirty).toBe(false)

    await undoEditInEditor()
    const undoneState = useWorkspaceStore.getState()
    expect(undoneState.contents['src/a.ts']).toBe('x\ny')
    // 磁盘上仍是应用后的内容（核心已经写过盘了）：基线不能被撤销顺手改掉，
    // 否则用户会以为磁盘也退回去了，下一次保存又把应用后的文本写一遍。
    expect(undoneState.saved['src/a.ts']).toBe('x\nY')
    expect(undoneState.tabs[0]?.dirty).toBe(true)
  })
})

describe('拒绝 / 撤销 / 不可再操作', () => {
  it('④ 拒绝不改变任何文本（模型与缓冲区都不动）', async () => {
    const model = openInEditor('src/a.ts', 'p\nq')
    core.disk.set('src/a.ts', 'p\nq')
    ingestDiff('edit-4', [{ path: 'src/a.ts', hunks: [hunk(0, 2, 'q', 'Q')] }])

    await useAgentStore.getState().rejectEdit()

    expect(model.getValue()).toBe('p\nq')
    expect(model.groups).toHaveLength(0)
    expect(useWorkspaceStore.getState().contents['src/a.ts']).toBe('p\nq')
    expect(core.disk.get('src/a.ts')).toBe('p\nq')
    expect(useAgentStore.getState().edit?.status).toBe('rejected')
    // 没有应用过，就没有可撤销的东西（拒绝 ≠ 撤销）。
    expect(useWorkspaceStore.getState().lastApply).toBeNull()
  })

  it('⑤ 撤销之后卡片不能再被接受/拒绝，也不能再撤销一次', async () => {
    const model = openInEditor('src/a.ts', 'm\nn')
    core.disk.set('src/a.ts', 'm\nn')
    core.applied.set('src/a.ts', 'm\nN')
    ingestDiff('edit-5', [{ path: 'src/a.ts', hunks: [hunk(0, 2, 'n', 'N')] }])

    await acceptEditInEditor()
    await undoEditInEditor()
    expect(useAgentStore.getState().edit?.status).toBe('undone')

    const commandsBefore = core.commands.length
    await useAgentStore.getState().acceptEdit()
    await useAgentStore.getState().rejectEdit()
    await undoEditInEditor()

    expect(useAgentStore.getState().edit?.status).toBe('undone')
    expect(model.getValue()).toBe('m\nn')
    // 一次应用只能撤销一次：快照用完即作废，再来一次不会再派发任何命令。
    expect(core.commands.length).toBe(commandsBefore)
  })
})

describe('与新建文件的交互', () => {
  it('⑥ 新建文件的 Diff 撤销后标签消失，但磁盘上的文件不删', async () => {
    // 应用前没有缓冲区 → `planEditApply` 判 skip → 快照里 before 为 null。
    core.applied.set('src/new.ts', 'hello')
    ingestDiff('edit-6', [
      {
        path: 'src/new.ts',
        hunks: [
          { index: 0, oldStart: 1, oldLines: 0, newStart: 1, newLines: 1, lines: ['+hello'] },
        ],
      },
    ])

    await acceptEditInEditor()
    // 接受本身不会打开标签（编辑器只跟随用户/树的动作）。
    expect(useWorkspaceStore.getState().tabs).toHaveLength(0)
    expect(useWorkspaceStore.getState().lastApply?.files[0]?.before).toBeNull()

    // 用户在应用之后点开了这个新文件（此时缓冲区与磁盘一致）。
    openInEditor('src/new.ts', 'hello')
    expect(useWorkspaceStore.getState().tabs).toHaveLength(1)

    await undoEditInEditor()

    const state = useWorkspaceStore.getState()
    expect(state.tabs).toHaveLength(0)
    expect(state.activePath).toBeNull()
    expect(state.contents['src/new.ts']).toBeUndefined()
    expect(state.saved['src/new.ts']).toBeUndefined()
    // 撤销不碰磁盘：核心已经创建了这个文件，web 层也没有"删除文件"命令。
    expect(core.disk.get('src/new.ts')).toBe('hello')
    expect(core.commands.filter((command) => command.type.includes('delete'))).toHaveLength(0)
  })

  it('⑥b 新文件被用户改脏之后，撤销不吞掉他的输入（标签留着）', async () => {
    core.applied.set('src/new.ts', 'hello')
    ingestDiff('edit-6b', [
      {
        path: 'src/new.ts',
        hunks: [
          { index: 0, oldStart: 1, oldLines: 0, newStart: 1, newLines: 1, lines: ['+hello'] },
        ],
      },
    ])
    await acceptEditInEditor()
    openInEditor('src/new.ts', 'hello')
    useWorkspaceStore.getState().updateContent('src/new.ts', 'hello world')

    await undoEditInEditor()

    const state = useWorkspaceStore.getState()
    expect(state.tabs).toHaveLength(1)
    expect(state.contents['src/new.ts']).toBe('hello world')
  })
})

describe('影响范围', () => {
  it('⑦ 撤销不动本次 Diff 之外的文件（含它自己的未保存改动）', async () => {
    openInEditor('src/a.ts', 'a\nb')
    const other = openInEditor('src/other.ts', 'o1\no2')
    core.disk.set('src/a.ts', 'a\nb')
    core.disk.set('src/other.ts', 'o1\no2')
    core.applied.set('src/a.ts', 'a\nB')
    // 另一个文件有用户自己的未保存改动（真机上是用户在 model 里敲的）。
    other.type('o1\nEDITED')
    ingestDiff('edit-7', [{ path: 'src/a.ts', hunks: [hunk(0, 2, 'b', 'B')] }])

    await acceptEditInEditor()
    const groupsBeforeUndo = other.groups.length
    await undoEditInEditor()

    const state = useWorkspaceStore.getState()
    expect(state.contents['src/a.ts']).toBe('a\nb')
    expect(state.contents['src/other.ts']).toBe('o1\nEDITED')
    expect(state.tabs.find((tab) => tab.path === 'src/other.ts')?.dirty).toBe(true)
    expect(other.getValue()).toBe('o1\nEDITED')
    // 没被这次应用碰过的 model 不该多出 undo 组（撤销不会给它造历史）。
    expect(other.groups).toHaveLength(groupsBeforeUndo)
  })
})
