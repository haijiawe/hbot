/**
 * UI 与核心之间唯一的 IPC 入口（docs/06-ide.md §8：所有动作都是 dispatch(command)）。
 *
 * 契约的唯一真源是 `crates/hbot-rpc/src/dto/`（command.rs / event.rs），经 ts-rs 生成到 `src/generated/`。
 * 本文件是 UI 侧的命令表（`workspace.tree` 这类命令 wire 上还没有，属于 M0 手写层），
 * 两种后端的形状差异集中在 `ipc-tauri.ts` / `ipc-mock.ts` 两个适配层里，
 * `dispatch()` 本身不写第二份命令表。
 *
 * 三种运行形态，按优先级：
 * 1. `window.__TAURI_INTERNALS__` 存在 → 真桌面壳：`invoke('dispatch', …)` + `hbot://event` 事件；
 * 2. `window.__HBOT__` 存在 → 旧的注入式桥（`hbot serve` WS 桥，M0 遗留）；
 * 3. 都没有 → 纯浏览器 dev 走 `ipc-mock.ts` 的内存后端。
 *
 * 运行态一律由事件驱动：`bot.start` / `bot.stop` 只表示"命令被接受"，状态改由
 * `bot.status` 事件推进（`isRunningEvent` 那种"用返回值改状态"的路径已删除）。
 */

import type {
  BotStatus as BotStatusDto,
  BotSummary as BotSummaryDto,
  Diagnostic as DiagnosticDto,
  Event as CoreEventDto,
  RunLog as RunLogDto,
  StepKind as StepKindDto,
} from '../generated'
import { dispatchViaTauri, isTauriRuntime, subscribeCoreEvents } from './ipc-tauri'
import { mockDispatch, subscribeMockEvents } from './ipc-mock'

export { IpcError, isUnimplementedError } from './ipc-tauri'

export type FileNodeKind = 'file' | 'dir'

/** 项目文件树节点；`path` 是项目内相对路径，统一用 `/`。 */
export interface FileNode {
  name: string
  path: string
  kind: FileNodeKind
  children?: FileNode[]
}

export interface ReadFileResult {
  path: string
  content: string
  /** 磁盘版本号，用于检测外部改动（M1 接入 watcher）。 */
  version: number
}

export interface WriteFileResult {
  path: string
  version: number
}

/**
 * 日志面板的级别（四档）。
 *
 * 契约里的 `LogLevel` 有五档（多一个 `trace`）；面板没有 trace 档，
 * 事件接入时把 `trace` 折成 `debug`（见 `run-store.ts` 的 `ingestEvent`）。
 */
export type LogLevel = 'debug' | 'info' | 'warn' | 'error'

/** 运行日志的一行；`at` 是 epoch 毫秒。 */
export interface LogLine {
  level: LogLevel
  source: string
  message: string
  at: number
}

/** bot 列表的一行（`bot_list` 的 `data`；生成物，勿手改）。 */
export type BotSummary = BotSummaryDto

/**
 * 一条诊断（问题面板的一行）。
 *
 * `path` 为空串表示项目级诊断（没有具体文件），这类条目**不能跳转**；`line` 为 0
 * 表示核心不知道位置（见 `crates/hbot-rpc/src/app/commands/files.rs` 的 `diagnostics`）。
 */
export type Diagnostic = DiagnosticDto

/** 诊断的严重级；顺序即问题面板的排序优先级（越靠前越严重）。 */
export const DIAGNOSTIC_SEVERITY_ORDER: readonly DiagnosticDto['severity'][] = [
  'error',
  'warning',
  'info',
  'hint',
]

/**
 * bot 运行状态。
 *
 * 直接复用生成的枚举（`stopped | starting | running | stopping | degraded`）：
 * 契约里**没有** `error` 态 —— 出错由 `bot.status` 事件的 `error` 文本表达，
 * 状态本身仍是一个合法的运行态（启动失败即 `stopped` + 错误原因）。
 */
export type BotStatus = BotStatusDto

/** `agent.send` 的返回：核心新建会话时回传 ID，UI 必须存下来续用同一个会话。 */
export interface AgentSendResult {
  /** 会话 ID：命令里传了就回显，传 `null` 时由核心分配。 */
  sessionId: string
}

