/**
 * Tauri 传输层：把 webview 里的 `window.__TAURI_INTERNALS__` 接到 `ipc.ts` 的公共契约上。
 *
 * 为什么不用 `@tauri-apps/api`：`apps/web` 的依赖里没有它（`tauri.conf.json` 也没开
 * `withGlobalTauri`），而 invoke / 事件订阅本身就是两条固定的 IPC 消息，直接对着
 * Tauri 的原始协议说更省也更好审：
 *
 * - 命令：`invoke('dispatch', { command })` → `crates/hbot-desktop/src/lib.rs` 的
 *   `dispatch`，返回 `Response`（`src/generated/Response.ts`，唯一错误通道）；
 * - 事件：`invoke('plugin:event|listen', { event, target, handler })` 订阅
 *   `hbot://event`（`crates/hbot-desktop/src/lib.rs` 的 `EVENT_CHANNEL`，改名 = 协议变更）。
 *
 * 本文件只做**形状适配**（M0 手写契约 ↔ 生成 DTO），不放业务判断：
 * - `workspace.tree` → 递归 `file_list`（wire 只有一层列表）；
 * - `fs.read` / `fs.write` → `file_read` / `file_write`，内容哈希在本地缓存，
 *   写回时作为 `baseHash`（wire 的并发保护，禁止静默覆盖）；
 * - `bot.list` → `bot_list`；`bot.start` / `bot.stop` → `bot_start` / `bot_stop`
 *   （启停只回 `data: null`，运行态由 `bot.status` 事件推进）；
 * - `bot_conversations` → `bot_conversations`（同名）：`data` 是 `ConversationSummary[]`，
 *   **空数组是合法结果**（这个 bot 还没有会话），所以走 `callWire` 而不是 void 变体；
 * - `bot_inject` → `bot_inject`（同名）：只回 `data: null`，本地平台的回复由
 *   `run.log` 事件逐行推来（`[reply] ` 前缀），命令本身没有返回值；
 * - `agent.send` → `agent_send`（`sessionId: null` 表示新建会话，核心回 `data.sessionId`）；
 * - `agent.approve` / `edit.apply` / `edit.reject` → 同名 snake_case 命令；
 * - `search.start` / `search.cancel` → `search_start` / `search_cancel`，两者都只回
 *   `data: null`：命中由 `file.search` 事件推来（`searchId` 也只随事件到达，命令不回传），
 *   所以这里**不能**编一个结果对象，也不许把 `data: null` 当成错误（用 `callWireVoid`）。
 * - `diagnostics` → `diagnostics`（同名）：`data` 是 `Diagnostic[]`，核心同时再推一条
 *   `diagnostics` 事件；条目为空数组是**合法结果**，不能走 `callWireVoid`。
 * - `project.open` → `project_open`：`data` 是 `ProjectOpenResult{path}`（核心规范化后的
 *   绝对路径；不是 `ProjectSummary`，见 `projectOpenResult` 的注释）。旧的裸字符串
 *   形状仍然兼容，统一归一成 `{ path }`；
 *   `project.create` → `project_create`：`data` 才是 `ProjectSummary`。
 */

import type {
  ApiError,
  BotSummary,
  Command as WireCommand,
  ConversationSummary,
  Diagnostic,
  DirEntry,
  Event as CoreEvent,
  FileContent,
  ProjectSummary,
  Response,
  RunLog,
} from '../generated'
import type { Command, FileNode, ProjectOpenResult, ReadFileResult, WriteFileResult } from './ipc'

/** 事件通道名。必须与 `crates/hbot-desktop/src/lib.rs` 的 `EVENT_CHANNEL` 一致。 */
export const EVENT_CHANNEL = 'hbot://event'

/** 错误码：内部错误（`docs/02-architecture.md` §9.4）。 */
const INTERNAL_CODE = -32006

/** 「功能未实现」的约定前缀（§9.4：码也是 -32006，靠前缀区分）。 */
const UNIMPLEMENTED_PREFIX = '尚未实现：'

/** 目录树递归的层数上限：防止核心返回环状路径时把 UI 卡死。 */
const MAX_TREE_DEPTH = 32

/** 不往树里放的重目录：与 `ipc-mock.ts` 的守卫保持一致的取舍。 */
const SKIPPED_DIRS = new Set(['.git', '.hbot', 'node_modules', 'target', 'dist'])

/**
 * 协议错误（`Response.type === 'error'`）的 JS 形态。
 *
 * 存在意义：`stores/*` 的 catch 走 `errorMessage()`，它只读 `error.message`，
 * 所以必须把 `code` 与可读文案一起塞进 message；同时保留 `code` 供 UI 分档
 * （-32001 不存在 / -32005 拒绝 / -32006 未上线…）。
 */
