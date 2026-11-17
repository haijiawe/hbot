// @vitest-environment jsdom
/**
 * 命令面板 / 快速打开（`features/command-palette/command-palette.tsx`）。
 *
 * 这里不测样式，只钉交互合同：
 * 1. 过滤是模糊匹配（字符按顺序出现即可），无匹配时给一句文案而不是空列表；
 * 2. Enter 执行**当前高亮项**并关闭面板，Esc 关闭；
 * 3. 禁用的命令（没有暂停现场时的「调试：继续」）既不能执行也不能关面板 ——
 *    否则用户按一下回车，面板没了、什么都没发生；
 * 4. 从命令模式进快速打开是"切换模式"不是"关面板"，Esc 再一步步退回。
 */

import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, FileNode } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useDebugStore } from '../../stores/debug-store'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { useSearchStore } from '../../stores/search-store'
import { CommandPalette } from './command-palette'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const stub = vi.fn(async (command: Command) => {
    if (command.type === 'fs.read') {
      return { path: command.path, content: 'const a = 1\n', version: 1 }
    }
    return null
  })
  return { ...actual, dispatch: stub }
})

const FILES: FileNode[] = [
  {
    name: 'src',
    path: 'src',
    kind: 'dir',
    children: [
      { name: 'main.ts', path: 'src/main.ts', kind: 'file' },
      { name: 'reply.ts', path: 'src/reply.ts', kind: 'file' },
    ],
  },
  { name: 'README.md', path: 'README.md', kind: 'file' },
]

function commandsOf<T extends Command['type']>(type: T): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

function renderPalette(mode: 'commands' | 'files' = 'commands') {
  const onClose = vi.fn()
  const onNotice = vi.fn()
  render(<CommandPalette mode={mode} onClose={onClose} onNotice={onNotice} />)
  return { onClose, onNotice }
}

beforeEach(() => {
  useWorkspaceStore.setState({
    files: FILES,
    treeLoaded: true,
    contents: {},
    saved: {},
    tabs: [],
    activePath: null,
    reveal: null,
    error: null,
    diagnostics: [],
    bottomPanelTab: 'logs',
    sidebarView: 'explorer',
  })
  useDebugStore.setState({
    botId: '',
    breakpoints: {},
    paused: null,
    selectedFrame: 0,
    busy: false,
    error: null,
  })
  useRunStore.setState({ botId: '', status: 'stopped', logs: [], busy: false, error: null })
  useSearchStore.setState({ focusSeq: 0 })
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('CommandPalette（命令模式）', () => {
  it('列出命令，第一项默认高亮', () => {
    renderPalette()

    expect(screen.getByRole('dialog', { name: '命令面板' })).toBeTruthy()
    const options = screen.getAllByRole('option')
    expect(options.length).toBeGreaterThan(5)
    expect(options[0]?.getAttribute('aria-selected')).toBe('true')
    expect(options[0]?.textContent).toContain('快速打开文件…')
  })

  it('按标题模糊过滤；没有匹配时给文案', async () => {
    const user = userEvent.setup()
    renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '问题')
    const options = screen.getAllByRole('option')
    expect(options).toHaveLength(1)
    expect(options[0]?.textContent).toContain('打开问题面板')

    await user.type(screen.getByLabelText('输入命令'), 'zzzz')
    expect(screen.getByRole('status').textContent).toBe('没有匹配项。')
  })

  it('回车执行高亮项：切到问题页签并关闭面板', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '问题')
    await user.keyboard('{Enter}')

    const workspace = useWorkspaceStore.getState()
    expect(workspace.bottomPanelTab).toBe('problems')
    expect(workspace.layout.bottomPanelCollapsed).toBe(false)
    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('方向键移动高亮，回车执行移动后的那一项', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '调试')
    const before = screen.getAllByRole('option').map((item) => item.textContent)
    expect(before[0]).toContain('打开调试面板')

    await user.keyboard('{ArrowDown}')
    await user.keyboard('{Enter}')

    // 第二项是「调试：继续」—— 没有暂停现场，它被禁用，所以什么都不该发生。
    expect(onClose).not.toHaveBeenCalled()
    expect(commandsOf('debug.resume')).toEqual([])
  })

  it('禁用项：aria-disabled 标出，回车既不执行也不关面板', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '继续')

    const option = screen.getAllByRole('option')[0]
    expect(option?.getAttribute('aria-disabled')).toBe('true')
    expect(option?.textContent).toContain('调试：继续')

    await user.keyboard('{Enter}')
    expect(onClose).not.toHaveBeenCalled()
    expect(commandsOf('debug.resume')).toEqual([])
  })

  it('暂停后同一项变成可执行，回车发 debug.resume', async () => {
    useDebugStore.setState({
      botId: 'bot-1',
      paused: {
        file: 'src/main.ts',
        line: 3,
        condition: null,
        scopes: [],
        callStack: [{ name: 'main', file: 'src/main.ts', line: 3, column: 1 }],
      },
      selectedFrame: 0,
    })
    const user = userEvent.setup()
    renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '继续')
    expect(screen.getAllByRole('option')[0]?.getAttribute('aria-disabled')).toBe('false')

    await user.keyboard('{Enter}')

    await waitFor(() => expect(commandsOf('debug.resume')).toEqual([{ botId: 'bot-1' }]))
  })

  it('Esc 关闭面板', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.keyboard('{Escape}')

    expect(onClose).toHaveBeenCalledTimes(1)
  })
})

