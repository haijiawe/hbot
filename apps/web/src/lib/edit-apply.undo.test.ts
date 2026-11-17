/**
 * 撤销分组的纯逻辑证据：快照只收"应用过/跳过的文件"、恢复动作走一个 undo 组。
 *
 * 端到端行为在 `features/agent/edit-undo.test.ts`；这里钉的是"快照内容长什么样"
 * 和"恢复到底怎么调 Monaco"—— 后者一旦改成 `setValue`，用户应用前的历史编辑会静默消失。
 */

import { describe, expect, it } from 'vitest'
import type { FileDecision, ModelEditOperation, UndoableModel } from './edit-apply'
import { captureApplySnapshot, restoreModelAsOneUndoGroup } from './edit-apply'

describe('captureApplySnapshot', () => {
  it('应用过的文件记应用前文本，跳过的记 null，被拒的不进快照', () => {
    const decisions: FileDecision[] = [
      {
        path: 'src/a.ts',
        action: 'apply',
        before: 'old a',
        after: 'new a',
        diskAfter: 'new a',
        dirty: false,
        edits: [],
      },
      { path: 'src/new.ts', action: 'skip', dirty: false },
      {
        path: 'src/b.ts',
        action: 'reject',
        dirty: true,
        reason: 'context-mismatch',
        hunkIndex: 3,
        message: '对不上',
      },
    ]

    const snapshot = captureApplySnapshot('edit-1', decisions)

    expect(snapshot.editId).toBe('edit-1')
    expect(snapshot.files).toEqual([
      { path: 'src/a.ts', before: 'old a' },
      { path: 'src/new.ts', before: null },
    ])
  })

  it('before 存的是同一个字符串引用（浅快照，不拷贝整个 workspace）', () => {
    const before = 'a\nb\nc'
    const decisions: FileDecision[] = [
      {
        path: 'src/a.ts',
        action: 'apply',
        before,
        after: 'x',
        diskAfter: 'x',
        dirty: false,
        edits: [],
      },
    ]

    expect(captureApplySnapshot('edit-1', decisions).files[0]?.before).toBe(before)
  })
})

describe('restoreModelAsOneUndoGroup', () => {
  function fakeModel(value: string): UndoableModel & {
    calls: string[]
    operations: readonly ModelEditOperation[][]
  } {
    let current = value
    const calls: string[] = []
    const operations: ModelEditOperation[][] = []
    return {
      calls,
      operations,
      getValue: () => current,
      pushStackElement: () => void calls.push('pushStackElement'),
      pushEditOperations: (ops) => {
        calls.push('pushEditOperations')
        operations.push([...ops])
        // 假模型只认"整份替换"：这条路径本来就只发一个覆盖全文的操作。
        const single = ops[0]
        if (single) current = single.text
      },
    }
  }

  it('整份恢复一次成一个 undo 组（闭合 → 替换 → 闭合）', () => {
    const model = fakeModel('line1\nline2')

    expect(restoreModelAsOneUndoGroup(model, 'old1\nold2')).toBe(true)

    expect(model.calls).toEqual(['pushStackElement', 'pushEditOperations', 'pushStackElement'])
    expect(model.operations[0]?.[0]?.range).toEqual({
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 2,
      endColumn: 6,
    })
    expect(model.getValue()).toBe('old1\nold2')
  })

  it('文本本来就一样时不动模型（不产生空的 undo 组）', () => {
    const model = fakeModel('same')

    expect(restoreModelAsOneUndoGroup(model, 'same')).toBe(false)
    expect(model.calls).toEqual([])
  })

  it('空模型恢复成有内容的文本也不越界（range 落在第 1 行第 1 列）', () => {
    const model = fakeModel('')

    expect(restoreModelAsOneUndoGroup(model, 'hello')).toBe(true)

    expect(model.operations[0]?.[0]?.range).toEqual({
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 1,
      endColumn: 1,
    })
    expect(model.getValue()).toBe('hello')
  })
})