export class IpcError extends Error {
  /** 协议错误码（`-32001..=-32006`）。 */
  readonly code: number
  /** 原始的 `ApiError`（含结构化 data）。 */
  readonly detail: ApiError

  constructor(detail: ApiError) {
    super(renderApiError(detail))
    this.name = 'IpcError'
    this.code = detail.code
    this.detail = detail
  }
}

/** 是否为「该功能未上线」（`-32006` + `尚未实现：` 前缀）。 */
export function isUnimplementedError(error: unknown): boolean {
  return (
    error instanceof IpcError &&
    error.code === INTERNAL_CODE &&
    error.detail.message.startsWith(UNIMPLEMENTED_PREFIX)
  )
}

/**
 * 把 `ApiError` 变成给人看的一句话。
 *
 * §9.4 要求「功能未实现」在 UI 上显示成"该功能未上线"，而不是当成参数错误，
 * 所以这里按前缀改写；其余错误保留错误码，方便用户报告时直接抄给开发者。
 */
function renderApiError(error: ApiError): string {
  if (error.code === INTERNAL_CODE && error.message.startsWith(UNIMPLEMENTED_PREFIX)) {
    return `该功能未上线：${error.message.slice(UNIMPLEMENTED_PREFIX.length)}`
  }
  return `[${error.code}] ${error.message}`
}

// ---------------------------------------------------------------------------
// Tauri 原始协议（`window.__TAURI_INTERNALS__` / `window.__TAURI__`）
// ---------------------------------------------------------------------------

/** Tauri 2 注入的底层接口（`__TAURI_INTERNALS__`）。 */
interface TauriInternals {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>
  /**
   * 把一个 JS 回调登记到 `window` 上并返回数字 id；事件监听把这个 id 交给 Rust 侧，
   * Rust 再用 `window._<id>(payload)` 回调（`@tauri-apps/api` 内部同一条路径）。
   */
  transformCallback(callback?: (payload: unknown) => void, once?: boolean): number
}

/** `withGlobalTauri` 开着时注入的高层封装；本仓没开，仅作尽力兼容。 */
interface TauriGlobal {
  core?: {
    invoke?<T>(cmd: string, args?: Record<string, unknown>): Promise<T>
  }
  event?: {
    listen(name: string, handler: (payload: unknown) => void): Promise<() => void>
  }
}

/**
 * 事件插件的内部登记表：Tauri 的注入脚本会往这里挂 `unregisterListener`。
 * 取消订阅时必须调它，否则 `transformCallback` 登在 `window` 上的回调不会释放
 * （`@tauri-apps/api` 的 `listen` 取消函数就是这么做的）。
 */
interface TauriEventPluginInternals {
  unregisterListener?: (event: string, handlerId: number) => void
}

declare global {
  interface Window {
    __TAURI_INTERNALS__?: TauriInternals
    __TAURI_EVENT_PLUGIN_INTERNALS__?: TauriEventPluginInternals
    __TAURI__?: TauriGlobal
    /** Tauri 2 注入的运行时标记（`@tauri-apps/api` 的 `isTauri()` 读的就是它）。 */
    isTauri?: boolean
  }
}

/** 是否运行在 Tauri webview 里（`ipc.ts` 用它决定走真实传输还是 mock）。 */
export function isTauriRuntime(): boolean {
  if (typeof window === 'undefined') return false
  return (
    window.__TAURI_INTERNALS__ !== undefined ||
    window.__TAURI__ !== undefined ||
    window.isTauri === true
  )
}

/** `invoke`：优先 `__TAURI_INTERNALS__`，退化到 `__TAURI__.core.invoke`。 */
async function invokeWire(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
  const internals = typeof window === 'undefined' ? undefined : window.__TAURI_INTERNALS__
  if (internals) return internals.invoke<unknown>(cmd, args)

  const globalInvoke = typeof window === 'undefined' ? undefined : window.__TAURI__?.core?.invoke
  if (globalInvoke) return globalInvoke<unknown>(cmd, args)

  throw new IpcError({
    code: INTERNAL_CODE,
    message: 'Tauri invoke 不可用：页面里既没有 __TAURI_INTERNALS__ 也没有 __TAURI__.core',
    data: null,
  })
}

