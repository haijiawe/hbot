/**
 * 项目动作（`lib/project-actions.ts`）：打开 / 新建项目那条链路的**产品级**证据。
 *
 * 为什么不把这些断言写进 store 的测试：动作层必须走 `switchProject` 的门闩
 * （先 flush 未保存缓冲区，写盘失败**不许切**），门闩本身在
 * `lib/session-journal.ts` 里只有函数级测试（`next` 没被调用）。这里钉的是
 * 「从 UI 动作发起，到 store 真的切过去 / 真的没切」这一段：
 *
 * 1. 成功：`projectRoot` 变成核心返回的路径、旧标签清空、文件树重读；
 * 2. 失败：journal 写盘失败时抛 `ProjectSwitchBlockedError`，`project_open`
 *    **一次都没发出**，当前项目原封不动 —— 这是 #15 门闩的产品级证据；
 * 3. 切换成功后旧项目的未保存正文不会跟着进新项目的快照（跨项目污染防线）。
 *
 * 用 `node` 环境跑：这里只碰 store 与 journal（无 DOM）。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Command, FileNode } from './ipc'
import { dispatch } from './ipc'
import type { JournalStore, SessionSnapshot } from './session-journal'
import { ProjectSwitchBlockedError, createMemoryJournalStore } from './session-journal'
import { flushSessionJournal, startSessionPersistence, stopSessionPersistence } from './session-persistence'
import { captureSession, useWorkspaceStore } from '../stores/workspace-store'
import { createProject, openProject } from './project-actions'

vi.mock('./ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./ipc')>()
  // fixture 必须定义在工厂内部：`vi.mock` 被提升到文件顶部，引用外部 const 会撞 TDZ。
  const tree: FileNode[] = [
    {
      name: 'src',
      path: 'src',
      kind: 'dir',
      children: [{ name: 'main.ts', path: 'src/main.ts', kind: 'file' }],
    },
    { name: 'hbot.json', path: 'hbot.json', kind: 'file' },
  ]
  const stub = vi.fn(async (command: Command) => {
    switch (command.type) {
      // 核心的 `project_open` 返回的是规范化后的路径字符串，web 侧把它归一成 `{ path }`。
      case 'project.open':
        return { path: command.path.replace(/\\/g, '/') }
      case 'project.create':
        return {
          path: command.path.replace(/\\/g, '/'),
          name: command.name,
          entry: 'src/main.ts',
        }
      case 'workspace.tree':
        return tree
      case 'fs.read':
        // 恢复现场时每个标签都会 `fs.read` 一次；这里给一份"磁盘上的版本"，
        // 让 journal 里的脏缓冲区与它不同，从而保持 dirty。
        return { path: command.path, content: '// 磁盘上的版本\n', version: 1 }
      default:
        return null
    }
  })
  return { ...actual, dispatch: stub }
})

/** 收集某一类命令实际发出去的参数（去掉 `type` 只留载荷）。 */
function commandsOf<T extends Command['type']>(type: T): Record<string, unknown>[] {
  return vi
    .mocked(dispatch)
    .mock.calls.map(([command]) => command)
    .filter((command) => command.type === type)
    .map(({ type: _type, ...rest }) => rest)
}

/** 造一个"已经开着旧项目、还有一个未保存标签"的现场。 */
function openOldProject(): void {
  useWorkspaceStore.setState({
    projectRoot: 'C:/old',
    tabs: [{ path: 'src/old.ts', dirty: true, version: 1 }],
    contents: { 'src/old.ts': 'const 旧项目未保存 = 1\n' },
    saved: { 'src/old.ts': '' },
    cursors: { 'src/old.ts': { line: 2, column: 3 } },
    activePath: 'src/old.ts',
    treeLoaded: true,
  })
}

beforeEach(() => {
  useWorkspaceStore.setState({
    projectRoot: null,
    tabs: [],
    contents: {},
    saved: {},
    cursors: {},
    activePath: null,
    files: [],
    treeLoaded: false,
    error: null,
    projectDialog: null,
  })
  vi.mocked(dispatch).mockClear()
})

afterEach(() => {
  stopSessionPersistence()
})

