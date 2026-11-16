/**
 * 未保存缓冲区 journal 的接线层（App 的唯一调用点）。
 *
 * `lib/session-journal.ts` 是纯机制（节流、串行写、切项目门闩、beforeunload 冲刷）；
 * 本文件负责把它接到真实运行环境上：
 *
 * - **录制**：订阅 workspaceStore，标签 / 活动标签 / 脏标记 / 脏标签正文 / 光标有任何变化
 *   就 `journal.record(captureSession())`。节流交给 journal（500ms），这里**不再**防抖。
 * - **恢复**：`journal.restore()` → 项目身份一致才 `restoreSession(snapshot)`。
 * - **切项目**：必须过 `runProjectSwitch`（先 flush，写不进去就拦下切换，字还在）。
 * - **错误去处**：一律进 workspaceStore.error（`StatusBar` 的 `role="alert"`）——
 *   静默吞掉写盘失败等于骗用户"字已经存好了"。
 *
 * ## 跨项目污染的唯一判据
 *
 * 快照里带 `projectRoot`（核心 `project_open` 返回的规范化项目根绝对路径），
 * 恢复时**只有** `snapshot.projectRoot !== null && 当前 projectRoot === snapshot.projectRoot`
 * 才写回缓冲区。任一方为 `null`、或者不相等，都一律不恢复（`isSameProject`）——
 * 宁可少恢复一次，也不能把 A 项目没保存的正文写进 B 项目的同名文件。
 * 旧存档没有 `projectRoot`（读成 `null`）= 项目身份不明，同样不恢复。
 *
 * ## 单条记录的含义
 *
 * journal 只有一条记录（同一个库、同一个 key），所以"项目 A 的现场"会在切到 B、且 B 里
 * 真的产生编辑之后被 B 的现场覆盖。切走的瞬间 A 的正文已经 flush 进 journal，之后若在
 * B 里一个字都没敲，切回 A 仍能恢复；一旦在 B 里开始编辑，A 那份就被 B 顶掉。要做到
 * "每个项目各留一份"得把记录按项目分键（另一批），本批按单记录语义实现。
 */

import { captureSession, useWorkspaceStore, type WorkspaceState } from '../stores/workspace-store'
import { errorMessage } from './format'
import { runtimeMode } from './ipc'
import {
  ProjectSwitchBlockedError,
  createBufferJournal,
  createIndexedDbJournalStore,
  installBeforeUnloadFlush,
  runProjectSwitch,
  type BufferJournal,
  type JournalStore,
  type SessionSnapshot,
} from './session-journal'

/**
 * 浏览器 mock 模式的"项目身份"。
 *
 * mock 后端只有一份内置文件树（没有真项目根，也没有 `project_open`），所以给一个固定的
 * 假项目根，让"退出再开恢复未保存缓冲区"在唯一能徒手验证的模式下真的会跑。真桌面
 * （Tauri）只用 `project_open` 返回值，永远不会落在这个值上；因此 mock 的现场也永远
 * 不会和真项目互相认领。
 */
export const MOCK_PROJECT_ROOT = 'mock://project'

/** 恢复尝试的结果（给调用方 / 测试一个可断言的形状，不靠日志猜）。 */
export type RestoreOutcome =
  /** 快照存在且属于当前项目，已写回编辑器。 */
  | 'restored'
  /** journal 里没有快照（首次启动 / 上一次是干净退出）。 */
  | 'empty'
  /** 快照或当前项目身份未知（`null`）：不恢复，也不动快照。 */
  | 'unknown-project'
  /** 快照属于另一个项目：不恢复（跨项目污染的唯一防线）。 */
  | 'other-project'
  /** 读 journal 本身失败（隐私模式 / 库损坏）：已上报到状态栏。 */
  | 'read-failed'

/** 打开/切换项目的动作，由调用方注入（返回核心规范化后的项目根绝对路径）。 */
export type ProjectOpener = (path: string) => Promise<string>

export interface SessionPersistenceOptions {
  /** 注入存储后端（测试用 `createMemoryJournalStore`）；默认 IndexedDB。只在第一次 start 生效。 */
  store?: JournalStore
}

interface PersistenceRuntime {
  journal: BufferJournal
  unsubscribe: () => void
  unbindBeforeUnload: () => void
}

/** 单例：journal 必须唯一，否则两个实例会互相覆盖同一条记录。 */
let runtime: PersistenceRuntime | null = null

/**
 * 恢复 / 切项目期间挂起录制。
 *
 * 为什么需要：`restoreSession` 会 `set` 一次 store，如果此时正在录制，journal 里那份
 * **读回来的快照**会先被"当前（空）现场"盖掉——真机上 `fs.read` 走 IPC 可能超过 500ms，
 * 定时写会在恢复完成之前把旧快照刷成空的，用户没保存的字就没了。
 */
