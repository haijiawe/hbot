/**
 * 试跑会话的状态机（`stores/tryrun-store.ts`）。
 *
 * 这里 mock `dispatch` 而不是走 `ipc-mock`：要断言"命令带了哪个 bot / 哪个会话"、
 * 以及"`bot_conversations` 回空数组不算错误"。mock 后端的固定会话会掩盖这两点。
 *
 * 三条不当成同一件事的"没成"各有断言：
 * - 空会话列表 → 先重试一次，仍空才给 `notice`（合法结果，不抛错）；
 * - 注入失败 → `error` 有原文，且 `conversationId` 保留（改一下就能重试）；
 * - 空文本 → 不发命令（`dispatch` 一次都没调）。
 *
 * 会话 id 必须是**内部 UUID**：`bot_inject` 只认 `conversationId`，`externalId`
 * （`local:default`）传进去真机会 -32001。下面有一条用例专门钉这个形状。
 */

import { beforeEach, describe, expect, it, vi } from 'vitest'

const { dispatchMock } = vi.hoisted(() => ({ dispatchMock: vi.fn() }))

vi.mock('../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/ipc')>()
  return { ...actual, dispatch: dispatchMock }
})

import type { BotSummary, ConversationSummary } from '../lib/ipc'
import { NO_CONVERSATION_NOTICE, useTryRunStore } from './tryrun-store'

const BOT: BotSummary = {
  botId: 'bot-1',
  name: '本地 bot',
  platform: 'local',
  status: 'stopped',
  updatedAt: 1,
}

/** 真机 `local` 平台建出来的会话行：内部 id 是 UUID，`externalId` 才是 `local:default`。 */
const CONVERSATION: ConversationSummary = {
  conversationId: '11111111-1111-4111-8111-111111111111',
  externalId: 'local:default',
  kind: 'private',
}

/** 内部会话 id 的形状：核心 conversations 表的主键是 UUID。 */
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

function resetStore(): void {
  useTryRunStore.setState({
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
  })
}

beforeEach(() => {
  dispatchMock.mockReset()
  resetStore()
})

describe('tryRunStore.loadBots', () => {
  it('默认选中第一条 bot', async () => {
    dispatchMock.mockResolvedValueOnce([BOT])

    await useTryRunStore.getState().loadBots()

    expect(dispatchMock).toHaveBeenCalledWith({ type: 'bot.list' })
    const state = useTryRunStore.getState()
    expect(state.bots).toEqual([BOT])
    expect(state.botsLoaded).toBe(true)
    expect(state.selectedBotId).toBe('bot-1')
  })

  it('刷新时保留仍然存在的选择，失效则回落到第一条', async () => {
    useTryRunStore.setState({ selectedBotId: 'bot-2' })
    dispatchMock.mockResolvedValueOnce([BOT])

    await useTryRunStore.getState().loadBots()

    expect(useTryRunStore.getState().selectedBotId).toBe('bot-1')
  })

  it('读取失败写进 error（不吞）', async () => {
    dispatchMock.mockRejectedValueOnce(new Error('core 没起来'))

    await useTryRunStore.getState().loadBots()

    const state = useTryRunStore.getState()
    expect(state.error).toContain('读取 bot 列表失败')
    expect(state.error).toContain('core 没起来')
    expect(state.botsLoaded).toBe(true)
  })
})

