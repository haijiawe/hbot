/**
 * 未保存缓冲区 journal（IndexedDB）+ 切项目门闩。
 *
 * 为什么不是 localStorage：一个 1 MB 的源文件就超了 localStorage 的 5 MB 配额（而且
 * 超限时 `setItem` 直接抛，用户敲的字就没了）。IndexedDB 存大字符串没问题。
 * 布局/侧栏那些小数据仍然走 localStorage（见 `lib/layout-prefs.ts`），各归各的。
 *
 * 写盘节流 500ms：用户每敲一个字都写一次 IndexedDB 太浪费；窗口内的改动合并成一份
 * 最新快照。切项目前必须 `await flush()`，**失败就拦下切换**——宁可不让切，也不能丢字。
 */

import { errorMessage } from './format'

/** 节流窗口（docs/06-ide.md §6：未保存缓冲区 journal 节流 500ms）。 */
export const JOURNAL_THROTTLE_MS = 500

/** 一个打开着的标签页在 journal 里的样子。 */
export interface JournalTab {
  path: string
  /** 有未保存改动（缓冲区 ≠ 磁盘）。 */
  dirty: boolean
  /** 光标位置（1-based，列也是 1-based）。 */
  cursor: { line: number; column: number }
  /** 缓冲区内容；**只有脏标签才存**（干净标签重启后从磁盘重读，省配额）。 */
  content: string | null
}

/** 一次会话的编辑器现场：标签顺序、光标、活动标签、未保存缓冲区。 */
export interface SessionSnapshot {
  version: 1
  /** 写入时间（毫秒），恢复时只用于展示"上次恢复于…"。 */
  savedAt: number
  activePath: string | null
  tabs: JournalTab[]
}

/** journal 的存储后端（IndexedDB 实现 / 测试用的内存实现）。 */
export interface JournalStore {
  read: () => Promise<SessionSnapshot | null>
  write: (snapshot: SessionSnapshot) => Promise<void>
}

export interface BufferJournalOptions {
  /** 节流窗口，默认 500ms。 */
  delayMs?: number
  /** 后台写失败时的回调（不静默吞：App 会把它显示到状态栏）。 */
  onError?: (error: unknown) => void
  now?: () => number
}

export interface BufferJournal {
  /** 记一份新快照（节流：窗口内只写一次，保留最新）。 */
  record: (snapshot: SessionSnapshot) => void
  /** 立刻把待写快照落盘；失败会 reject（调用方必须处理，见 `runProjectSwitch`）。 */
  flush: () => Promise<void>
  /** 读回上次的现场；没有或格式不认识返回 `null`。 */
  restore: () => Promise<SessionSnapshot | null>
  hasPending: () => boolean
  dispose: () => void
}

/**
 * 建一个 journal。
 *
 * 串行化：同一时刻只有一次 `store.write` 在飞，后到的 `flush` 排在它后面 ——
 * 否则切项目那次 flush 可能与节流定时器的写并发，谁先落盘说不清。
 */