let suspendDepth = 0

function reportJournalError(error: unknown): void {
  useWorkspaceStore.getState().reportError(`未保存缓冲区没能写进本地 journal：${errorMessage(error)}`)
}

/**
 * 编辑器现场是否变了（决定要不要 `record`）。
 *
 * 只认与快照有关的那几个字段：`set({ files })`、诊断、布局、错误条这些变化不该触发
 * 一次 journal 写入（`loadTree` / `diagnostics` 事件都很频繁）。
 */
function sessionChanged(previous: WorkspaceState, next: WorkspaceState): boolean {
  if (previous.projectRoot !== next.projectRoot) return true
  if (previous.activePath !== next.activePath) return true
  if (previous.tabs !== next.tabs) {
    if (previous.tabs.length !== next.tabs.length) return true
    for (let index = 0; index < next.tabs.length; index += 1) {
      const before = previous.tabs[index]
      const after = next.tabs[index]
      if (before === after) continue
      if (before === undefined || after === undefined) return true
      if (before.path !== after.path || before.dirty !== after.dirty) return true
    }
  }
  // 脏标签的正文：用户每敲一个字 `contents` 都是新引用。
  for (const tab of next.tabs) {
    if (!tab.dirty) continue
    if (previous.contents[tab.path] !== next.contents[tab.path]) return true
  }
  // 光标：编辑器每帧都可能上报，合并窗口在 journal 里。
  if (previous.cursors !== next.cursors) {
    for (const tab of next.tabs) {
      const before = previous.cursors[tab.path]
      const after = next.cursors[tab.path]
      if (before === after) continue
      if (before === undefined || after === undefined) return true
      if (before.line !== after.line || before.column !== after.column) return true
    }
  }
  return false
}

/** 快照是否属于当前项目（跨项目污染的**唯一**判据，任一方未知都算否）。 */
export function isSameProject(snapshot: SessionSnapshot, currentRoot: string | null): boolean {
  return snapshot.projectRoot !== null && currentRoot !== null && snapshot.projectRoot === currentRoot
}

/** 建运行时（幂等）；第一次调用时接上订阅与 beforeunload。 */
function ensureRuntime(options: SessionPersistenceOptions): PersistenceRuntime {
  if (runtime !== null) return runtime

  // mock 模式先定下项目身份，再开始订阅（否则这次的 set 会被录成一份"空现场"）。
  if (useWorkspaceStore.getState().projectRoot === null && runtimeMode() === 'mock') {
    useWorkspaceStore.getState().setProjectRoot(MOCK_PROJECT_ROOT)
  }

  const journal = createBufferJournal(options.store ?? createIndexedDbJournalStore(), {
    onError: reportJournalError,
  })
  const unsubscribe = useWorkspaceStore.subscribe((state, previous) => {
    if (suspendDepth > 0) return
    if (sessionChanged(previous, state)) journal.record(captureSession())
  })
  const unbindBeforeUnload = installBeforeUnloadFlush(journal, reportJournalError)
  runtime = { journal, unsubscribe, unbindBeforeUnload }

  // 启动时已经有标签（未来"先开项目再挂外壳"的形态）才需要补录一份；
  // 空现场不写——否则会在恢复读盘之前把上一次的快照刷成"什么都没有"。
  if (useWorkspaceStore.getState().tabs.length > 0) journal.record(captureSession())

  return runtime
}

/** 恢复 / 切项目期间挂起录制（可嵌套：切项目里还会再调恢复）。 */
async function withRecordingSuspended<T>(action: () => Promise<T>): Promise<T> {
  suspendDepth += 1
  try {
    return await action()
  } finally {
    suspendDepth -= 1
  }
}

/**
 * 真正的恢复逻辑（内部用：调用方已经挂起了录制）。
 *
 * `reportMismatch` 只影响"要不要把丢弃的原因弹到状态栏"：
 * - 启动恢复（`restoreSessionIfSameProject`）要说，否则用户以为字还在；
 * - 切项目（`switchProject`）不说 —— 旧现场在过门闩时已经 flush 落盘，用户是主动切走的，
 *   再弹一句"那份现场属于另一个项目"只会让人以为出了错。
 */
