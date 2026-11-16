/**
 * parity #16「Agent 写入不覆盖用户未保存修改」的证据。
 *
 * 这里钉住四件事（全部走真实的 `planEditApply` / `applyLineEdits` / `applyEditsAsOneUndoGroup`）：
 * 1. 干净文件、hunk 与磁盘对得上 → 正常应用，内容正确；
 * 2. 脏文件**不是**整份跳过：缓冲区与 hunk 对得上就应用到缓冲区，用户别处的未保存字照旧保留；
 * 3. 脏文件里 hunk 的区域被用户改过 → 拒绝该文件（`HUMAN_HINT_DIRTY` + 冲突诊断），
 *    不碰缓冲区、不静默写入；
 * 4. hunk 对不上（越界 / 上下文不符 / 纯插入的锚点被前面的脏行顶偏）→ 拒绝并给原因。
 *
 * 纯逻辑测试，环境用默认的 node（组件测试才需要 jsdom）。
 */

import { describe, expect, it } from 'vitest'
import type { DiffHunk, EditFile } from '../generated'
import type { ApplyContext, LineEdit, ModelEditOperation, UndoableModel } from './edit-apply'
import {
  EDIT_APPLY_CONFLICT_CODE,
  applyEditsAsOneUndoGroup,
  applyLineEdits,
  hunkToLineEdit,
  planEditApply,
  splitDocumentLines,
  splitHunkLines,
  verifyHunkAgainstLines,
} from './edit-apply'

/** 造一个 hunk（`index` 默认 0；`lines` 是统一的 `-`/`+` 前缀行）。 */
function makeHunk(
  oldStart: number,
  lines: string[],
  overrides: Partial<Pick<DiffHunk, 'index' | 'newStart'>> = {},
): DiffHunk {
  const { oldLines, newLines } = splitHunkLines(lines)
  const oldCount = oldLines.length
  const newCount = newLines.length
  return {
    index: overrides.index ?? 0,
    oldStart,
    oldLines: oldCount,
    newStart: overrides.newStart ?? oldStart,
    newLines: newCount,
    lines,
  }
}

function makeFile(path: string, hunks: DiffHunk[]): EditFile {
  return { path, hunks }
}

/** 最小上下文：默认"文件没打开、不脏、没有磁盘副本"。 */
function makeContext(overrides: Partial<ApplyContext> = {}): ApplyContext {
  return {
    bufferOf: () => undefined,
    diskOf: () => undefined,
    dirtyOf: () => false,
    ...overrides,
  }
}

describe('planEditApply：干净文件照常应用', () => {
  it('缓冲区与磁盘一致、hunk 上下文对得上 → 应用，after / diskAfter 都正确', () => {
    const disk = 'a\nb\nc\n'
    const file = makeFile('src/a.ts', [makeHunk(2, ['-b', '+B'])])
    const plan = planEditApply(
      [file],
      makeContext({
        bufferOf: () => disk,
        diskOf: () => disk,
        dirtyOf: () => false,
      }),
    )

    expect(plan.problems).toEqual([])
    expect(plan.decisions).toHaveLength(1)
    const decision = plan.decisions[0]
    expect(decision).toMatchObject({ path: 'src/a.ts', action: 'apply', dirty: false })
    if (decision?.action !== 'apply') return
    expect(decision.before).toBe(disk)
    expect(decision.after).toBe('a\nB\nc\n')
    expect(decision.diskAfter).toBe('a\nB\nc\n')
    expect(decision.edits).toEqual([{ startLine: 2, endLine: 2, lines: ['B'] }])
  })

  it('文件没在编辑器里打开（没有缓冲区）→ skip，且 dirty 恒为 false', () => {
    const file = makeFile('src/closed.ts', [makeHunk(1, ['-a', '+A'])])
    const plan = planEditApply(
      [file],
      makeContext({ bufferOf: () => undefined, dirtyOf: () => true }),
    )

    expect(plan.decisions).toEqual([{ path: 'src/closed.ts', action: 'skip', dirty: false }])
    expect(plan.problems).toEqual([])
  })
})

