/**
 * 未保存缓冲区 journal 的测试（node 环境，内存后端）。
 *
 * 覆盖三件事，缺一件就等于没接线：
 * 1. **机制**：节流、写失败不丢快照、切项目门闩（flush 失败时 `next` 一次都不许跑）；
 * 2. **快照形状**：脏标签才带正文、光标兜底 1:1、坏数据一律丢弃；
 * 3. **接线**（`session-persistence.ts`）：订阅录制、项目身份一致才恢复、不一致必须丢弃。
 *
 * 只有 `beforeunload` 那条需要 `window`：node 里没有 window，所以用最小 `EventTarget`
 * 顶替（`installBeforeUnloadFlush` 只用 add/removeEventListener，不需要 jsdom）。
 * `App.tsx` 里那一个 `useEffect` 本文件测不到（要渲染整棵外壳，属组件测试），
 * 它调的 `startSessionPersistence` / `restoreSessionIfSameProject` 都在这里被覆盖。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { captureSession, useWorkspaceStore } from '../stores/workspace-store'
import {
  JOURNAL_THROTTLE_MS,
  ProjectSwitchBlockedError,
  createBufferJournal,
  createMemoryJournalStore,
  installBeforeUnloadFlush,
  runProjectSwitch,
  sanitizeSnapshot,
  type JournalStore,
  type SessionSnapshot,
} from './session-journal'
import {
  MOCK_PROJECT_ROOT,
  flushSessionJournal,
  isSameProject,
  restoreSessionIfSameProject,
  startSessionPersistence,
  stopSessionPersistence,
  switchProject,
} from './session-persistence'

// `vi.mock` 会被提升到文件顶部，所以 mock 句柄必须用 `vi.hoisted` 建。
const { dispatchMock } = vi.hoisted(() => ({ dispatchMock: vi.fn() }))

// 只替换 `dispatch`，其余导出（runtimeMode / 类型）保持真实实现：
// `session-persistence` 靠 `runtimeMode() === 'mock'` 决定要不要给 mock 后端定一个项目身份。
vi.mock('./ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./ipc')>()
  return { ...actual, dispatch: dispatchMock }
})

/** 一份合法快照（默认：属于 project-a，有一个带正文的脏标签）。 */
function snapshotOf(overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    version: 1,
    savedAt: 1,
    projectRoot: 'C:/work/project-a',
    activePath: 'src/main.ts',
    tabs: [
      {
        path: 'src/main.ts',
        dirty: true,
        cursor: { line: 2, column: 3 },
        content: '未保存的正文',
      },
    ],
    ...overrides,
  }
}

interface RecordingStore {
  store: JournalStore
  writes: SessionSnapshot[]
  read: () => Promise<SessionSnapshot | null>
}

/** 记账用的内存后端（数写入次数，能读回最后一次写进去的东西）。 */
function recordingStore(initial: SessionSnapshot | null = null): RecordingStore {
  const inner = createMemoryJournalStore(initial)
  const writes: SessionSnapshot[] = []
  return {
    writes,
    read: () => inner.read(),
    store: {
      read: () => inner.read(),
      async write(snapshot) {
        writes.push(snapshot)
        await inner.write(snapshot)
      },
    },
  }
}

/** 前 `failures` 次写盘抛错，之后成功的后端（模拟配额满 / 隐私模式）。 */
function flakyStore(failures = 1): JournalStore {
  const inner = createMemoryJournalStore(null)
  let remaining = failures
  return {
    read: () => inner.read(),
    async write(snapshot) {
      if (remaining > 0) {
        remaining -= 1
        throw new Error('配额已满')
      }
      await inner.write(snapshot)
    },
  }
}

beforeEach(() => {
  stopSessionPersistence()
  dispatchMock.mockReset()
  dispatchMock.mockImplementation(async (command: { type: string; path?: string }) => {
    if (command.type === 'workspace.tree') return []
    if (command.type === 'fs.read') {
      return { path: command.path ?? '', content: '磁盘上的正文', version: 7 }
    }
    throw new Error(`测试没有为 ${command.type} 准备返回值`)
  })
  useWorkspaceStore.setState({
    tabs: [],
    contents: {},
    saved: {},
    cursors: {},
    activePath: null,
    projectRoot: null,
    error: null,
  })
})

afterEach(() => {
  stopSessionPersistence()
  vi.useRealTimers()
})

