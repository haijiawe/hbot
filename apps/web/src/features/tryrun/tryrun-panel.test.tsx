// @vitest-environment jsdom
/**
 * 试跑面板的闭环（`features/tryrun/tryrun-panel.tsx`）。
 *
 * 钉一条真链路：渲染 → 选 bot → 开始试跑（拿到会话）→ 输入 → 发送 → 回复区出现文本。
 * 回复来自 `run-store` 的日志（本面板不另存日志），所以测试里直接往 `run-store` 送一条
 * `run.log` 事件 —— 这与真机 `Event::RunLog` 的归约路径是同一个函数。
 *
 * 为什么 mock `dispatch` 而不是走 `ipc-mock`：真机 wire 上一条 bot 也可能没有会话
 * （空数组），需要能把"空会话"和"没有 bot"分别摆出来；mock 后端的固定 bot/会话
 * 会把这两种边界盖住。`ipc-mock` 自己的试跑行为另有一组用例。
 */

import { act, cleanup, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BotSummary, Command, ConversationSummary } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { useTryRunStore } from '../../stores/tryrun-store'
import { TryRunPanel } from './tryrun-panel'

const BOT_LOCAL: BotSummary = {
  botId: 'bot-local',
  name: '本地 bot',
  platform: 'local',
  status: 'stopped',
  updatedAt: 1,
}

const BOT_SECOND: BotSummary = {
  botId: 'bot-second',
  name: '第二个 bot',
  platform: 'local',
  status: 'stopped',
  updatedAt: 2,
}

/** 真机 `local` 平台建出来的会话行：内部 id 是 UUID，`externalId` 才是 `local:default`。 */
const CONVERSATION: ConversationSummary = {
  conversationId: '11111111-1111-4111-8111-111111111111',
  externalId: 'local:default',
  kind: 'private',
}

/** 内部会话 id 的形状：核心 conversations 表的主键是 UUID。 */
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

/** 每个用例自己决定 `bot.list` 回什么（空列表也要能摆出来）。 */
let botsResponse: BotSummary[] = []

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  const stub = vi.fn(async (command: Command) => {
    switch (command.type) {
      case 'bot.list':
        return botsResponse
      case 'bot_conversations':
        return [CONVERSATION]
      default:
        return null
    }
  })
  return { ...actual, dispatch: stub }
})

/** 真机上这条事件由 `Event::RunLog` 经 `use-core-events` 送进 `run-store`。 */
function pushRunLog(botId: string, line: string, at: number): void {
  act(() => {
    useRunStore.getState().ingestEvent({ type: 'run.log', botId, level: 'info', line, at })
  })
}

beforeEach(() => {
  botsResponse = [BOT_LOCAL, BOT_SECOND]
  useRunStore.setState({ botId: '', status: 'stopped', logs: [], busy: false, error: null })
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
  vi.mocked(dispatch).mockClear()
})

afterEach(cleanup)

describe('TryRunPanel', () => {
  it('选 bot → 开始试跑 → 发送 → 回复区出现 bot 的那一行', async () => {
    const user = userEvent.setup()
    render(<TryRunPanel />)

    // 1. bot 列表来自 bot_list，显示 name 与 platform。
    const selector = await screen.findByLabelText('选择 bot')
    expect(screen.getByRole('option', { name: '本地 bot · local' })).toBeTruthy()

    // 2. 换到第二个 bot：这是"选择器真的生效"的证据（开始试跑时命令行里是第二个 id）。
    await user.selectOptions(selector, 'bot-second')
    expect(useTryRunStore.getState().selectedBotId).toBe('bot-second')

    // 3. 开始试跑：先 bot_start 再 bot_conversations，界面显示拿到的会话。
    await user.click(screen.getByRole('button', { name: '开始试跑' }))
    expect(await screen.findByText(`会话：${CONVERSATION.conversationId}`)).toBeTruthy()
    expect(vi.mocked(dispatch)).toHaveBeenCalledWith({ type: 'bot.start', botId: 'bot-second' })
    expect(vi.mocked(dispatch)).toHaveBeenCalledWith({
      type: 'bot_conversations',
      botId: 'bot-second',
    })

    // 4. 还没有回复时的空态（不是空白）。
    expect(screen.getByText('还没有回复')).toBeTruthy()

    // 5. 输入并发送：bot_inject 带着 bot id + 会话 id + 文本。
    await user.type(screen.getByLabelText('要注入的消息'), '你好')
    await user.click(screen.getByRole('button', { name: '发送' }))
    expect(vi.mocked(dispatch)).toHaveBeenCalledWith({
      type: 'bot_inject',
      botId: 'bot-second',
      conversationId: CONVERSATION.conversationId,
      text: '你好',
    })
    // 注入带的是内部 UUID（不是 `local:default`）：拿错字段真机会 -32001。
    const injectCommand = vi
      .mocked(dispatch)
      .mock.calls.map(([command]) => command)
      .find((command) => command.type === 'bot_inject')
    expect(injectCommand?.type === 'bot_inject' ? injectCommand.conversationId : null).toMatch(
      UUID_PATTERN,
    )
    // 发送成功清空输入框（失败时要留着，见 store 的用例）。
    expect((screen.getByLabelText('要注入的消息') as HTMLInputElement).value).toBe('')

    // 6. bot 的回复经 run-store 到达：只有 [reply] 行，前缀被去掉。
    pushRunLog('bot-second', '[reply] 收到：你好', 5)
    expect(await screen.findByText('收到：你好')).toBeTruthy()
    expect(screen.getByRole('list', { name: '试跑回复' })).toBeTruthy()
    expect(screen.queryByText('还没有回复')).toBeNull()
  })

  it('只有带 [reply] 前缀的 bot 行进回复区，别的日志行不进', async () => {
    const user = userEvent.setup()
    render(<TryRunPanel />)

    await screen.findByLabelText('选择 bot')
    await user.click(screen.getByRole('button', { name: '开始试跑' }))
    await screen.findByText(`会话：${CONVERSATION.conversationId}`)

    pushRunLog('bot-local', '普通 run log，不是回复', 1)
    pushRunLog('bot-local', '[reply] 第一行', 2)
    pushRunLog('bot-local', '[reply] 第二行', 3)

    const list = await screen.findByRole('list', { name: '试跑回复' })
    const items = Array.from(list.querySelectorAll('li')).map((item) => item.textContent)
    expect(items).toEqual(['第一行', '第二行'])
    expect(screen.queryByText('普通 run log，不是回复')).toBeNull()
  })

  it('没有 bot 时给出可读提示，而不是空面板', async () => {
    botsResponse = []
    render(<TryRunPanel />)

    expect(await screen.findByText('还没有可试跑的 bot：先打开一个带 bot 的项目。')).toBeTruthy()
    expect(screen.queryByLabelText('选择 bot')).toBeNull()
  })
})