/**
 * 20 个打开标签 × 200 KB 缓冲区（≈4 MB 总量）在 store 层的内存形状与操作代价
 * （`stores/workspace-store.ts`）。
 *
 * 这条测试回答出口标准「20 标签不卡」里**store 那一半**的问题，钉住三个事实：
 * 1. 每个干净缓冲区在内存里**只有一份文本**：`contents[path]` 与 `saved[path]` 是
 *    同一个字符串实例（`openFile` 把 `fs.read` 的同一个 `result.content` 同时放进两个
 *    map，见 `workspace-store.ts:300-314`）。两份 map 只是两个**引用**，不是两份字节。
 * 2. 只有**脏**标签才会真的多出一份：`updateContent` 造新字符串，`saved` 保留旧文本
 *    （dirty 判断与非破坏性 Diff 都需要它，这是语义要求，不是浪费）。
 * 3. `updateContent`（即「setBuffer」）的代价与缓冲区**字节数无关**，只与标签数有关：
 *    200 KB 与 20 KB 的缓冲区上耗时同一量级 —— 说明 store 层没有逐次拷贝文本。
 *
 * ⚠️ 这是 **Node 里的 store 测量**，不是浏览器渲染测量：没有 DOM、没有 React 重渲染、
 * 没有 Monaco。Monaco 的 model 自己还持有一份文本并做 tokenize/装饰，真机「20 标签不卡」
 * 必须在真浏览器里量帧时间，本文件量不到。
 *
 * ⚠️ `JSON.stringify` 只能当**保守**代理指标：它会把同一个字符串实例在两个 map 里各序列化
 * 一次，所以 4 MB 的共享缓冲区会报成 8 MB。本机实测堆增量 3.91 MB ↔ stringify 8.06 MB，
 * 别把 8 MB 当内存读数引用。
 */

import { beforeEach, describe, expect, it, vi } from 'vitest'

const { dispatchMock } = vi.hoisted(() => ({ dispatchMock: vi.fn() }))

vi.mock('../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/ipc')>()
  return { ...actual, dispatch: dispatchMock }
})

import { useWorkspaceStore } from './workspace-store'

/** 单个缓冲区大小：200 KB（ASCII，1 char = 1 byte）。 */
const BUFFER_BYTES = 200 * 1024
/** 打开的标签数。 */
const TAB_COUNT = 20

/**
 * 防回归阈值，**不是性能承诺**。
 *
 * 本机（Windows / Node 24 / vitest forks）实测：打开 20×200 KB **0.6–0.8 ms**、
 * `updateContent` 200 次 **0.8 ms**、切标签 2000 次 **0.5–0.65 ms**、关 20 个 **0.3–0.4 ms**。
 * 给 2 s 是留约 2500× 余量，专门避免 CI 抖动误红；它只是「有人往 setBuffer 里塞了全量
 * 深拷贝/序列化」时的报警线。
 */
const BUDGET_MS = 2_000

/** 造一份恰好 `bytes` 字节的源码文本。 */
function makeBuffer(index: number, bytes: number): string {
  const head = `// file ${index}\n`
  const padding = 'const value = 1 // padding line\n'
  const body = padding.repeat(Math.ceil((bytes - head.length) / padding.length))
  return (head + body).slice(0, bytes)
}

/** `src/f12.ts` → 12。 */
function pathIndex(path: string): number | null {
  const match = /^src\/f(\d+)\.ts$/.exec(path)
  if (match === null) return null
  const digits = match[1]
  return digits === undefined ? null : Number(digits)
}

/** 让 `fs.read` 按路径回对应的缓冲区，模拟核心读盘。 */
function installDispatch(buffers: readonly string[]): void {
  dispatchMock.mockImplementation(async (command: { type: string; path?: string }) => {
    if (command.type === 'fs.write') return { version: 2 }
    if (command.type !== 'fs.read') throw new Error(`unexpected command: ${command.type}`)
    const index = pathIndex(command.path ?? '')
    const content = index === null ? undefined : buffers[index]
    if (content === undefined) throw new Error(`unexpected read: ${command.path ?? ''}`)
    return { content, version: 1 }
  })
}

function resetStore(): void {
  useWorkspaceStore.setState({
    tabs: [],
    contents: {},
    saved: {},
    cursors: {},
    activePath: null,
  })
}

/** 把 20 个标签的缓冲区造好并真的走一遍 `openFile`。 */
async function openTabs(bytes = BUFFER_BYTES): Promise<string[]> {
  const buffers = Array.from({ length: TAB_COUNT }, (_, index) => makeBuffer(index, bytes))
  installDispatch(buffers)
  for (let index = 0; index < TAB_COUNT; index += 1) {
    await useWorkspaceStore.getState().openFile(`src/f${index}.ts`)
  }
  return buffers
}

/** 内存里实际有几个不同的字符串实例（干净标签 = 标签数，脏标签 = 标签数 + 脏标签数）。 */
function distinctInstances(): number {
  const state = useWorkspaceStore.getState()
  return new Set([...Object.values(state.contents), ...Object.values(state.saved)]).size
}