describe('createBufferJournal 机制', () => {
  it('record → flush 落盘，restore 能读回同一份现场', async () => {
    const { store, writes } = recordingStore()
    const journal = createBufferJournal(store, { now: () => 4_242 })

    journal.record(snapshotOf())
    expect(journal.hasPending()).toBe(true)

    await journal.flush()
    expect(writes).toHaveLength(1)
    expect(journal.hasPending()).toBe(false)

    const restored = await journal.restore()
    expect(restored).toEqual({ ...snapshotOf(), savedAt: 4_242 })
  })

  it('节流窗口内多次 record 只写一次，写的是窗口内最后一份快照', async () => {
    vi.useFakeTimers()
    const { store, writes } = recordingStore()
    const journal = createBufferJournal(store)

    journal.record(snapshotOf({ activePath: 'a.ts' }))
    journal.record(snapshotOf({ activePath: 'b.ts' }))
    journal.record(snapshotOf({ activePath: 'c.ts' }))

    await vi.advanceTimersByTimeAsync(JOURNAL_THROTTLE_MS - 1)
    expect(writes).toHaveLength(0)

    await vi.advanceTimersByTimeAsync(1)
    expect(writes).toHaveLength(1)
    expect(writes[0]?.activePath).toBe('c.ts')
    journal.dispose()
  })

  it('store.write 抛错时 flush reject，但快照不丢（下一次 flush 落盘）', async () => {
    const store = flakyStore(1)
    const journal = createBufferJournal(store)

    journal.record(snapshotOf({ activePath: 'retry.ts' }))
    await expect(journal.flush()).rejects.toThrow('配额已满')
    // 关键：失败之后待写快照还在，不是"写失败就算写过了"。
    expect(journal.hasPending()).toBe(true)

    await journal.flush()
    expect(journal.hasPending()).toBe(false)
    expect((await journal.restore())?.activePath).toBe('retry.ts')
  })

  it('restore 遇到坏数据返回 null，不把半截快照交给编辑器', async () => {
    const store = createMemoryJournalStore(null)
    const journal = createBufferJournal(store)
    // 绕过类型直接塞一份坏存档（模拟手改 / 旧版本）。
    const broken = { version: 2, tabs: [] }
    await store.write(broken as unknown as SessionSnapshot)
    expect(await journal.restore()).toBeNull()
  })
})

describe('runProjectSwitch 门闩', () => {
  it('flush 失败：抛 ProjectSwitchBlockedError，next 一次都没被调用', async () => {
    const journal = createBufferJournal(flakyStore(1))
    journal.record(snapshotOf({ activePath: 'unsaved.ts' }))
    const next = vi.fn(async () => undefined)

    await expect(runProjectSwitch(journal, next)).rejects.toBeInstanceOf(ProjectSwitchBlockedError)
    expect(next).not.toHaveBeenCalled()
  })

  it('flush 成功：先落盘再切（顺序可断言），next 只调一次', async () => {
    const order: string[] = []
    const inner = createMemoryJournalStore(null)
    const journal = createBufferJournal({
      read: () => inner.read(),
      async write(snapshot) {
        order.push('write')
        await inner.write(snapshot)
      },
    })
    journal.record(snapshotOf())

    await runProjectSwitch(journal, async () => {
      order.push('switch')
    })

    expect(order).toEqual(['write', 'switch'])
  })
})

describe('sanitizeSnapshot 校验', () => {
  it('拒绝坏数据（版本不对 / 缺 tabs / path 空 / 不是对象）都返回 null', () => {
    const cases: unknown[] = [
      null,
      undefined,
      'snapshot',
      [],
      { version: 2, tabs: [] },
      { version: 1 },
      { version: 1, tabs: 'nope' },
      { version: 1, tabs: [{}] },
      { version: 1, tabs: [{ path: '' }] },
      { version: 1, tabs: [{ path: 42 }] },
    ]
    for (const raw of cases) expect(sanitizeSnapshot(raw)).toBeNull()
  })

  it('接受合法快照：保留 projectRoot、夹紧光标、脏标记按正文推断', () => {
    const parsed = sanitizeSnapshot({
      version: 1,
      savedAt: 12,
      projectRoot: 'C:/work/project-a',
      activePath: 'a.ts',
      tabs: [{ path: 'a.ts', dirty: true, cursor: { line: -3, column: 0 }, content: 'x' }],
    })
    expect(parsed).toEqual({
      version: 1,
      savedAt: 12,
      projectRoot: 'C:/work/project-a',
      activePath: 'a.ts',
      tabs: [{ path: 'a.ts', dirty: true, cursor: { line: 1, column: 1 }, content: 'x' }],
    })
  })

  it('旧存档（没有 projectRoot）读成 null = 项目身份不明', () => {
    const parsed = sanitizeSnapshot({ version: 1, tabs: [{ path: 'a.ts', dirty: false }] })
    expect(parsed?.projectRoot).toBeNull()
    expect(parsed?.tabs[0]?.content).toBeNull()
  })
})

