// @vitest-environment jsdom
/**
 * Diff 卡片的界面侧证据（parity #16）：有未保存改动时警告**必须点名到文件**。
 *
 * 只断言"渲染了角色 note"是不够的 —— 用户看到警告却不知道是哪个文件，
 * 仍然可能把卡片点掉、以为自己的字丢了。所以这里钉住无障碍名与汇总提示里都带路径。
 */

import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { PendingEdit } from '../../stores/agent-store'
import { DiffCard } from './diff-card'

afterEach(cleanup)

function makeEdit(
  paths: readonly string[],
  status: PendingEdit['status'] = 'pending',
): PendingEdit {
  return {
    editId: 'edit-1',
    status,
    files: paths.map((path, index) => ({
      path,
      hunks: [
        {
          index,
          oldStart: 1,
          oldLines: 1,
          newStart: 1,
          newLines: 1,
          lines: ['-a', '+b'],
        },
      ],
    })),
  }
}

const noop = (): void => {}

describe('DiffCard：未保存改动的提示', () => {
  it('脏文件有 role=note，且无障碍名带文件名', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'])}
        dirtyPaths={['src/a.ts']}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    // 按无障碍名找：说明警告本身就能告诉"哪个文件"。
    expect(screen.getByRole('note', { name: 'src/a.ts 有未保存改动' })).toBeTruthy()
  })

  it('汇总提示里列出脏文件名，并说明不会覆盖未保存的字', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'])}
        dirtyPaths={['src/a.ts']}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    const note = screen.getByText(/标了「有未保存改动」的文件（src\/a\.ts）/)
    expect(note.textContent).toContain('不会覆盖你没存的字')
  })

  it('同一张卡片里只标注真正脏的文件，干净文件不带警告', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts', 'src/b.ts'])}
        dirtyPaths={['src/b.ts']}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    expect(screen.getAllByRole('note')).toHaveLength(1)
    expect(screen.getByRole('note', { name: 'src/b.ts 有未保存改动' })).toBeTruthy()
    expect(screen.queryByRole('note', { name: /src\/a\.ts/ })).toBeNull()
    expect(screen.getByText(/标了「有未保存改动」的文件（src\/b\.ts）/)).toBeTruthy()
  })

  it('没有脏文件时不渲染警告，提示回到"核心按块落盘"的口径', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'])}
        dirtyPaths={[]}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    expect(screen.queryAllByRole('note')).toHaveLength(0)
    expect(screen.getByText(/接受后由核心按块落盘/)).toBeTruthy()
  })
})

describe('DiffCard：撤销这次应用', () => {
  it('只有已接受的卡片能点「撤销这次应用」，点了就调 onUndo', () => {
    const onUndo = vi.fn()
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'], 'accepted')}
        dirtyPaths={[]}
        onAccept={noop}
        onReject={noop}
        onUndo={onUndo}
      />,
    )

    const button = screen.getByRole('button', { name: '撤销这次应用：src/a.ts' })
    expect((button as HTMLButtonElement).disabled).toBe(false)

    fireEvent.click(button)
    expect(onUndo).toHaveBeenCalledTimes(1)
  })

  it('待审阅时撤销不可点（该点拒绝：核心还没写盘）', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'])}
        dirtyPaths={[]}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    const button = screen.getByRole('button', { name: '撤销这次应用：src/a.ts' }) as HTMLButtonElement
    expect(button.disabled).toBe(true)
  })

  it('撤销后显示"已撤销"，三种操作全部不可再点', () => {
    render(
      <DiffCard
        edit={makeEdit(['src/a.ts'], 'undone')}
        dirtyPaths={[]}
        onAccept={noop}
        onReject={noop}
        onUndo={noop}
      />,
    )

    expect(screen.getByText('已撤销')).toBeTruthy()
    expect(screen.getByText(/已撤销这次应用/)).toBeTruthy()
    for (const name of ['接受对 src/a.ts 的改动', '拒绝对 src/a.ts 的改动', '撤销这次应用：src/a.ts']) {
      const button = screen.getByRole('button', { name }) as HTMLButtonElement
      expect(button.disabled).toBe(true)
    }
  })
})