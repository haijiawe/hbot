/**
 * mock 传输里的项目命令（`lib/ipc-mock.ts`）。
 *
 * 为什么值得测：浏览器 dev / 没有桌面壳时的整条链路（命令面板 → 对话框 →
 * `switchProject` → `dispatch`）都落在这张内存实现上。这里最容易出的错是
 * "打开项目成功、但文件树还是空的" —— UI 看着像没打开任何东西。
 */

import { describe, expect, it } from 'vitest'
import type { FileNode } from './ipc'
import { MOCK_PROJECT_PATH, mockDispatch } from './ipc-mock'

describe('mockDispatch：project.open', () => {
  it('把用户输入归一成斜杠路径，并回 { path }（与核心的字符串 data 对齐）', async () => {
    const result = await mockDispatch({ type: 'project.open', path: 'C:\\work\\demo\\' })

    expect(result).toEqual({ path: MOCK_PROJECT_PATH })
  })

  it('打开项目之后文件树不是空的', async () => {
    await mockDispatch({ type: 'project.open', path: MOCK_PROJECT_PATH })

    const tree = await mockDispatch({ type: 'workspace.tree' })
    if (!Array.isArray(tree)) throw new Error('workspace.tree 应该回文件树数组')
    expect(tree.length).toBeGreaterThan(0)
    expect((tree as FileNode[]).some((node) => node.name === 'hbot.json')).toBe(true)
  })
})

describe('mockDispatch：project.create', () => {
  it('名称只有空白时按核心的 -32002 拒绝', async () => {
    await expect(mockDispatch({ type: 'project.create', path: 'C:/work/x', name: '   ' })).rejects.toThrow(
      'name 不能为空',
    )
  })

  it('同路径重复登记按 -32003 拒绝', async () => {
    await mockDispatch({ type: 'project.create', path: 'C:/work/dupe', name: 'dupe' })

    await expect(mockDispatch({ type: 'project.create', path: 'C:/work/dupe', name: 'dupe' })).rejects.toThrow(
      '-32003',
    )
  })

  it('成功时回项目摘要（path/name/entry）', async () => {
    const summary = await mockDispatch({ type: 'project.create', path: 'C:/work/fresh', name: 'fresh' })

    expect(summary).toEqual({ path: 'C:/work/fresh', name: 'fresh', entry: 'src/main.ts' })
  })
})

/**
 * 本地试跑（`platform: 'local'`）：`bot_inject` 的**唯一**可观察产出是 run log 里的
 * `[reply] ` 行（真机也是这条路，`crates/hbot-adapters/src/platforms/local/mod.rs`）。
 * 所以 mock 必须自己把"注入 → 逐行回显"演出来，否则浏览器里试跑面板永远是空的。
 */
describe('mockDispatch：本地试跑', () => {
  const BOT_ID = 'demo-bot'
  /** 内部会话 id 是 UUID（conversations 表主键）；`local:default` 是平台侧 externalId。 */
  const CONVERSATION_ID = '11111111-1111-4111-8111-111111111111'
  const EXTERNAL_ID = 'local:default'
  const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

  it('bot_conversations 回本地平台的固定会话：conversationId 是 UUID、externalId 是 local:default', async () => {
    const conversations = await mockDispatch({ type: 'bot_conversations', botId: BOT_ID })

    expect(conversations).toEqual([
      { conversationId: CONVERSATION_ID, externalId: EXTERNAL_ID, kind: 'private' },
    ])
    // 两个 id 不能混用：`bot_inject` 只认 conversationId，拿错就是真机的 -32001。
    expect(CONVERSATION_ID).toMatch(UUID_PATTERN)
    expect(CONVERSATION_ID).not.toBe(EXTERNAL_ID)
  })

  it('往 externalId 注入被拒绝（mock 不替"字段拿错"兜底）', async () => {
    await mockDispatch({ type: 'bot.start', botId: BOT_ID })

    await expect(
      mockDispatch({
        type: 'bot_inject',
        botId: BOT_ID,
        conversationId: EXTERNAL_ID,
        text: '你好',
      }),
    ).rejects.toThrow('会话不存在')
  })

  it('bot 没在运行时注入被拒绝，而不是静默丢弃', async () => {
    await mockDispatch({ type: 'bot.stop', botId: BOT_ID })

    await expect(
      mockDispatch({
        type: 'bot_inject',
        botId: BOT_ID,
        conversationId: CONVERSATION_ID,
        text: '你好',
      }),
    ).rejects.toThrow('未在运行')
  })

  it('多行文本逐行回显成 [reply] 日志，并能从 logs.tail 读回来', async () => {
    await mockDispatch({ type: 'bot.start', botId: BOT_ID })
    await mockDispatch({
      type: 'bot_inject',
      botId: BOT_ID,
      conversationId: CONVERSATION_ID,
      text: '你好\n第二行',
    })

    const rows = await mockDispatch({ type: 'logs.tail', botId: BOT_ID, limit: 10 })
    if (!Array.isArray(rows)) throw new Error('logs.tail 应该回日志数组')
    const tail = (rows as { line: string; source: string }[]).slice(-2)

    expect(tail.map((row) => row.line)).toEqual(['[reply] 收到：你好', '[reply] 收到：第二行'])
    expect(tail.every((row) => row.source === 'bot')).toBe(true)
  })

  it('空文本（含只有空行）不产生日志，按拒绝处理', async () => {
    await mockDispatch({ type: 'bot.start', botId: BOT_ID })

    await expect(
      mockDispatch({
        type: 'bot_inject',
        botId: BOT_ID,
        conversationId: CONVERSATION_ID,
        text: '  \n  ',
      }),
    ).rejects.toThrow('text 不能为空')
  })
})