// @vitest-environment jsdom
/**
 * 「打开项目…」/「新建项目…」的完整交互路径：
 * 命令面板 → `projectDialog` 状态 → 对话框 → `lib/project-actions.ts` →
 * `switchProject` 门闩 → `dispatch` → store。
 *
 * 刻意**不 mock `project-actions`**：这条链上最容易断的正是中间那几跳的接线
 * （store 里的 `projectDialog` 谁写谁读、门闩有没有被绕开、成功后有没有清标签），
 * 只 mock 最外层的 `lib/ipc`（wire 协议已在 `ipc-tauri.project-open.test.ts` 里钉住）。
 */

import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, FileNode } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { createMemoryJournalStore } from '../../lib/session-journal'
import { startSessionPersistence, stopSessionPersistence } from '../../lib/session-persistence'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { CommandPalette } from './command-palette'
import { ProjectDialog } from './project-dialog'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const tree: FileNode[] = [
    { name: 'hbot.json', path: 'hbot.json', kind: 'file' },
    { name: 'src', path: 'src', kind: 'dir', children: [{ name: 'main.ts', path: 'src/main.ts', kind: 'file' }] },
  ]
  const stub = vi.fn(async (command: Command) => {
    switch (command.type) {
      case 'project.open':
        return { path: command.path.replace(/\\/g, '/') }
      case 'project.create':
        return { path: command.path.replace(/\\/g, '/'), name: command.name, entry: 'src/main.ts' }
      case 'workspace.tree':
        return tree
      default:
        return null
    }
  })
  return { ...actual, dispatch: stub }
})

function commandsOf<T extends Command['type']>(type: T): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

/** 与 `app/App.tsx` 相同的组合：面板 + 对话框同时挂载，靠 store 里的状态切换。 */
function renderShell() {
  const onClose = vi.fn()
  const onNotice = vi.fn()
  render(
    <>
      <CommandPalette mode="commands" onClose={onClose} onNotice={onNotice} />
      <ProjectDialog onNotice={onNotice} />
    </>,
  )
  return { onClose, onNotice }
}

beforeEach(() => {
  useWorkspaceStore.setState({
    projectRoot: null,
    files: [],
    treeLoaded: false,
    contents: {},
    saved: {},
    tabs: [],
    activePath: null,
    reveal: null,
    error: null,
    diagnostics: [],
    projectDialog: null,
  })
  vi.mocked(dispatch).mockClear()
  startSessionPersistence({ store: createMemoryJournalStore() })
})

afterEach(() => {
  stopSessionPersistence()
  cleanup()
})

describe('命令面板 → 项目对话框', () => {
  it('「打开项目…」弹对话框，粘贴路径后 store 真的切到新项目', async () => {
    const user = userEvent.setup()
    const { onClose, onNotice } = renderShell()

    await user.type(screen.getByLabelText('输入命令'), '项目')
    await user.keyboard('{Enter}')

    // 面板关掉、对话框起来（面板只负责发起，渲染在 App 层）。
    expect(onClose).toHaveBeenCalledTimes(1)
    expect(screen.getByRole('dialog', { name: '打开项目' })).toBeTruthy()

    await user.type(screen.getByLabelText('项目路径'), 'C:\\work\\demo')
    await user.click(screen.getByRole('button', { name: '打开' }))

    await waitFor(() => expect(useWorkspaceStore.getState().projectRoot).toBe('C:/work/demo'))
    const state = useWorkspaceStore.getState()
    expect(state.projectDialog).toBeNull()
    expect(state.tabs).toEqual([])
    expect(state.treeLoaded).toBe(true)
    expect(commandsOf('project.open')).toEqual([{ path: 'C:\\work\\demo' }])
    expect(onNotice).toHaveBeenCalledWith('已打开项目：C:/work/demo')
    expect(screen.queryByRole('dialog', { name: '打开项目' })).toBeNull()
  })

  it('「新建项目…」弹的对话框多一个名称输入框，创建后带 path+name 发命令', async () => {
    const user = userEvent.setup()
    const { onNotice } = renderShell()

    await user.type(screen.getByLabelText('输入命令'), '项目')
    await user.keyboard('{ArrowDown}{Enter}')

    expect(screen.getByRole('dialog', { name: '新建项目' })).toBeTruthy()
    // 打开模式的对话框没有名称框 —— 这也是"两条命令真的走两个模式"的证据。
    expect(screen.queryByLabelText('项目名称')).not.toBeNull()

    await user.type(screen.getByLabelText('项目路径'), 'C:\\work\\new-bot')
    await user.type(screen.getByLabelText('项目名称'), 'new-bot')
    await user.click(screen.getByRole('button', { name: '创建' }))

    await waitFor(() => expect(useWorkspaceStore.getState().projectRoot).toBe('C:/work/new-bot'))
    expect(commandsOf('project.create')).toEqual([{ path: 'C:\\work\\new-bot', name: 'new-bot' }])
    expect(onNotice).toHaveBeenCalledWith('已新建项目：C:/work/new-bot')
  })

  it('路径为空就地报错、对话框留在原地、一个命令都不发', async () => {
    const user = userEvent.setup()
    const { onNotice } = renderShell()

    await user.type(screen.getByLabelText('输入命令'), '打开项目')
    await user.keyboard('{Enter}')
    await user.click(screen.getByRole('button', { name: '打开' }))

    expect(screen.getByRole('alert').textContent).toContain('请粘贴项目绝对路径')
    expect(useWorkspaceStore.getState().projectDialog).toBe('open')
    expect(commandsOf('project.open')).toEqual([])
    expect(onNotice).not.toHaveBeenCalled()
  })

  it('取消按钮关掉对话框（不开项目、不报错）', async () => {
    const user = userEvent.setup()
    renderShell()

    await user.type(screen.getByLabelText('输入命令'), '打开项目')
    await user.keyboard('{Enter}')
    await user.click(screen.getByRole('button', { name: '取消' }))

    expect(useWorkspaceStore.getState().projectDialog).toBeNull()
    expect(screen.queryByRole('dialog', { name: '打开项目' })).toBeNull()
    expect(commandsOf('project.open')).toEqual([])
  })
})