describe('CommandPalette（快速打开 / 模式切换）', () => {
  it('命令模式里执行「快速打开文件…」只切模式，不关面板', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '快速打开')
    await user.keyboard('{Enter}')

    expect(onClose).not.toHaveBeenCalled()
    expect(screen.getByRole('dialog', { name: '快速打开文件' })).toBeTruthy()
    // 文件模式下输入框换了 aria-label，之前的查询串也清空了 —— 否则切过去是「没有匹配项」。
    const input = screen.getByLabelText('输入文件名') as HTMLInputElement
    expect(input.value).toBe('')
    expect(screen.queryByRole('status')).toBeNull()
    expect(screen.getAllByRole('option').length).toBe(3)
  })

  it('文件模式列出树里的文件；已在标签页打开的那个带「已打开」', () => {
    useWorkspaceStore.setState({
      tabs: [{ path: 'src/main.ts', dirty: false, version: 1 }],
    })
    renderPalette('files')

    const options = screen.getAllByRole('option')
    expect(options.map((item) => item.textContent)).toEqual([
      'src/main.ts已打开',
      'src/reply.ts',
      'README.md',
    ])
  })

  it('文件模式下按文件名过滤，回车打开文件并关面板', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette('files')

    await user.type(screen.getByLabelText('输入文件名'), 'reply')
    await user.keyboard('{Enter}')

    await waitFor(() => expect(useWorkspaceStore.getState().activePath).toBe('src/reply.ts'))
    expect(commandsOf('fs.read')).toEqual([{ path: 'src/reply.ts' }])
    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('文件模式 Esc 关面板（不是回到命令模式）', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette('files')

    await user.keyboard('{Escape}')

    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('从命令模式切进来的文件模式，Esc 先返回命令模式，再 Esc 才关闭', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '快速打开')
    await user.keyboard('{Enter}')
    expect(screen.getByRole('dialog', { name: '快速打开文件' })).toBeTruthy()

    await user.keyboard('{Escape}')
    expect(screen.getByRole('dialog', { name: '命令面板' })).toBeTruthy()
    expect(onClose).not.toHaveBeenCalled()

    await user.keyboard('{Escape}')
    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('文件模式里查询为空时退格 = 返回命令模式', async () => {
    const user = userEvent.setup()
    const { onClose } = renderPalette()

    await user.type(screen.getByLabelText('输入命令'), '快速打开')
    await user.keyboard('{Enter}')
    await user.keyboard('{Backspace}')

    expect(screen.getByRole('dialog', { name: '命令面板' })).toBeTruthy()
    expect(onClose).not.toHaveBeenCalled()
  })
})