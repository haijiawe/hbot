/**
 * tryRunStore：本地试跑的**会话归属**（选哪个 bot、打哪个会话、命令在不在途）。
 *
 * 边界（既有分层约定，别越界）：
 * - 不 import `run-store` / `workspace-store` / `agent-store`（store 之间不许互相引用）。
 *   回复区需要日志，由**面板组件**从 `useRunStore` 读已归约好的 `logs` 再投影
 *   （`features/tryrun/replies.ts` 的 `repliesOfBot`）—— 这样日志只有 `run-store`
 *   一份缓冲，事件也仍然只在 `app/use-core-events.ts` 一处订阅。
 * - 也不 import 组件；本文件只有命令与状态。
 *
 * 三种"没成"要分开表达，别都塞进 error：
 * - `notice`：合法的空结果（`bot_conversations` 回空数组 = 这个 bot 还没有会话行；
 *   后端契约里它不是错误，只有 bot 不存在才是 -32001），重试一次仍空才落到这里；
 * - `error`：命令真的失败（原文来自核心，不吞）；
 * - `starting` / `sending` / `stopping`：只用于禁用按钮，不代表 bot 运行态
 *   （运行态的唯一真源是 `bot.status` → `run-store`）。
 */

import { create } from 'zustand'
import { errorMessage } from '../lib/format'
import type { BotSummary } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

export interface TryRunState {
  /** `bot_list` 的当前快照（选择器的数据来源）。 */
  bots: BotSummary[]
  /** 至少查过一次 `bot_list`：区分"还没查"与"查了但没有 bot"。 */
  botsLoaded: boolean
  selectedBotId: string | null
  /**
   * 注入的目标会话：`bot_conversations[0].conversationId`，即核心 conversations 表的
   * **内部 UUID**（不是平台侧的 `externalId`，`local` 平台那个是 `local:default` 字面量）。
   * `null` 表示还没开始试跑（或空列表重试后仍然没拿到会话）。
   */
  conversationId: string | null
  starting: boolean
  sending: boolean
  stopping: boolean
  /** 不是错误但必须让用户看见的说明（如「这个 bot 还没有会话」）。 */
  notice: string | null
  error: string | null
  /** 最近一次成功注入：状态真的更新了才有值（面板用它在成功后清空输入框）。 */
  lastInjected: { text: string; at: number } | null
  /** 成功注入的条数（纯计数，便于断言/显示"已试跑几条"）。 */
  injectedCount: number

  /** 读 `bot_list` 并保持当前选择（选择失效时回落到第一条）。 */
  loadBots: () => Promise<void>
  /** 换一个 bot：会话归属跟着失效，必须重新「开始试跑」。 */
  selectBot: (botId: string) => void
  /** `bot_start` → `bot_conversations` 的第一条；空数组是合法的"还没有会话"。 */
  start: () => Promise<void>
  /** `bot_inject`：返回是否发出去（空文本 / 没有会话直接拒绝，不发命令）。 */
  send: (text: string) => Promise<boolean>
  stop: () => Promise<void>
  dismissError: () => void
}

/** 「开始试跑后没拿到会话」：空数组按"还没就绪"处理（重试一次仍空才提示），不是错误。 */
export const NO_CONVERSATION_NOTICE = '正在准备会话…（还没有会话，稍后再点一次「开始试跑」重试）'

/**
 * 空会话列表时的重试间隔。
 *
 * 后端契约：`bot_start` 内部会**同步**把 `local` 平台的会话行建好（幂等），所以正常情况下
 * 一次就能拿到；留一次重试是为了兜住"启动刚返回、会话行还没可见"的那一瞬，避免把
 * 一个短暂的准备期显示成"这个 bot 没有会话"。
 */
const SESSION_RETRY_DELAY_MS = 150

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms)
  })
}