describe('planEditApply：脏文件（未保存修改）', () => {
  it('hunk 落在用户没动过的行上 → 应用到缓冲区，用户别处的未保存字一个不丢', () => {
    // 磁盘 a/b/c，用户把第 3 行改成 C（未保存）；Agent 要改第 2 行 b→B。
    const disk = 'a\nb\nc\n'
    const buffer = 'a\nb\nC\n'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(2, ['-b', '+B'])])],
      makeContext({ bufferOf: () => buffer, diskOf: () => disk, dirtyOf: () => true }),
    )

    const decision = plan.decisions[0]
    // 脏 ≠ 整份跳过：能对上就应用（这正是"覆盖"与"不覆盖"的分水岭）。
    expect(decision).toMatchObject({ path: 'src/a.ts', action: 'apply', dirty: true })
    if (decision?.action !== 'apply') return
    expect(decision.before).toBe(buffer)
    expect(decision.after).toBe('a\nB\nC\n')
    expect(decision.after).toContain('C')
    // 磁盘侧的预期结果按磁盘叠 hunk，不受用户未保存改动影响。
    expect(decision.diskAfter).toBe('a\nB\nc\n')
  })

  it('hunk 的区域被用户改过 → 拒绝该文件，缓冲区一字未改，诊断带 HUMAN_HINT_DIRTY', () => {
    // 磁盘第 2 行 b，用户在缓冲区里改成了 B_EDIT；Agent 的 hunk 仍按磁盘算作 b→B。
    const disk = 'a\nb\nc\n'
    const buffer = 'a\nB_EDIT\nc\n'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(2, ['-b', '+B'])])],
      makeContext({ bufferOf: () => buffer, diskOf: () => disk, dirtyOf: () => true }),
    )

    expect(plan.decisions[0]).toMatchObject({
      path: 'src/a.ts',
      action: 'reject',
      dirty: true,
      reason: 'context-mismatch',
      hunkIndex: 0,
    })
    const rejected = plan.decisions[0]
    if (rejected?.action !== 'reject') return
    expect(rejected.message).toContain('这个文件有未保存改动')
    expect(rejected.message).toContain('src/a.ts')
    expect(rejected.message).toContain('第 2 行对不上')
    // 拒绝类问题进问题面板，code 可被 UI 区分出来。
    expect(plan.problems).toHaveLength(1)
    expect(plan.problems[0]).toMatchObject({
      path: 'src/a.ts',
      severity: 'error',
      code: EDIT_APPLY_CONFLICT_CODE,
    })
    // 没有任何 apply 决定 ⇒ 缓冲区没有被写回。
    expect(plan.decisions.some((item) => item.action === 'apply')).toBe(false)
  })
})

describe('planEditApply：hunk 对不上就拒绝，不许静默写入', () => {
  it('干净文件、oldStart 超出文件末尾 → 拒绝，提示"对不上当前内容"', () => {
    const disk = 'a\nb\n'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(9, ['-x', '+X'])])],
      makeContext({ bufferOf: () => disk, diskOf: () => disk, dirtyOf: () => false }),
    )

    const decision = plan.decisions[0]
    expect(decision).toMatchObject({
      path: 'src/a.ts',
      action: 'reject',
      dirty: false,
      reason: 'context-mismatch',
    })
    if (decision?.action !== 'reject') return
    expect(decision.message).toContain('Agent 的改动对不上当前内容')
    expect(decision.message).toContain('超出文件末尾')
    expect(plan.problems[0]?.code).toBe(EDIT_APPLY_CONFLICT_CODE)
  })

  it('干净文件、上下文行不符 → 拒绝', () => {
    const disk = 'a\nb\n'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(1, ['-zzz', '+X'])])],
      makeContext({ bufferOf: () => disk, diskOf: () => disk }),
    )

    expect(plan.decisions[0]).toMatchObject({ action: 'reject', reason: 'context-mismatch' })
    expect(plan.problems).toHaveLength(1)
  })

  it('同一批里脏文件冲突、干净文件正常 → 逐文件判定，干净照常应用', () => {
    const files = [
      makeFile('src/dirty.ts', [makeHunk(2, ['-b', '+B'])]),
      makeFile('src/clean.ts', [makeHunk(2, ['-y', '+Y'])]),
    ]
    const plan = planEditApply(files, {
      bufferOf: (path) => (path === 'src/dirty.ts' ? 'a\nUSER\n' : 'x\ny\n'),
      diskOf: (path) => (path === 'src/dirty.ts' ? 'a\nb\n' : 'x\ny\n'),
      dirtyOf: (path) => path === 'src/dirty.ts',
    })

    expect(plan.decisions.map((item) => [item.path, item.action])).toEqual([
      ['src/dirty.ts', 'reject'],
      ['src/clean.ts', 'apply'],
    ])
    const clean = plan.decisions[1]
    if (clean?.action !== 'apply') return
    expect(clean.after).toBe('x\nY\n')
    // 只报脏文件那一条问题。
    expect(plan.problems).toHaveLength(1)
    expect(plan.problems[0]?.path).toBe('src/dirty.ts')
  })
})