describe('tryRunStore.start', () => {
  it('先 bot_start 再 bot_conversations，拿第一条会话', async () => {
    useTryRunStore.setState({ bots: [BOT], botsLoaded: true, selectedBotId: 'bot-1' })
    dispatchMock.mockResolvedValueOnce(null).mockResolvedValueOnce([CONVERSATION])

    await useTryRunStore.getState().start()

    expect(dispatchMock).toHaveBeenNthCalledWith(1, { type: 'bot.start', botId: 'bot-1' })
    expect(dispatchMock).toHaveBeenNthCalledWith(2, {
      type: 'bot_conversations',
      botId: 'bot-1',
    })
    const state = useTryRunStore.getState()
    expect(state.conversationId).toBe(CONVERSATION.conversationId)
    expect(state.starting).toBe(false)
    expect(state.error).toBeNull()
    expect(state.notice).toBeNull()
  })

  it('bot_conversations 回空数组时先重试一次，仍空才提示「正在准备会话」，不抛错', async () => {
    useTryRunStore.setState({ bots: [BOT], botsLoaded: true, selectedBotId: 'bot-1' })
    // bot_start 回 null（成功），两次 bot_conversations 都回空数组。
    dispatchMock.mockImplementation(async (command: { type: string }) =>
      command.type === 'bot_conversations' ? [] : null,
    )

    // 没有 reject 就是"不抛"：空数组是合法结果，不是异常。
    await expect(useTryRunStore.getState().start()).resolves.toBeUndefined()

    const state = useTryRunStore.getState()
    expect(state.notice).toBe(NO_CONVERSATION_NOTICE)
    expect(state.notice).toContain('正在准备会话')
    expect(state.notice).toContain('还没有会话')
    expect(state.error).toBeNull()
    expect(state.conversationId).toBeNull()
    expect(state.starting).toBe(false)
    // 空结果重试过一次：一共 1 次 bot_start + 2 次 bot_conversations。
    expect(dispatchMock).toHaveBeenCalledTimes(3)
    expect(dispatchMock.mock.calls.filter(([command]) => command.type === 'bot_conversations')).toHaveLength(2)
  })

  it('第一次空、第二次拿到会话时视为"刚就绪"，不打扰用户', async () => {
    useTryRunStore.setState({ bots: [BOT], botsLoaded: true, selectedBotId: 'bot-1' })
    dispatchMock
      .mockResolvedValueOnce(null) // bot_start
      .mockResolvedValueOnce([]) // 第一次查：还没可见
      .mockResolvedValueOnce([CONVERSATION]) // 重试：拿到了

    await useTryRunStore.getState().start()

    const state = useTryRunStore.getState()
    expect(state.conversationId).toBe(CONVERSATION.conversationId)
    expect(state.notice).toBeNull()
    expect(state.error).toBeNull()
  })

  it('bot_start 失败时错误可见，且没有会话可注入', async () => {
    useTryRunStore.setState({ bots: [BOT], botsLoaded: true, selectedBotId: 'bot-1' })
    dispatchMock.mockRejectedValueOnce(new Error('bot 不存在'))

    await useTryRunStore.getState().start()

    const state = useTryRunStore.getState()
    expect(state.error).toContain('开始试跑失败')
    expect(state.error).toContain('bot 不存在')
    expect(state.conversationId).toBeNull()
    expect(state.starting).toBe(false)
    // 会话查询没被调用：起不来就没有"第一条会话"可言。
    expect(dispatchMock).toHaveBeenCalledTimes(1)
  })
})