describe('workspaceStore 20 标签 × 200 KB', () => {
  beforeEach(() => {
    dispatchMock.mockReset()
    resetStore()
  })

  it('打开 20 个 200KB 缓冲区：20 个标签，每个都以完整长度进 contents/saved', async () => {
    const started = performance.now()
    await openTabs()
    const elapsed = performance.now() - started

    const state = useWorkspaceStore.getState()
    expect(state.tabs).toHaveLength(TAB_COUNT)
    expect(Object.keys(state.contents)).toHaveLength(TAB_COUNT)
    expect(Object.keys(state.saved)).toHaveLength(TAB_COUNT)
    expect(Object.values(state.contents).every((text) => text.length === BUFFER_BYTES)).toBe(true)
    expect(elapsed).toBeLessThan(BUDGET_MS)
  })

  it('干净缓冲区不重复存：contents[path] 与 saved[path] 是同一个字符串实例', async () => {
    await openTabs()

    const state = useWorkspaceStore.getState()
    const notShared = Object.keys(state.contents).filter(
      (path) => !Object.is(state.contents[path], state.saved[path]),
    )
    expect(notShared).toEqual([])
    // 20 个标签 = 恰好 20 个不同实例：一张 4 MB 的表，不是两张。
    expect(distinctInstances()).toBe(TAB_COUNT)
  })

  it('JSON.stringify 代理指标会把共享缓冲区算两次 —— 不能当内存读数', async () => {
    await openTabs()

    const state = useWorkspaceStore.getState()
    const contentsJson = JSON.stringify(state.contents).length
    const bothJson = JSON.stringify({ contents: state.contents, saved: state.saved }).length

    // 同一份文本在两个 map 里各序列化一次 ≈ 2×。这只是「stringify 会重复计数」的记录，
    // 不是「存了两份」的证据（上一条用例的实例数才是）。
    expect(bothJson).toBeGreaterThan(contentsJson * 1.9)
    expect(bothJson).toBeLessThan(contentsJson * 2.2)
  })

  it('脏标签才多一份：改过的标签两份文本不同实例（语义要求，非重复存储）', async () => {
    const buffers = await openTabs()
    const original = buffers[0]
    if (original === undefined) throw new Error('缓冲区没造出来')

    useWorkspaceStore.getState().updateContent('src/f0.ts', `${original}// edit`)

    const state = useWorkspaceStore.getState()
    expect(state.tabs[0]?.dirty).toBe(true)
    expect(Object.is(state.contents['src/f0.ts'], state.saved['src/f0.ts'])).toBe(false)
    // 19 个干净标签共享 19 份 + 这个脏标签的 2 份 = 21 个实例。
    expect(distinctInstances()).toBe(TAB_COUNT + 1)
  })

  it('setBuffer / 切换 / 关闭的耗时才在防回归阈值内，且关闭后不留残项', async () => {
    const buffers = await openTabs()
    const edited = `${buffers[0] ?? ''}// edit`

    const setStarted = performance.now()
    for (let index = 0; index < 200; index += 1) {
      useWorkspaceStore.getState().updateContent('src/f0.ts', `${edited} ${index}`)
    }
    const setMs = performance.now() - setStarted
    expect(setMs).toBeLessThan(BUDGET_MS)

    const switchStarted = performance.now()
    for (let round = 0; round < 100; round += 1) {
      for (let index = 0; index < TAB_COUNT; index += 1) {
        useWorkspaceStore.getState().activateTab(`src/f${index}.ts`)
      }
    }
    const switchMs = performance.now() - switchStarted
    expect(switchMs).toBeLessThan(BUDGET_MS)

    const closeStarted = performance.now()
    for (let index = 0; index < TAB_COUNT; index += 1) {
      useWorkspaceStore.getState().closeTab(`src/f${index}.ts`)
    }
    const closeMs = performance.now() - closeStarted
    expect(closeMs).toBeLessThan(BUDGET_MS)

    const closed = useWorkspaceStore.getState()
    expect(closed.tabs).toEqual([])
    expect(closed.contents).toEqual({})
    expect(closed.saved).toEqual({})
    expect(closed.cursors).toEqual({})
    expect(closed.activePath).toBeNull()
  })

  it('setBuffer 的耗时与缓冲区字节数无关（200KB ≈ 20KB，说明没逐次拷文本）', async () => {
    /** 在 20 标签的现场里对 `src/f0.ts` 连写 2000 次，返回总耗时。 */
    const setBufferCost = (bytes: number): number => {
      resetStore()
      const buffers = Array.from({ length: TAB_COUNT }, (_, index) => makeBuffer(index, bytes))
      installDispatch(buffers)
      // 直接铺状态（等价于 openFile 之后的样子），把测量集中到 updateContent 上。
      useWorkspaceStore.setState({
        tabs: buffers.map((_, index) => ({ path: `src/f${index}.ts`, dirty: false, version: 1 })),
        contents: Object.fromEntries(buffers.map((text, index) => [`src/f${index}.ts`, text])),
        saved: Object.fromEntries(buffers.map((text, index) => [`src/f${index}.ts`, text])),
        activePath: 'src/f0.ts',
      })
      const target = buffers[0] ?? ''
      const started = performance.now()
      for (let index = 0; index < 2000; index += 1) {
        useWorkspaceStore.getState().updateContent('src/f0.ts', `${target} ${index}`)
      }
      return performance.now() - started
    }

    const small = setBufferCost(20 * 1024)
    const large = setBufferCost(BUFFER_BYTES)
    // 本机实测 200KB 甚至比 20KB 还快（1.50 µs vs 1.80 µs / 次），比值 ≈ 1。
    // 这里只要求「没有随字节数放大」，留 5× 给噪声；真要是每次拷贝文本，200KB 会是 10× 以上。
    expect(large).toBeLessThan(Math.max(small, 0.5) * 5)
  })
})