describe('planEditApply：纯插入（oldLines = 0）的锚点', () => {
  it('干净文件、插到末尾 → 应用到正确位置', () => {
    const disk = 'a\nb'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(3, ['+c'])])],
      makeContext({ bufferOf: () => disk, diskOf: () => disk, dirtyOf: () => false }),
    )

    const decision = plan.decisions[0]
    expect(decision).toMatchObject({ action: 'apply' })
    if (decision?.action !== 'apply') return
    expect(decision.after).toBe('a\nb\nc')
  })

  it('脏缓冲区里插入点之前多了用户的行 → 拒绝（否则会插错位）', () => {
    // 磁盘 a/b/c；用户在文件头插了一行 u（未保存）。hunk 是磁盘坐标"第 3 行之前插入 X"，
    // 直接照搬行号会插到缓冲区第 3 行（"b"）之前 —— 位置是错的。
    const disk = 'a\nb\nc\n'
    const buffer = 'u\na\nb\nc\n'
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(3, ['+X'])])],
      makeContext({ bufferOf: () => buffer, diskOf: () => disk, dirtyOf: () => true }),
    )

    const decision = plan.decisions[0]
    expect(decision).toMatchObject({
      path: 'src/a.ts',
      action: 'reject',
      dirty: true,
      reason: 'context-mismatch',
    })
    if (decision?.action !== 'reject') return
    expect(decision.message).toContain('这个文件有未保存改动')
    expect(decision.message).toContain('插入点之前的第 1 行')
    expect(plan.problems[0]?.code).toBe(EDIT_APPLY_CONFLICT_CODE)
  })

  it('脏改动只在插入点之后 → 锚点仍然一致，正常插入且保留用户的字', () => {
    const disk = 'a\nb\nc\n'
    const buffer = 'a\nb\nc\nD' // 用户在末尾追加了一行 D
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(4, ['+X'])])],
      makeContext({ bufferOf: () => buffer, diskOf: () => disk, dirtyOf: () => true }),
    )

    const decision = plan.decisions[0]
    expect(decision).toMatchObject({ action: 'apply', dirty: true })
    if (decision?.action !== 'apply') return
    expect(decision.after).toBe('a\nb\nc\nX\nD')
  })

  it('脏文件但没有磁盘副本可对照 → 拒绝而不是照搬行号', () => {
    const plan = planEditApply(
      [makeFile('src/a.ts', [makeHunk(2, ['+X'])])],
      makeContext({ bufferOf: () => 'a\nb\n', diskOf: () => undefined, dirtyOf: () => true }),
    )

    expect(plan.decisions[0]).toMatchObject({ action: 'reject', dirty: true })
    const decision = plan.decisions[0]
    if (decision?.action !== 'reject') return
    expect(decision.message).toContain('磁盘版本未读入')
  })
})

describe('纯函数细节', () => {
  it('splitDocumentLines 去掉 CRLF 的行尾 \\r（否则每个 CRLF 文件都会被判冲突）', () => {
    expect(splitDocumentLines('a\r\nb\r\n')).toEqual(['a', 'b', ''])
  })

  it('verifyHunkAgainstLines 只比 old 侧：前导 + 行不参与比对', () => {
    expect(verifyHunkAgainstLines(makeHunk(1, ['-a', '+A']), ['a', 'b'])).toBeNull()
    expect(verifyHunkAgainstLines(makeHunk(1, ['-a', '+A']), ['b', 'b'])).toContain('第 1 行对不上')
  })

  it('applyLineEdits 一次叠多条编辑（行号都相对原始文本）', () => {
    const edits: LineEdit[] = [
      { startLine: 1, endLine: 1, lines: ['A'] },
      { startLine: 3, endLine: 3, lines: ['C'] },
    ]
    expect(applyLineEdits('a\nb\nc', edits)).toBe('A\nb\nC')
  })

  it('hunkToLineEdit 纯插入时 endLine = startLine - 1', () => {
    expect(hunkToLineEdit(makeHunk(4, ['+X']))).toEqual({ startLine: 4, endLine: 3, lines: ['X'] })
  })
})

describe('applyEditsAsOneUndoGroup：一次应用一个 undo 组', () => {
  it('先闭合、再一次 pushEditOperations、再闭合（整批编辑只有一个 undo 元素）', () => {
    const calls: string[] = []
    const operations: ModelEditOperation[][] = []
    const model: UndoableModel = {
      getValue: () => 'a\nb',
      pushEditOperations: (ops) => {
        calls.push('pushEditOperations')
        operations.push([...ops])
      },
      pushStackElement: () => {
        calls.push('pushStackElement')
      },
    }

    const count = applyEditsAsOneUndoGroup(model, [{ startLine: 3, endLine: 2, lines: ['c'] }])

    expect(count).toBe(1)
    expect(calls).toEqual(['pushStackElement', 'pushEditOperations', 'pushStackElement'])
    expect(operations[0]?.[0]?.text).toBe('\nc')
  })

  it('没有可应用的编辑时不碰 undo 栈', () => {
    const calls: string[] = []
    const model: UndoableModel = {
      getValue: () => 'a',
      pushEditOperations: () => calls.push('pushEditOperations'),
      pushStackElement: () => calls.push('pushStackElement'),
    }

    expect(applyEditsAsOneUndoGroup(model, [])).toBe(0)
    expect(calls).toEqual([])
  })
})