/** 订阅一个 Tauri 事件；返回取消订阅函数。 */
async function listenWire(
  event: string,
  handler: (payload: unknown) => void,
): Promise<() => void> {
  const internals = typeof window === 'undefined' ? undefined : window.__TAURI_INTERNALS__
  if (internals) {
    const handlerId = internals.transformCallback((payload) => handler(payload), false)
    await internals.invoke('plugin:event|listen', {
      event,
      target: { kind: 'Any' },
      handler: handlerId,
    })
    return () => {
      // 先释放 window 上的 JS 回调（官方 listen 的取消函数也先做这一步），
      // 再通知 Rust 侧停止推送。顺序反了不会报错，但会留下一个永远收不到数据的回调。
      window.__TAURI_EVENT_PLUGIN_INTERNALS__?.unregisterListener?.(event, handlerId)

      // 取消订阅是 fire-and-forget：窗口正在关闭时 unlisten 失败很正常，
      // 但失败要留痕（不能静默吞掉，否则事件泄漏永远查不出来）。
      void internals
        .invoke('plugin:event|unlisten', { event, eventId: handlerId })
        .catch((error: unknown) => {
          console.warn(`[ipc] 取消订阅 ${event} 失败`, error)
        })
    }
  }

  const listen = typeof window === 'undefined' ? undefined : window.__TAURI__?.event?.listen
  if (listen) return listen(event, handler)

  console.warn('[ipc] 当前运行时没有事件通道，onEvent 订阅不会收到任何事件')
  return () => {}
}

// ---------------------------------------------------------------------------
// Response 解析
// ---------------------------------------------------------------------------

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

function isResponse(value: unknown): value is Response {
  if (!isRecord(value)) return false
  return value.type === 'ok' || value.type === 'error'
}

/** 发一条 wire 命令并拆掉 `Response` 外壳：`error` → 抛 `IpcError`。 */
async function callWire<T>(label: string, command: WireCommand): Promise<T> {
  const response = await invokeWire('dispatch', { command })

  if (!isResponse(response)) {
    throw new IpcError({
      code: INTERNAL_CODE,
      message: `桌面壳返回了无法解析的 Response（${label}）`,
      data: null,
    })
  }

  if (response.type === 'error') throw new IpcError(response.error)

  if (response.data === null) {
    // 快照类命令必须有 data；null 说明壳层与契约对不上，不能当成空成功。
    throw new IpcError({
      code: INTERNAL_CODE,
      message: `${label} 没有返回数据（core 返回了 data: null）`,
      data: null,
    })
  }

  return response.data as T
}

/**
 * 发一条"成功时没有返回值"的 wire 命令（启停、取消、审批、编辑应用/拒绝）。
 *
 * 与 `callWire` 的唯一区别：允许 `data: null` —— 这些命令的成功就是"没有数据"。
 * 协议错误照样抛 `IpcError`：空成功只对**契约里确实没有返回值**的命令成立，
 * 不能拿它给"该有数据却没有"的命令兜底。
 */
async function callWireVoid(label: string, command: WireCommand): Promise<null> {
  const response = await invokeWire('dispatch', { command })

  if (!isResponse(response)) {
    throw new IpcError({
      code: INTERNAL_CODE,
      message: `桌面壳返回了无法解析的 Response（${label}）`,
      data: null,
    })
  }

  if (response.type === 'error') throw new IpcError(response.error)

  return null
}

/**
 * 发消息给 Agent，并把会话 ID 拿回来。
 *
 * 契约：`sessionId: null` = 新建会话，核心在 `data.sessionId` 回传新 ID；
 * 带了 ID 时行为不变、`data` 是 `null`（回显命令里那个 ID 即可）。
 * 新建却没拿到 ID 是**硬错误**：静默放行会让 UI 每句话开一个新会话。
 */
async function agentSend(
  text: string,
  sessionId: string | null,
): Promise<{ sessionId: string }> {
  const response = await invokeWire('dispatch', {
    command: { type: 'agent_send', sessionId, text },
  })

  if (!isResponse(response)) {
    throw new IpcError({
      code: INTERNAL_CODE,
      message: '桌面壳返回了无法解析的 Response（agent_send）',
      data: null,
    })
  }

  if (response.type === 'error') throw new IpcError(response.error)

  if (sessionId !== null) return { sessionId }

  const data = response.data
  const assigned = isRecord(data) && typeof data.sessionId === 'string' ? data.sessionId : null
  if (assigned === null) {
    throw new IpcError({
      code: INTERNAL_CODE,
      message: 'agent_send 新建会话没有返回 sessionId：UI 无法续用这个会话',
      data: null,
    })
  }

  return { sessionId: assigned }
}

// ---------------------------------------------------------------------------
// 命令适配（M0 手写契约 → 生成 DTO）
// ---------------------------------------------------------------------------