export const useTryRunStore = create<TryRunState>()((set, get) => ({
  bots: [],
  botsLoaded: false,
  selectedBotId: null,
  conversationId: null,
  starting: false,
  sending: false,
  stopping: false,
  notice: null,
  error: null,
  lastInjected: null,
  injectedCount: 0,

  async loadBots() {
    try {
      const bots = await dispatch({ type: 'bot.list' })
      const current = get().selectedBotId
      const keep = current !== null && bots.some((bot) => bot.botId === current)
      set({
        bots,
        botsLoaded: true,
        selectedBotId: keep ? current : (bots[0]?.botId ?? null),
      })
    } catch (error) {
      set({ botsLoaded: true, error: `读取 bot 列表失败：${errorMessage(error)}` })
    }
  },

  selectBot(botId) {
    if (get().selectedBotId === botId) return
    set({
      selectedBotId: botId,
      // 会话属于 bot：换 bot 之后原来的 conversationId 不能继续用。
      conversationId: null,
      notice: null,
      error: null,
      lastInjected: null,
    })
  },

  async start() {
    const botId = get().selectedBotId
    if (botId === null) {
      set({ error: '还没有选择 bot，没法开始试跑' })
      return
    }
    set({ starting: true, conversationId: null, notice: null, error: null })
    try {
      await dispatch({ type: 'bot.start', botId })
      // 契约（后端已确认）：bot 存在但没有会话时 `bot_conversations` **成功回 `[]`**，
      // 只有 bot 不存在才是 -32001 —— 所以空数组不能当错误抛。
      let conversations = await dispatch({ type: 'bot_conversations', botId })
      if (conversations.length === 0) {
        await delay(SESSION_RETRY_DELAY_MS)
        conversations = await dispatch({ type: 'bot_conversations', botId })
      }
      // 顺序由后端定（`ORDER BY kind, external_id`），UI 不再排序：直接取第一条。
      // 注意取的是 `conversationId`（内部 UUID），不是 `externalId`（`local:default`）——
      // `bot_inject` 只认前者。
      const first = conversations[0]
      if (first === undefined) {
        // 重试一次仍然空：说明这个 bot 暂时真的没有会话，明确告诉用户，且不抛。
        set({ starting: false, conversationId: null, notice: NO_CONVERSATION_NOTICE })
        return
      }
      set({ starting: false, conversationId: first.conversationId, notice: null })
    } catch (error) {
      // 失败原因来自核心（bot 不存在 / 未上线 / 会话查询被拒），原文展示。
      set({ starting: false, error: `开始试跑失败：${errorMessage(error)}` })
    }
  },

  async send(text) {
    const trimmed = text.trim()
    if (trimmed.length === 0) {
      // 空文本不发命令：面板的按钮也会禁用，这里再守一道（回车提交绕过 disabled）。
      set({ error: '消息是空的，没有发送' })
      return false
    }
    const { selectedBotId, conversationId } = get()
    if (selectedBotId === null || conversationId === null) {
      set({ error: '还没有可注入的会话，先点「开始试跑」' })
      return false
    }
    set({ sending: true, error: null })
    try {
      await dispatch({ type: 'bot_inject', botId: selectedBotId, conversationId, text: trimmed })
      set({
        sending: false,
        lastInjected: { text: trimmed, at: Date.now() },
        injectedCount: get().injectedCount + 1,
      })
      return true
    } catch (error) {
      // 关键：conversationId 保留、sending 复位 —— 用户改一下再点发送就能重试，
      // 不用重新「开始试跑」；错误原文给用户看，不许吞。
      set({ sending: false, error: `发送失败：${errorMessage(error)}` })
      return false
    }
  },

  async stop() {
    const botId = get().selectedBotId
    if (botId === null) return
    set({ stopping: true, error: null })
    try {
      await dispatch({ type: 'bot.stop', botId })
      // 停了就没有可注入的会话了：清掉它，按钮自然禁用（回复区仍显示历史回复）。
      set({ stopping: false, conversationId: null, notice: '试跑已停止' })
    } catch (error) {
      set({ stopping: false, error: `停止试跑失败：${errorMessage(error)}` })
    }
  },

  dismissError() {
    set({ error: null })
  },
}))