export function createBufferJournal(
  store: JournalStore,
  options: BufferJournalOptions = {},
): BufferJournal {
  const delayMs = options.delayMs ?? JOURNAL_THROTTLE_MS
  const now = options.now ?? (() => Date.now())
  let pending: SessionSnapshot | null = null
  let timer: ReturnType<typeof setTimeout> | null = null
  let chain: Promise<void> = Promise.resolve()
  let disposed = false

  function clearTimer(): void {
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }

  async function writePending(): Promise<void> {
    if (pending === null) return
    const snapshot = pending
    pending = null
    try {
      await store.write({ ...snapshot, savedAt: now() })
    } catch (error) {
      // 写失败不丢快照：放回去，下一次 flush 还能重试（切项目会因此被拦下）。
      pending = pending ?? snapshot
      throw error
    }
  }

  function flush(): Promise<void> {
    clearTimer()
    // 每次 flush 都把写排进链上，保证不并发；失败只 reject 给本次调用方。
    const task = chain.then(writePending, writePending)
    chain = task.then(
      () => undefined,
      () => undefined,
    )
    return task
  }

  return {
    record(snapshot) {
      if (disposed) return
      pending = snapshot
      // 窗口内已经排过写：只更新待写快照，不再排第二个定时器（节流）。
      if (timer !== null) return
      timer = setTimeout(() => {
        timer = null
        void flush().catch((error: unknown) => options.onError?.(error))
      }, delayMs)
    },

    flush,

    async restore() {
      const raw = await store.read()
      return raw === null ? null : sanitizeSnapshot(raw)
    },

    hasPending() {
      return pending !== null
    },

    dispose() {
      disposed = true
      clearTimer()
    },
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

/** 把 IndexedDB 里读到的东西校验成快照（旧版本 / 手改过的数据一律当"没有"）。 */
export function sanitizeSnapshot(raw: unknown): SessionSnapshot | null {
  if (!isRecord(raw)) return null
  if (raw.version !== 1 || !Array.isArray(raw.tabs)) return null
  const tabs: JournalTab[] = []
  for (const item of raw.tabs) {
    if (!isRecord(item) || typeof item.path !== 'string' || item.path.length === 0) return null
    const cursor = isRecord(item.cursor) ? item.cursor : null
    const line = cursor && typeof cursor.line === 'number' ? cursor.line : 1
    const column = cursor && typeof cursor.column === 'number' ? cursor.column : 1
    const content = typeof item.content === 'string' ? item.content : null
    tabs.push({
      path: item.path,
      dirty: item.dirty === true || (content !== null && item.dirty !== false),
      cursor: { line: Math.max(1, Math.trunc(line)), column: Math.max(1, Math.trunc(column)) },
      content,
    })
  }
  const activePath = typeof raw.activePath === 'string' ? raw.activePath : null
  const savedAt = typeof raw.savedAt === 'number' ? raw.savedAt : 0
  return { version: 1, savedAt, activePath, tabs }
}

/** 测试 / 无 IndexedDB 环境用的内存后端（不写盘，进程内有效）。 */
export function createMemoryJournalStore(initial: SessionSnapshot | null = null): JournalStore {
  let current = initial
  return {
    async read() {
      return current
    },
    async write(snapshot) {
      current = snapshot
    },
  }
}

const DB_NAME = 'hbot-ide-journal'
const STORE_NAME = 'session'
const RECORD_KEY = 'current'

/**
 * IndexedDB 后端：一个库、一个 object store、一条记录（当前项目的现场）。
 *
 * 连接懒建且复用；`open` 失败（隐私模式 / 配额为 0）时错误向上抛，由 journal 的
 * `onError` 显示出来 —— 不做"失败就当成功"的静默降级，那样用户以为字被存了。
 */
export function createIndexedDbJournalStore(dbName = DB_NAME): JournalStore {
  let connection: Promise<IDBDatabase> | null = null

  function open(): Promise<IDBDatabase> {
    if (connection !== null) return connection
    connection = new Promise<IDBDatabase>((resolve, reject) => {
      if (typeof indexedDB === 'undefined') {
        reject(new Error('当前环境没有 IndexedDB，未保存缓冲区无法持久化'))
        return
      }
      const request = indexedDB.open(dbName, 1)
      request.onupgradeneeded = () => {
        const database = request.result
        if (!database.objectStoreNames.contains(STORE_NAME)) database.createObjectStore(STORE_NAME)
      }
      request.onsuccess = () => resolve(request.result)
      request.onerror = () => reject(request.error ?? new Error('打开 IndexedDB 失败'))
    })
    return connection
  }

  async function run<T>(mode: IDBTransactionMode, act: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
    const database = await open()
    return new Promise<T>((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, mode)
      const request = act(transaction.objectStore(STORE_NAME))
      request.onsuccess = () => resolve(request.result)
      request.onerror = () => reject(request.error ?? new Error('IndexedDB 请求失败'))
      transaction.onabort = () => reject(transaction.error ?? new Error('IndexedDB 事务被中止'))
    })
  }

  return {
    async read() {
      const value: unknown = await run('readonly', (store) => store.get(RECORD_KEY) as IDBRequest<unknown>)
      return value === undefined ? null : (value as SessionSnapshot | null)
    },
    async write(snapshot) {
      await run('readwrite', (store) => store.put(snapshot, RECORD_KEY) as IDBRequest<IDBValidKey>)
    },
  }
}

/** 切项目被拦下（journal 没写成功）。 */
export class ProjectSwitchBlockedError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProjectSwitchBlockedError'
  }
}

/**
 * 切项目门闩：先 flush 当前缓冲区，**成功才允许切**；失败抛 `ProjectSwitchBlockedError`，
 * `next` 一次都不会被调用（用户留在原项目，字还在）。
 */
export async function runProjectSwitch(
  journal: BufferJournal,
  next: () => void | Promise<void>,
): Promise<void> {
  try {
    await journal.flush()
  } catch (error) {
    throw new ProjectSwitchBlockedError(
      `切换项目被拦下：未保存缓冲区没能写进本地 journal（${errorMessage(error)}）。先重试保存再切。`,
    )
  }
  await next()
}

/**
 * `beforeunload` 时尽力 flush。
 *
 * 卸载阶段不能 `await`，只能"发出去"：这里主动 flush 并吞掉错误（浏览器正在关页面，
 * 没有地方给用户看错误）；真正保证不丢字的是"切项目必须 await flush"和 500ms 节流本身。
 * 返回解绑函数。
 */
export function installBeforeUnloadFlush(journal: BufferJournal, onError?: (error: unknown) => void): () => void {
  if (typeof window === 'undefined') return () => undefined
  const handler = (): void => {
    void journal.flush().catch((error: unknown) => onError?.(error))
  }
  window.addEventListener('beforeunload', handler)
  return () => window.removeEventListener('beforeunload', handler)
}