/** `file_read` / `file_write` 拿到的内容哈希，写回时作 `baseHash`（按路径缓存）。 */
const contentHashes = new Map<string, string>()

/** 读文件。`ReadFileResult.version` 在 wire 上没有对应物，恒为 0（见下面的说明）。 */
async function readFile(path: string): Promise<ReadFileResult> {
  const data = await callWire<FileContent>('file_read', { type: 'file_read', path })
  contentHashes.set(path, data.hash)
  // M0：wire 只给内容哈希，没有数值版本号；`EditorTab.version` 目前只存不比较，
  // 真正的外部改动检测走写回时的 baseHash（哈希不匹配核心会拒绝覆盖）。
  return { path, content: data.content, version: 0 }
}

/** 写文件：带上最近一次读到的哈希，让核心能发现"打开后文件被别人改过"。 */
async function writeFile(path: string, content: string): Promise<WriteFileResult> {
  const data = await callWire<FileContent>('file_write', {
    type: 'file_write',
    path,
    content,
    baseHash: contentHashes.get(path) ?? null,
  })
  contentHashes.set(path, data.hash)
  return { path, version: 0 }
}

/** 目录优先、再按名字排序（与资源管理器的习惯一致）。 */
function compareNodes(left: FileNode, right: FileNode): number {
  if (left.kind !== right.kind) return left.kind === 'dir' ? -1 : 1
  return left.name.localeCompare(right.name)
}

/** 递归列目录：wire 的 `file_list` 只给一层，树由 UI 侧拼。 */
async function listDir(dir: string, depth: number): Promise<FileNode[]> {
  if (depth > MAX_TREE_DEPTH) {
    throw new IpcError({
      code: INTERNAL_CODE,
      message: `目录树超过 ${MAX_TREE_DEPTH} 层（${dir}）：疑似路径成环，已停止递归`,
      data: null,
    })
  }

  const entries = await callWire<DirEntry[]>('file_list', { type: 'file_list', path: dir })
  const nodes: FileNode[] = []

  for (const entry of entries) {
    if (SKIPPED_DIRS.has(entry.name)) continue
    const node: FileNode = {
      name: entry.name,
      path: entry.path,
      kind: entry.isDir ? 'dir' : 'file',
    }
    if (entry.isDir) node.children = await listDir(entry.path, depth + 1)
    nodes.push(node)
  }

  return nodes.sort(compareNodes)
}

/**
 * `project_open` 的 data 形状适配（生成 DTO：`src/generated/ProjectOpenResult.ts`）。
 *
 * **主路径**：核心回 `{ "path": "<规范化后的绝对路径>" }` —— `system.rs` 的
 * `project_open` 成功分支把 `dto::ProjectOpenResult` 序列化进 `Response::with_data`。
 * 只有 `path`：打开一个已有目录不要求它已经有 `hbot.json`，所以没有 `name` / `entry`
 * （那是 `project_create` 的 `ProjectSummary`）。
 *
 * **兼容路径**：旧核心 / 旧快照回的是裸字符串（`Value::String(root)`）。桌面壳与
 * web 产物不是同一刻升级的，把旧形状判死会让"打开项目"在过渡期整条链路失败。
 *
 * 两者都对不上就抛错，绝不猜一个路径 —— 项目根是 journal 的项目身份，猜错会把
 * A 项目没保存的正文写进 B 项目。空字符串同样算"没给路径"（核心不会回空串）。
 */
function projectOpenResult(raw: unknown): ProjectOpenResult {
  // 主路径：生成 DTO 的形状。多余字段一律忽略，只取 `path`。
  if (isRecord(raw) && typeof raw.path === 'string' && raw.path.length > 0) {
    return { path: raw.path }
  }
  // 兼容路径：旧核心的裸字符串。
  if (typeof raw === 'string' && raw.length > 0) return { path: raw }
  throw new IpcError({
    code: INTERNAL_CODE,
    message: 'project_open 返回了无法识别的形状（既不是 { path } 对象，也不是路径字符串）',
    data: null,
  })
}

/**
 * 把 M0 手写命令翻译成 wire 命令。
 *
 * 返回值故意是 `unknown`：具体类型由 `ipc.ts` 的 `CommandOutputs` 收窄（与
 * `__HBOT__` 桥那条路径一致），这里只保证**形状按契约**。
 */