/**
 * 全局搜索没有"结果类型"：`search_start` 的返回是 `data: null`，命中由
 * `file.search { searchId, matches }` 事件推来，所以在 `CommandOutputs` 里是 `null`，
 * 累加与分组在 `stores/search-store.ts`（事件经 `app/use-core-events.ts` 扇出）。
 */

/** 每个命令的入参（不含 `type` 字段）。 */
export interface CommandInputs {
  'workspace.tree': { root?: string }
  'fs.read': { path: string }
  'fs.write': { path: string; content: string }
  /**
   * 开始一次全局搜索。
   *
   * 返回 `data: null`：命中**不**经返回值，而是 `file.search` 事件；`searchId` 也只随
   * 该事件到达（命令本身不回传 ID）。`globs` 是 gitignore 语义的 override 列表
   * （`*.ts` 只搜 TS、`!vendor/**` 排除），由 `stores/search-store.ts` 的 `parseGlobs` 拆。
   */
  'search.start': { query: string; regex: boolean; globs: string[] }
  /** 取消一次搜索（best-effort；核心随即停止推事件，不会补一条"结果"）。 */
  'search.cancel': { searchId: string }
  /**
   * 请求诊断（问题面板的数据来源）。
   *
   * `path: null` = 整个项目；核心随后**必定**推一条 `diagnostics` 事件
   * （`items` 为空也推），`data` 里则是同一批条目。UI 以事件为准
   * （`stores/workspace-store.ts` 的 `ingestEvent`），命令的返回值只用于
   * 首帧兜底，避免"命令成功但事件丢了"时面板永远空着。
   */
  diagnostics: { path: string | null }
  /** 列出当前项目的 bot（无字段；`data` 是 `BotSummary[]`）。 */
  'bot.list': Record<never, never>
  'bot.start': { botId: string }
  'bot.stop': { botId: string }
  /**
   * 读运行日志。
   *
   * `botId: null` = 全项目日志（含没有 bot 归属的 Agent 行，`source: 'agent'`）；
   * 传具体 bot ID 时只看该 bot 的行。
   */
  'logs.tail': { botId: string | null; limit: number }
  /**
   * 发一条消息给 Agent。
   *
   * `sessionId: null` = 新建会话（核心分配 ID 并回传）；之后必须一直带同一个 ID，
   * 否则每句话都会开一个新会话（历史清零）。
   */
  'agent.send': { text: string; sessionId: string | null }
  'agent.cancel': { sessionId: string }
  'agent.approve': { approvalId: string; decision: 'approve' | 'deny'; always: boolean }
  'edit.apply': { editId: string; acceptHunks: number[] }
  'edit.reject': { editId: string }
  /**
   * 同步一个文件的断点（**全量覆盖**语义）。
   *
   * `lines` 是该文件当前全部断点行号（1-based，空数组 = 移除该文件的断点）；
   * 契约见 docs/02-architecture.md §6.5.1 的 `Breakpoints::set`。
   */
  'debug.set-breakpoints': { botId: string; file: string; lines: number[] }
  /** 继续运行到下一个断点（暂停期间核心阻塞等这一条）。 */
  'debug.resume': { botId: string }
  /** 单步：`over` 跳过 / `into` 进入 / `out` 跳出。 */
  'debug.step': { botId: string; kind: StepKindDto }
}

export type CommandType = keyof CommandInputs

/** 可分发命令的判别联合：`{ type: 'fs.read', path: '...' }`。 */
export type Command = {
  [K in CommandType]: { type: K } & CommandInputs[K]
}[CommandType]

/** 每个命令的结果。 */
export interface CommandOutputs {
  'workspace.tree': FileNode[]
  'fs.read': ReadFileResult
  'fs.write': WriteFileResult
  /** 搜"启停"只回 `data: null`；命中见 `file.search` 事件（上面的说明）。 */
  'search.start': null
  'search.cancel': null
  /** 诊断条目；条目为空表示"没有诊断"，与"命令失败"是两件事（失败会抛 `IpcError`）。 */
  diagnostics: Diagnostic[]
  'bot.list': BotSummary[]
  /** 启停只回"命令已接受"（`data: null`）；状态看 `bot.status` 事件。 */
  'bot.start': null
  'bot.stop': null
  /** 日志行（核心按从旧到新返回，最新一行在末尾；面板按原顺序渲染）。 */
  'logs.tail': RunLogDto[]
  'agent.send': AgentSendResult
  'agent.cancel': null
  'agent.approve': null
  'edit.apply': null
  'edit.reject': null
  'debug.set-breakpoints': null
  'debug.resume': null
  'debug.step': null
}