describe('tryRunStore.send', () => {
  beforeEach(() => {
    useTryRunStore.setState({
      bots: [BOT],
      botsLoaded: true,
      selectedBotId: 'bot-1',
      conversationId: CONVERSATION.conversationId,
    })
  })

  it('注入成功后状态真的更新（lastInjected / injectedCount / sending 复位）', async () => {
    dispatchMock.mockResolvedValueOnce(null)

    const sent = await useTryRunStore.getState().send('  你好  ')

    expect(sent).toBe(true)
    expect(dispatchMock).toHaveBeenCalledWith({
      type: 'bot_inject',
      botId: 'bot-1',
      conversationId: CONVERSATION.conversationId,
      // 前后空白被去掉：注入的是用户看得见的那句话。
      text: '你好',
    })
    // 钉形状：注入的是内部 UUID，不是平台侧的 `local:default`（传错真机 -32001）。
    const injectCommand = dispatchMock.mock.calls[0]?.[0] as { conversationId?: string } | undefined
    expect(injectCommand?.conversationId).toMatch(UUID_PATTERN)
    expect(injectCommand?.conversationId).not.toBe(CONVERSATION.externalId)
    expect(injectCommand?.conversationId).not.toBe('local:default')
    const state = useTryRunStore.getState()
    expect(state.lastInjected?.text).toBe('你好')
    expect(state.injectedCount).toBe(1)
    expect(state.sending).toBe(false)
    expect(state.error).toBeNull()
  })

  it('注入失败时错误可见，会话与状态保留（可以原样重试）', async () => {
    dispatchMock.mockRejectedValueOnce(new Error('bot 未在运行'))

    const sent = await useTryRunStore.getState().send('你好')

    expect(sent).toBe(false)
    const failed = useTryRunStore.getState()
    expect(failed.error).toContain('发送失败')
    expect(failed.error).toContain('bot 未在运行')
    expect(failed.sending).toBe(false)
    expect(failed.conversationId).toBe(CONVERSATION.conversationId)
    expect(failed.lastInjected).toBeNull()
    expect(failed.injectedCount).toBe(0)

    // 回滚到可重试：不改任何东西，再发一次就能成功，错误被清掉。
    dispatchMock.mockResolvedValueOnce(null)
    await expect(useTryRunStore.getState().send('你好')).resolves.toBe(true)
    const retried = useTryRunStore.getState()
    expect(retried.error).toBeNull()
    expect(retried.injectedCount).toBe(1)
  })

  it('空文本不发命令', async () => {
    const sent = await useTryRunStore.getState().send('   ')

    expect(sent).toBe(false)
    expect(dispatchMock).not.toHaveBeenCalled()
    expect(useTryRunStore.getState().error).toBe('消息是空的，没有发送')
  })

  it('还没拿到会话时不发命令，并提示先开始试跑', async () => {
    useTryRunStore.setState({ conversationId: null })

    const sent = await useTryRunStore.getState().send('你好')

    expect(sent).toBe(false)
    expect(dispatchMock).not.toHaveBeenCalled()
    expect(useTryRunStore.getState().error).toContain('先点「开始试跑」')
  })
})

describe('tryRunStore.selectBot / stop', () => {
  it('换 bot 后旧会话失效（不能拿 A 的会话去注入 B）', () => {
    useTryRunStore.setState({
      bots: [BOT],
      botsLoaded: true,
      selectedBotId: 'bot-1',
      conversationId: CONVERSATION.conversationId,
      notice: '旧提示',
    })

    useTryRunStore.getState().selectBot('bot-2')

    const state = useTryRunStore.getState()
    expect(state.selectedBotId).toBe('bot-2')
    expect(state.conversationId).toBeNull()
    expect(state.notice).toBeNull()
  })

  it('停止试跑清掉会话并给出提示', async () => {
    useTryRunStore.setState({
      bots: [BOT],
      botsLoaded: true,
      selectedBotId: 'bot-1',
      conversationId: CONVERSATION.conversationId,
    })
    dispatchMock.mockResolvedValueOnce(null)

    await useTryRunStore.getState().stop()

    expect(dispatchMock).toHaveBeenCalledWith({ type: 'bot.stop', botId: 'bot-1' })
    const state = useTryRunStore.getState()
    expect(state.conversationId).toBeNull()
    expect(state.notice).toBe('试跑已停止')
    expect(state.stopping).toBe(false)
  })

  it('停止失败时错误可见，会话保留（还能继续注入）', async () => {
    useTryRunStore.setState({
      bots: [BOT],
      botsLoaded: true,
      selectedBotId: 'bot-1',
      conversationId: CONVERSATION.conversationId,
    })
    dispatchMock.mockRejectedValueOnce(new Error('停不下来'))

    await useTryRunStore.getState().stop()

    const state = useTryRunStore.getState()
    expect(state.error).toContain('停止试跑失败')
    expect(state.conversationId).toBe(CONVERSATION.conversationId)
    expect(state.stopping).toBe(false)
  })
})