async function restoreFromJournal(reportMismatch = true): Promise<RestoreOutcome> {
  const active = ensureRuntime({})
  let snapshot: SessionSnapshot | null
  try {
    snapshot = await active.journal.restore()
  } catch (error) {
    reportJournalError(error)
    return 'read-failed'
  }
  if (snapshot === null) return 'empty'

  const currentRoot = useWorkspaceStore.getState().projectRoot
  if (!isSameProject(snapshot, currentRoot)) {
    const dropped = snapshot.tabs.filter((tab) => tab.dirty).length
    if (reportMismatch && dropped > 0) {
      if (currentRoot === null) {
        // 启动时还没有打开项目：快照原样留着，等打开项目后再按身份恢复，不必打扰用户。
      } else if (snapshot.projectRoot === null) {
        // 旧存档没有项目身份：不能猜一个项目往上套，但要说清为什么没恢复。
        useWorkspaceStore
          .getState()
          .reportError(
            `上次有 ${dropped} 个文件没保存，但那份存档没有项目身份（旧版本存档），已跳过恢复。`,
          )
      } else {
        // 丢弃必须说出来：用户会以为自己的字还在（但绝不能写进别的项目）。
        useWorkspaceStore
          .getState()
          .reportError(
            `上次有 ${dropped} 个文件没保存，但那份现场属于另一个项目（${snapshot.projectRoot}），已跳过恢复：不会把它的内容写进当前项目。`,
          )
      }
    }
    return currentRoot === null || snapshot.projectRoot === null ? 'unknown-project' : 'other-project'
  }

  await useWorkspaceStore.getState().restoreSession(snapshot)
  return 'restored'
}

/**
 * 启动（或切到某个项目后）恢复上次现场。
 *
 * App 在 mount 时调一次：今天 web 侧还没有 `project_open` 的接线，启动时
 * `projectRoot` 是 `null`（mock 模式是 `MOCK_PROJECT_ROOT`），所以这里会诚实地返回
 * `unknown-project` 而不是"猜一个项目恢复"。真正的启动恢复要等打开项目那条链路接上
 * 之后由 `switchProject` 触发（见该函数）。
 */
export async function restoreSessionIfSameProject(): Promise<RestoreOutcome> {
  return withRecordingSuspended(restoreFromJournal)
}

/**
 * 切项目门闩 + 目标项目的现场恢复。
 *
 * **UI 入口接线属另一批**：`lib/ipc.ts` 的手写命令表里还没有 `project_open`
 * （wire 契约里有：`apps/web/src/generated/Command.ts`），所以"发命令"这一步必须以
 * `openProject` 的形式注入，避免在这里抄一份 IPC 协议（第二份真源）。接线时传
 * `path => dispatch 项目打开命令并返回规范化后的项目根`。
 *
 * 顺序是硬要求：先 `runProjectSwitch`（flush 失败抛 `ProjectSwitchBlockedError`，
 * `openProject` 一次都不会被调用），再换 `projectRoot`、重读文件树、恢复目标项目现场。
 */
export async function switchProject(path: string, openProject: ProjectOpener): Promise<void> {
  const active = ensureRuntime({})
  await withRecordingSuspended(async () => {
    try {
      await runProjectSwitch(active.journal, async () => {
        const root = await openProject(path)
        const workspace = useWorkspaceStore.getState()
        workspace.setProjectRoot(root)
        // 旧项目的标签必须在这里清掉：留着它们，下一次 captureSession() 就会把 A 的
        // 缓冲区记到 B 的身份下面 —— 那正是这套判据要防的跨项目污染。
        workspace.resetSession()
        await useWorkspaceStore.getState().loadTree()
      })
    } catch (error) {
      // 门闩拦下的切换必须让用户看见原因（状态栏），然后照常抛给调用方。
      if (error instanceof ProjectSwitchBlockedError) useWorkspaceStore.getState().reportError(error.message)
      throw error
    }
    await restoreFromJournal(false)
    // 恢复完立刻把（可能刚恢复出来的）现场记一份，并给快照盖上新的项目身份。
    active.journal.record(captureSession())
  })
}

/** 立刻把待写快照落盘（退出前 / 需要确定性的地方用；失败会 reject）。 */
export async function flushSessionJournal(): Promise<void> {
  if (runtime === null) return
  await runtime.journal.flush()
}

/**
 * 启动接线：建 journal、订阅编辑器状态、注册 beforeunload 冲刷。返回解绑函数
 * （`useEffect` 的 cleanup 直接用）。重复调用不会建第二个 journal。
 */
export function startSessionPersistence(options: SessionPersistenceOptions = {}): () => void {
  ensureRuntime(options)
  return stopSessionPersistence
}

/** 解绑并释放 journal（卸载 / 测试收尾）。 */
export function stopSessionPersistence(): void {
  if (runtime === null) return
  const active = runtime
  runtime = null
  active.unsubscribe()
  active.unbindBeforeUnload()
  // 卸下之前尽量把待写快照送出去（不 await：cleanup 是同步的）。
  void active.journal.flush().catch(reportJournalError)
  active.journal.dispose()
}