describe('captureSession 快照生产者', () => {
  it('干净标签 content 为 null，脏标签带缓冲区正文；光标缺省按 1:1', () => {
    useWorkspaceStore.setState({
      projectRoot: 'C:/work/project-a',
      activePath: 'dirty.ts',
      tabs: [
        { path: 'clean.ts', dirty: false, version: 1 },
        { path: 'dirty.ts', dirty: true, version: 1 },
      ],
      contents: { 'clean.ts': '磁盘上也有的正文', 'dirty.ts': '还没保存的正文' },
      cursors: { 'dirty.ts': { line: 9, column: 4 } },
    })

    const snapshot = captureSession()
    expect(snapshot.version).toBe(1)
    expect(snapshot.projectRoot).toBe('C:/work/project-a')
    expect(snapshot.activePath).toBe('dirty.ts')
    expect(snapshot.tabs).toEqual([
      { path: 'clean.ts', dirty: false, cursor: { line: 1, column: 1 }, content: null },
      { path: 'dirty.ts', dirty: true, cursor: { line: 9, column: 4 }, content: '还没保存的正文' },
    ])
  })
})

describe('跨项目判据 isSameProject', () => {
  it('任一方身份未知都不算同一个项目', () => {
    expect(isSameProject(snapshotOf(), 'C:/work/project-a')).toBe(true)
    expect(isSameProject(snapshotOf(), 'C:/work/project-b')).toBe(false)
    expect(isSameProject(snapshotOf({ projectRoot: null }), 'C:/work/project-a')).toBe(false)
    expect(isSameProject(snapshotOf(), null)).toBe(false)
  })
})

describe('session-persistence 接线', () => {
  it('项目一致：把 journal 里的未保存正文写回编辑器', async () => {
    const { store } = recordingStore(snapshotOf())
    useWorkspaceStore.setState({ projectRoot: 'C:/work/project-a' })
    startSessionPersistence({ store })

    expect(await restoreSessionIfSameProject()).toBe('restored')

    const state = useWorkspaceStore.getState()
    expect(state.tabs).toEqual([{ path: 'src/main.ts', dirty: true, version: 7 }])
    expect(state.contents['src/main.ts']).toBe('未保存的正文')
    expect(state.saved['src/main.ts']).toBe('磁盘上的正文')
    expect(state.cursors['src/main.ts']).toEqual({ line: 2, column: 3 })
    expect(state.activePath).toBe('src/main.ts')
  })

  it('项目不一致：丢弃现场（不读文件、不碰编辑器），并把"已跳过"说出来', async () => {
    const { store, read } = recordingStore(snapshotOf({ projectRoot: 'C:/work/project-a' }))
    useWorkspaceStore.setState({ projectRoot: 'C:/work/project-b' })
    startSessionPersistence({ store })

    expect(await restoreSessionIfSameProject()).toBe('other-project')

    expect(dispatchMock).not.toHaveBeenCalled()
    expect(useWorkspaceStore.getState().tabs).toEqual([])
    const error = useWorkspaceStore.getState().error ?? ''
    expect(error).toContain('另一个项目')
    expect(error).toContain('已跳过恢复')
    // 快照本身没有被清掉或改写（下次回到 project-a 还能恢复）。
    expect((await read())?.projectRoot).toBe('C:/work/project-a')
  })

  it('项目身份不明（旧存档 / 还没有项目）：不恢复', async () => {
    const { store } = recordingStore(snapshotOf({ projectRoot: null }))
    useWorkspaceStore.setState({ projectRoot: 'C:/work/project-b' })
    startSessionPersistence({ store })

    expect(await restoreSessionIfSameProject()).toBe('unknown-project')
    expect(dispatchMock).not.toHaveBeenCalled()
    // 旧存档没有 projectRoot：说清"为什么没恢复"，但绝不猜一个项目往上套。
    expect(useWorkspaceStore.getState().error ?? '').toContain('没有项目身份')
  })

  it('启动时还没打开项目（projectRoot 为空）：不恢复、也不弹无关警告', async () => {
    const { store } = recordingStore(snapshotOf({ projectRoot: 'C:/work/project-a' }))
    startSessionPersistence({ store })
    // mock 后端启动时会先定一个假项目根（`MOCK_PROJECT_ROOT`）；这里把它抹掉，
    // 模拟真桌面"外壳起来了但还没打开项目"的那一刻（project_open 还没返回）。
    useWorkspaceStore.setState({ projectRoot: null })

    expect(await restoreSessionIfSameProject()).toBe('unknown-project')
    expect(dispatchMock).not.toHaveBeenCalled()
    expect(useWorkspaceStore.getState().tabs).toEqual([])
    expect(useWorkspaceStore.getState().error).toBeNull()
  })

  it('journal 里没有快照：返回 empty，什么也不做', async () => {
    const { store } = recordingStore(null)
    startSessionPersistence({ store })

    expect(await restoreSessionIfSameProject()).toBe('empty')
    expect(useWorkspaceStore.getState().tabs).toEqual([])
  })

  it('编辑之后经节流自动落盘，快照带项目身份与脏标签正文', async () => {
    vi.useFakeTimers()
    const { store, read } = recordingStore()
    startSessionPersistence({ store })

    useWorkspaceStore.setState({
      tabs: [{ path: 'bot.ts', dirty: true, version: 1 }],
      contents: { 'bot.ts': '写了一半' },
      saved: { 'bot.ts': '磁盘版' },
      activePath: 'bot.ts',
    })

    await vi.advanceTimersByTimeAsync(JOURNAL_THROTTLE_MS)

    const written = await read()
    expect(written?.projectRoot).toBe(MOCK_PROJECT_ROOT)
    expect(written?.activePath).toBe('bot.ts')
    expect(written?.tabs).toEqual([
      { path: 'bot.ts', dirty: true, cursor: { line: 1, column: 1 }, content: '写了一半' },
    ])
  })
})

