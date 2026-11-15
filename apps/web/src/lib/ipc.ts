/**
 * UI 与核心之间唯一的 IPC 入口（docs/06-ide.md §8：所有动作都是 dispatch(command)）。
 *
 * 契约的唯一真源是 `crates/hbot-rpc/src/dto/`（command.rs / event.rs），经 ts-rs 生成到 `src/generated/`。
 * M0 阶段生成物尚未落地，这里先声明最小命令集与结果类型；M1 换成生成 DTO 后，
 * 本文件只保留 `dispatch()` 的实现，不要再手写第二份命令表。
 *
 * 非 Tauri 环境（纯浏览器 dev）自动走 `ipc-mock.ts` 的内存后端；
 * Tauri / `hbot serve` 注入 `window.__HBOT__` 后自动切到真实实现。
 */

import { mockDispatch } from './ipc-mock'

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

export type LogLevel = 'debug' | 'info' | 'warn' | 'error'

/** 运行日志的一行；`at` 是 epoch 毫秒。 */
export interface LogLine {
  level: LogLevel
  source: string
  message: string
  at: number
}

export type BotStatus = 'stopped' | 'starting' | 'running' | 'stopping' | 'error'

export interface BotSnapshot {
  botId: string
  status: BotStatus
  queueDepth: number
}

/** 启停命令的返回：新状态 + 核心顺带产生的日志行。 */
export interface BotTransition {
  bot: BotSnapshot
  lines: LogLine[]
}

export interface SearchHit {
  path: string
  /** 1-based 行号。 */
  line: number
  /** 1-based 列号。 */
  column: number
  preview: string
}

export interface SearchResult {
  query: string
  hits: SearchHit[]
  /** 命中过多被截断（UI 需要提示用户）时为 true。 */
  truncated: boolean
}

export interface AgentReply {
  messageId: string
  reply: string
}

/** 每个命令的入参（不含 `type` 字段）。 */
export interface CommandInputs {
  'workspace.tree': { root?: string }
  'fs.read': { path: string }
  'fs.write': { path: string; content: string }
  search: { query: string; caseSensitive?: boolean }
  'bot.start': { botId: string }
  'bot.stop': { botId: string }
  'bot.status': { botId: string }
  'agent.send': { text: string }
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
  search: SearchResult
  'bot.start': BotTransition
  'bot.stop': BotTransition
  'bot.status': BotSnapshot
  'agent.send': AgentReply
}

export type CommandResult<T extends Command> = CommandOutputs[T['type']]

/**
 * 真实后端的接入点。Tauri 侧在页面加载前挂载：
 * `window.__HBOT__ = { dispatch }`（实现见 `crates/hbot-desktop/src/commands.rs`）。
 */
export interface HbotBridge {
  dispatch(command: Command): Promise<unknown>
}

declare global {
  interface Window {
    __HBOT__?: HbotBridge
  }
}

/** 是否已接入真实后端（Tauri / `hbot serve` WS 桥）。 */
export function hasBridge(): boolean {
  return typeof window !== 'undefined' && window.__HBOT__ !== undefined
}

export function runtimeMode(): 'tauri' | 'mock' {
  return hasBridge() ? 'tauri' : 'mock'
}

/**
 * 唯一分发入口。UI 任何动作都必须经这里，不直接 fetch / invoke / 改多个 store。
 */
export async function dispatch<T extends Command>(command: T): Promise<CommandOutputs[T['type']]> {
  const bridge = typeof window === 'undefined' ? undefined : window.__HBOT__
  if (bridge) {
    const result = await bridge.dispatch(command)
    return result as CommandOutputs[T['type']]
  }
  // mock 返回的是命令结果联合类型；按 T 收窄由调用方的判别字段保证。
  return (await mockDispatch(command)) as CommandOutputs[T['type']]
}