describe('openProject', () => {
  it('成功后 projectRoot 变成核心返回的路径、旧标签清空、重新读文件树', async () => {
    startSessionPersistence({ store: createMemoryJournalStore() })
    openOldProject()

    await openProject('C:\\work\\demo')

    const state = useWorkspaceStore.getState()
    expect(state.projectRoot).toBe('C:/work/demo')
    expect(state.tabs).toEqual([])
    expect(state.activePath).toBeNull()
    expect(state.files.length).toBeGreaterThan(0)
    expect(state.treeLoaded).toBe(true)
    // 命令确实发出了，参数原样（规范化是核心的事，web 侧不改写用户输入）。
    expect(commandsOf('project.open')).toEqual([{ path: 'C:\\work\\demo' }])
    expect(commandsOf('workspace.tree').length).toBeGreaterThan(0)
  })

  it('journal 写盘失败时不切项目：抛错、projectRoot 不变、project_open 一次都没发', async () => {
    const failing: JournalStore = {
      read: async () => null,
      write: async () => {
        throw new Error('quota 超限')
      },
    }
    startSessionPersistence({ store: failing })
    // 先有未保存的活，才会有待写快照；否则 flush 无事可做、门闩不会拦。
    openOldProject()

    await expect(openProject('C:/work/demo')).rejects.toBeInstanceOf(ProjectSwitchBlockedError)

    const state = useWorkspaceStore.getState()
    expect(state.projectRoot).toBe('C:/old')
    expect(state.tabs).toHaveLength(1)
    expect(commandsOf('project.open')).toEqual([])
    expect(commandsOf('workspace.tree')).toEqual([])
    expect(state.error).toContain('切换项目被拦下')
  })

  it('切换成功后旧项目的未保存正文不会进新项目快照', async () => {
    startSessionPersistence({ store: createMemoryJournalStore() })
    openOldProject()
    // 先把"旧项目身份 + 未保存正文"真的落到 journal 里，再切 ——
    // 否则这条测试可能因为 journal 本来就是空的而假绿。
    await flushSessionJournal()
    const before = captureSession()
    expect(before.projectRoot).toBe('C:/old')
    expect(JSON.stringify(before)).toContain('旧项目未保存')

    await openProject('C:/work/demo')

    const snapshot = captureSession()
    expect(snapshot.projectRoot).toBe('C:/work/demo')
    expect(snapshot.tabs).toEqual([])
    expect(JSON.stringify(snapshot)).not.toContain('旧项目未保存')
    expect(Object.keys(useWorkspaceStore.getState().contents)).toEqual([])
  })

  it('打开目标项目时，它自己那份 journal 现场会被恢复（未保存缓冲区回来）', async () => {
    // 这正是任务书里"未保存缓冲区永远不恢复"的另一半：只有当打开的**就是**那个项目，
    // 它的现场才允许写回（`isSameProject` 判据），而且脏标签的正文来自 journal。
    const snapshot: SessionSnapshot = {
      version: 1,
      savedAt: 1,
      projectRoot: 'C:/work/demo',
      activePath: 'src/main.ts',
      tabs: [
        {
          path: 'src/main.ts',
          dirty: true,
          cursor: { line: 3, column: 1 },
          content: 'const 未保存 = 1\n',
        },
      ],
    }
    startSessionPersistence({ store: createMemoryJournalStore(snapshot) })

    await openProject('C:/work/demo')

    const state = useWorkspaceStore.getState()
    expect(state.projectRoot).toBe('C:/work/demo')
    expect(state.tabs.map((tab) => tab.path)).toEqual(['src/main.ts'])
    expect(state.tabs[0]?.dirty).toBe(true)
    expect(state.contents['src/main.ts']).toBe('const 未保存 = 1\n')
    expect(state.activePath).toBe('src/main.ts')
  })

  it('空路径在本地就报错，不把空串问核心', async () => {
    startSessionPersistence({ store: createMemoryJournalStore() })

    await expect(openProject('   ')).rejects.toThrow('项目路径不能为空')

    expect(commandsOf('project.open')).toEqual([])
    expect(useWorkspaceStore.getState().error).toContain('项目路径不能为空')
    expect(useWorkspaceStore.getState().projectRoot).toBe('mock://project')
  })
})

describe('createProject', () => {
  it('把 path/name 交给核心，成功后切到核心返回的根', async () => {
    startSessionPersistence({ store: createMemoryJournalStore() })

    const root = await createProject('C:\\work\\new-bot', 'new-bot')

    expect(root).toBe('C:/work/new-bot')
    expect(useWorkspaceStore.getState().projectRoot).toBe('C:/work/new-bot')
    expect(commandsOf('project.create')).toEqual([{ path: 'C:\\work\\new-bot', name: 'new-bot' }])
  })

  it('名称只有空白时本地就拦下，不发命令', async () => {
    startSessionPersistence({ store: createMemoryJournalStore() })

    await expect(createProject('C:/work/new-bot', '  ')).rejects.toThrow('项目名称不能为空')

    expect(commandsOf('project.create')).toEqual([])
  })
})