describe('switchProject 切项目门闩', () => {
  it('journal 写不进去：拦下切换，openProject 一次都没被调用，原因进状态栏', async () => {
    const store = flakyStore(1)
    // 真桌面里 projectRoot 来自 project_open：先定下当前项目，再起 journal。
    useWorkspaceStore.setState({ projectRoot: 'C:/work/project-a' })
    startSessionPersistence({ store })
    useWorkspaceStore.setState({
      tabs: [{ path: 'unsaved.ts', dirty: true, version: 1 }],
      contents: { 'unsaved.ts': '还没保存' },
      activePath: 'unsaved.ts',
    })
    const openProject = vi.fn(async () => 'C:/work/project-b')

    await expect(switchProject('C:/work/project-b', openProject)).rejects.toBeInstanceOf(
      ProjectSwitchBlockedError,
    )

    expect(openProject).not.toHaveBeenCalled()
    expect(useWorkspaceStore.getState().projectRoot).toBe('C:/work/project-a')
    expect(useWorkspaceStore.getState().error ?? '').toContain('切换项目被拦下')
  })

  it('journal 写得进去：先落盘旧现场，再换项目身份、清空会话、重读文件树', async () => {
    const { store, writes, read } = recordingStore(snapshotOf({ projectRoot: 'C:/work/project-a' }))
    useWorkspaceStore.setState({ projectRoot: 'C:/work/project-a' })
    startSessionPersistence({ store })
    useWorkspaceStore.setState({
      tabs: [{ path: 'a.ts', dirty: true, version: 1 }],
      contents: { 'a.ts': 'A 项目没保存' },
      activePath: 'a.ts',
    })
    const openProject = vi.fn(async () => 'C:/work/project-b')

    await switchProject('C:/work/project-b', openProject)

    expect(openProject).toHaveBeenCalledWith('C:/work/project-b')
    const state = useWorkspaceStore.getState()
    expect(state.projectRoot).toBe('C:/work/project-b')
    expect(state.treeLoaded).toBe(true)
    // 旧项目的标签不能留在编辑器里（否则会被记成 B 的现场）。
    expect(state.tabs).toEqual([])
    // 切走之前 A 的未保存正文已经落盘（闸门的承诺：不丢字）。
    expect(writes[0]?.projectRoot).toBe('C:/work/project-a')
    expect(writes[0]?.tabs[0]?.content).toBe('A 项目没保存')

    // 切完之后记下的是 B 的现场，且它属于 B（A 的快照属于另一个项目，恢复时被丢弃）。
    await flushSessionJournal()
    const recorded = await read()
    expect(recorded?.projectRoot).toBe('C:/work/project-b')
    expect(recorded?.tabs).toEqual([])
  })
})

describe('installBeforeUnloadFlush', () => {
  it('触发 beforeunload 会 flush；解绑后不再 flush', async () => {
    const { store, read } = recordingStore()
    const journal = createBufferJournal(store)
    const fakeWindow = new EventTarget()
    Object.defineProperty(globalThis, 'window', { value: fakeWindow, configurable: true })
    try {
      const unbind = installBeforeUnloadFlush(journal)

      journal.record(snapshotOf({ activePath: 'before-unload.ts' }))
      fakeWindow.dispatchEvent(new Event('beforeunload'))
      await new Promise((resolve) => setTimeout(resolve, 0))
      expect((await read())?.activePath).toBe('before-unload.ts')

      unbind()
      journal.record(snapshotOf({ activePath: 'after-unbind.ts' }))
      fakeWindow.dispatchEvent(new Event('beforeunload'))
      await new Promise((resolve) => setTimeout(resolve, 0))
      expect((await read())?.activePath).toBe('before-unload.ts')
    } finally {
      delete (globalThis as { window?: unknown }).window
      journal.dispose()
    }
  })
})