export async function dispatchViaTauri(command: Command): Promise<unknown> {
  switch (command.type) {
    case 'workspace.tree':
      return listDir(command.root ?? '.', 0)
    case 'fs.read':
      return readFile(command.path)
    case 'fs.write':
      return writeFile(command.path, command.content)
    case 'search.start':
      return callWireVoid('search_start', {
        type: 'search_start',
        query: command.query,
        regex: command.regex,
        globs: command.globs,
      })
    case 'search.cancel':
      return callWireVoid('search_cancel', {
        type: 'search_cancel',
        searchId: command.searchId,
      })
    case 'diagnostics':
      // 返回 `Diagnostic[]`（核心 `ok_data(&items)`）；条目为空是合法结果，不是错误。
      return callWire<Diagnostic[]>('diagnostics', {
        type: 'diagnostics',
        path: command.path,
      })
    case 'bot.list':
      return callWire<BotSummary[]>('bot_list', { type: 'bot_list' })
    case 'bot.start':
      return callWireVoid('bot_start', { type: 'bot_start', botId: command.botId })
    case 'bot.stop':
      return callWireVoid('bot_stop', { type: 'bot_stop', botId: command.botId })
    case 'bot_conversations':
      // 空数组是合法结果（还没登记会话）：`callWire` 只把 `data: null` 当协议错误，
      // 空数组照常返回 —— 用 `callWireVoid` 会把这一档成功变成"没有数据"。
      return callWire<ConversationSummary[]>('bot_conversations', {
        type: 'bot_conversations',
        botId: command.botId,
      })
    case 'bot_inject':
      // 本地平台不联网：回复由 `run.log` 事件推来，这里只确认命令被接受。
      return callWireVoid('bot_inject', {
        type: 'bot_inject',
        botId: command.botId,
        conversationId: command.conversationId,
        text: command.text,
      })
    case 'logs.tail':
      // `botId: null` 是全项目日志（含 Agent 行）；核心按从旧到新返回。
      return callWire<RunLog[]>('logs_tail', {
        type: 'logs_tail',
        botId: command.botId,
        limit: command.limit,
      })
    case 'agent.send':
      return agentSend(command.text, command.sessionId)
    case 'agent.cancel':
      return callWireVoid('agent_cancel', {
        type: 'agent_cancel',
        sessionId: command.sessionId,
      })
    case 'agent.approve':
      return callWireVoid('agent_approve', {
        type: 'agent_approve',
        approvalId: command.approvalId,
        decision: command.decision,
        always: command.always,
      })
    case 'edit.apply':
      return callWireVoid('edit_apply', {
        type: 'edit_apply',
        editId: command.editId,
        acceptHunks: command.acceptHunks,
      })
    case 'edit.reject':
      return callWireVoid('edit_reject', { type: 'edit_reject', editId: command.editId })
    case 'debug.set-breakpoints':
      // 全量覆盖：`lines: []` 就是"移除该文件的断点"（契约 §6.5.1）。
      return callWireVoid('debug_set_breakpoints', {
        type: 'debug_set_breakpoints',
        botId: command.botId,
        file: command.file,
        lines: command.lines,
      })
    case 'debug.resume':
      return callWireVoid('debug_resume', { type: 'debug_resume', botId: command.botId })
    case 'debug.step':
      return callWireVoid('debug_step', {
        type: 'debug_step',
        botId: command.botId,
        kind: command.kind,
      })
    case 'project.open':
      // `project_open` 成功时 data 是 `ProjectOpenResult{path}`（兼容旧的裸字符串）；
      // 归一化与"不认识的形状就抛错"见 `projectOpenResult`。
      return projectOpenResult(
        await callWire<unknown>('project_open', {
          type: 'project_open',
          path: command.path,
        }),
      )
    case 'project.create':
      return callWire<ProjectSummary>('project_create', {
        type: 'project_create',
        path: command.path,
        name: command.name,
      })
  }
}

// ---------------------------------------------------------------------------
// 事件订阅
// ---------------------------------------------------------------------------

function isCoreEvent(value: unknown): value is CoreEvent {
  if (!isRecord(value)) return false
  // 生成 DTO 的判别字段是 dotted 的 `type`（如 `agent.delta`）；具体变体由
  // `src/generated/Event.ts` 决定，这里不重复维护一张事件名表（第二份真源）。
  return typeof value.type === 'string'
}

/**
 * 订阅核心事件（`hbot://event`），回调拿到生成 DTO 的 `Event` 联合。
 *
 * 未知形状的 payload 只警告并丢弃：事件是附加通道，坏一条不该让 UI 崩掉。
 */
export async function subscribeCoreEvents(handler: (event: CoreEvent) => void): Promise<() => void> {
  return listenWire(EVENT_CHANNEL, (payload) => {
    if (!isCoreEvent(payload)) {
      console.warn('[ipc] 丢弃无法识别的事件', payload)
      return
    }
    handler(payload)
  })
}