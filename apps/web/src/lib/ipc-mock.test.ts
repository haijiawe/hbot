/**
 * mock 传输里的项目命令（`lib/ipc-mock.ts`）。
 *
 * 为什么值得测：浏览器 dev / 没有桌面壳时的整条链路（命令面板 → 对话框 →
 * `switchProject` → `dispatch`）都落在这张内存实现上。这里最容易出的错是
 * "打开项目成功、但文件树还是空的" —— UI 看着像没打开任何东西。
 */

import { describe, expect, it } from 'vitest'
import type { DeviceSettings } from '../generated'
import type { CoreEvent, FileNode } from './ipc'
import {
  MOCK_PROJECT_PATH,
  mockBreakpointsOf,
  mockDispatch,
  simulateMockPause,
  subscribeMockEvents,
} from './ipc-mock'

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

/**
 * 宿主设备设置（`device_settings_get` / `device_settings_set`）。
 *
 * mock 必须**真的存一份**而不是回固定对象：否则浏览器里点"保存"看着毫无反应，
 * 组件测试也就永远测不出"保存之后读回新值"这条链路。
 */
describe('mockDispatch：宿主设备设置', () => {
  it('set 之后 get 能读回新值（内存里那份被真的覆盖了，不是回常量）', async () => {
    await mockDispatch({
      type: 'deviceSettingsSet',
      settings: {
        enabled: true,
        capabilities: ['screen', 'input'],
        allowedApps: ['PathOfExile.exe'],
        defaultMinutes: 7,
        maxMinutes: 20,
        maxActionsPerSession: 500,
      },
    })

    const readBack = (await mockDispatch({ type: 'deviceSettingsGet' })) as DeviceSettings

    expect(readBack.enabled).toBe(true)
    expect(readBack.capabilities).toEqual(['screen', 'input'])
    expect(readBack.allowedApps).toEqual(['PathOfExile.exe'])
    expect(readBack.defaultMinutes).toBe(7)
    expect(readBack.maxMinutes).toBe(20)
    expect(readBack.maxActionsPerSession).toBe(500)
  })

  it('`maxMinutes` 超 30 按 -32002 拒绝，并且原设置一个字段都不动', async () => {
    const before = (await mockDispatch({ type: 'deviceSettingsGet' })) as DeviceSettings

    await expect(
      mockDispatch({
        type: 'deviceSettingsSet',
        settings: { ...before, maxMinutes: 31 },
      }),
    ).rejects.toThrow('-32002')

    expect(await mockDispatch({ type: 'deviceSettingsGet' })).toEqual(before)
  })
})

/**
 * 条件断点在 mock 链路上的两个观察点：
 * 1. `debug.set-breakpoints` 的 `breakpoints` 里带的条件要**原样存下**（不是只记行号），
 *    否则浏览器里写好的条件切一次文件就没了；
 * 2. `debug.paused` 事件里的 `condition` 要能透传到订阅者 —— 调试面板的"命中的条件"
 *    完全依赖这一个字段。mock 不自动伪造暂停（见 `simulateMockPause` 的注释），
 *    所以第二条要用显式入口造事件。
 */
describe('mockDispatch：条件断点', () => {
  it('mock 记下断点条目（含条件），空数组等于清空该文件', async () => {
    await mockDispatch({
      type: 'debug.set-breakpoints',
      botId: 'demo-bot',
      file: 'src/cond.ts',
      breakpoints: [
        { line: 3, condition: null },
        { line: 9, condition: 'count > 3' },
      ],
    })
    expect(mockBreakpointsOf('src/cond.ts')).toEqual([
      { line: 3, condition: null },
      { line: 9, condition: 'count > 3' },
    ])

    await mockDispatch({
      type: 'debug.set-breakpoints',
      botId: 'demo-bot',
      file: 'src/cond.ts',
      breakpoints: [],
    })
    expect(mockBreakpointsOf('src/cond.ts')).toEqual([])
  })

  it('simulateMockPause 造的 debug.paused 带条件，订阅者能收到', () => {
    const events: CoreEvent[] = []
    const unsubscribe = subscribeMockEvents((event) => events.push(event))

    simulateMockPause({ file: 'src/cond.ts', line: 9, condition: 'count > 3' })
    simulateMockPause({ file: 'src/cond.ts', line: 3 })
    unsubscribe()

    expect(events).toEqual([
      {
        type: 'debug.paused',
        botId: 'demo-bot',
        file: 'src/cond.ts',
        line: 9,
        condition: 'count > 3',
        scopes: [],
        callStack: [{ name: 'onMessage', file: 'src/cond.ts', line: 9, column: 1 }],
      },
      {
        type: 'debug.paused',
        botId: 'demo-bot',
        file: 'src/cond.ts',
        line: 3,
        condition: null,
        scopes: [],
        callStack: [{ name: 'onMessage', file: 'src/cond.ts', line: 3, column: 1 }],
      },
    ])
  })
})