export type CommandResult<T extends Command> = CommandOutputs[T['type']]

/**
 * 旧的真实后端接入点（M0 遗留）：页面加载前挂 `window.__HBOT__ = { dispatch }`。
 *
 * Tauri 桌面壳不再走这条路径 —— 它由 `ipc-tauri.ts` 直接对 `__TAURI_INTERNALS__` 说话。
 */
export interface HbotBridge {
  dispatch(command: Command): Promise<unknown>
}

declare global {
  interface Window {
    __HBOT__?: HbotBridge
  }
}

/** 是否已接入真实后端（Tauri IPC 或 `hbot serve` 注入的桥）。 */
export function hasBridge(): boolean {
  if (isTauriRuntime()) return true
  return typeof window !== 'undefined' && window.__HBOT__ !== undefined
}

export function runtimeMode(): 'tauri' | 'mock' {
  return hasBridge() ? 'tauri' : 'mock'
}

/** 核心推送的事件（`crates/hbot-rpc/src/dto/event.rs` 的生成物，勿手改）。 */
export type CoreEvent = CoreEventDto

export type CoreEventHandler = (event: CoreEvent) => void

/**
 * 订阅核心事件（`hbot://event`），返回取消订阅函数。
 *
 * 消费者都在 `app/use-core-events.ts` 一处订阅后扇出：`run-store`（`bot.status` / `run.log`）、
 * `agent-store`（`agent.*` / `approval.request`）、`search-store`（`file.search`）——
 * store 之间不许互相 import（AGENTS.md §0.10）。
 *
 * 纯浏览器 dev 没有 `hbot://event` 通道，但 mock 后端有自己的事件总线（`search_start`
 * 会推 `file.search`），所以 mock 分支订阅的是它而不是空订阅 —— 否则搜索面板在唯一
 * 能跑的模式下永远是空的。旧桥（`window.__HBOT__`）本来就不产出事件，仍由 Tauri 分支
 * 之外的路径落到空订阅。
 * 订阅失败会 `console.error` 留痕，而不是让整个 IDE 静默失去实时性。
 */
export function onEvent(handler: CoreEventHandler): () => void {
  if (!isTauriRuntime()) return subscribeMockEvents(handler)

  let unsubscribe: (() => void) | undefined
  let cancelled = false

  void subscribeCoreEvents(handler)
    .then((off) => {
      if (cancelled) {
        off()
        return
      }
      unsubscribe = off
    })
    .catch((error: unknown) => {
      console.error('[ipc] 订阅核心事件失败', error)
    })

  return () => {
    cancelled = true
    unsubscribe?.()
  }
}

/**
 * 唯一分发入口。UI 任何动作都必须经这里，不直接 fetch / invoke / 改多个 store。
 *
 * 错误一律抛 `IpcError`（带 `code` / `detail`），调用方的 catch 会把它变成
 * 用户可见的错误条；**不存在"出错但返回空结果"的分支** —— 未实现的命令由
 * `ipc-tauri.ts` 抛「该功能未上线」，绝不返回空数据让界面显示成成功。
 */
export async function dispatch<T extends Command>(command: T): Promise<CommandOutputs[T['type']]> {
  const bridge = typeof window === 'undefined' ? undefined : window.__HBOT__
  if (bridge) {
    const result = await bridge.dispatch(command)
    return result as CommandOutputs[T['type']]
  }
  if (isTauriRuntime()) {
    return (await dispatchViaTauri(command)) as CommandOutputs[T['type']]
  }
  // mock 返回的是命令结果联合类型；按 T 收窄由调用方的判别字段保证。
  return (await mockDispatch(command)) as CommandOutputs[T['type']]
}