/**
 * 试跑回复的投影（`features/tryrun/replies.ts`）。
 *
 * 这里是 UI 侧唯一决定"哪一行算 bot 的回复"的地方，所以四条判据都钉死：
 * 1. 只认 `source === 'bot'`（`agent` 的日志不是回复）；
 * 2. 只认 `[reply] ` 前缀，且**前缀必须完整**（`[reply]` 后面没空格不算）；
 * 3. 去前缀后原样返回，顺序与日志一致；
 * 4. `[reply] ` 后面是空字符串时保留空回复 —— 那是 bot 真的回了一行空文本，
 *    不是"这条不算"。
 */

import { describe, expect, it } from 'vitest'
import type { LogLine } from '../../lib/ipc'
import { REPLY_PREFIX, extractReplies, repliesOfBot } from './replies'

describe('extractReplies', () => {
  it('只取 source 是 bot 且带 [reply] 前缀的行', () => {
    const replies = extractReplies([
      { source: 'bot', line: '[reply] 你好' },
      { source: 'agent', line: '[reply] 我是 agent 的自言自语' },
      { source: 'bot', line: '普通日志，没有前缀' },
      { source: 'bot', line: '[reply] 再见' },
    ])

    expect(replies).toEqual(['你好', '再见'])
  })

  it('去掉前缀后原样保留正文（前缀只去一次，正文里的方括号不动）', () => {
    const replies = extractReplies([{ source: 'bot', line: '[reply] [reply] 嵌套不是真前缀' }])

    expect(replies).toEqual(['[reply] 嵌套不是真前缀'])
  })

  it('保持日志的时间顺序', () => {
    const replies = extractReplies([
      { source: 'bot', line: '[reply] 第一行' },
      { source: 'bot', line: '[reply] 第二行' },
      { source: 'bot', line: '[reply] 第三行' },
    ])

    expect(replies).toEqual(['第一行', '第二行', '第三行'])
  })

  it('[reply] 后面是空字符串时保留一个空回复，而不是丢掉这一行', () => {
    const replies = extractReplies([
      { source: 'bot', line: REPLY_PREFIX },
      { source: 'bot', line: '[reply] 有内容' },
    ])

    expect(replies).toEqual(['', '有内容'])
    expect(replies).toHaveLength(2)
  })

  it('别的前缀/近似前缀不误取', () => {
    const replies = extractReplies([
      { source: 'bot', line: '[reply]缺空格' },
      { source: 'bot', line: '[replied] 不是回复' },
      { source: 'bot', line: ' [reply] 前面有空格' },
      { source: 'bot', line: '[agent] 这是 agent 的' },
      { source: 'bot', line: '[reply] 真的' },
    ])

    expect(replies).toEqual(['真的'])
  })

  it('空日志没有回复（不抛错）', () => {
    expect(extractReplies([])).toEqual([])
  })
})

describe('repliesOfBot', () => {
  function log(botId: string, message: string): LogLine {
    return { level: 'info', source: botId, message, at: 1 }
  }

  it('只取选中 bot 的行：多 bot 试跑时不把别人的回复算进来', () => {
    const logs = [
      log('bot-a', '[reply] A 的回复'),
      log('bot-b', '[reply] B 的回复'),
      log('bot-a', '[reply] A 的第二条'),
    ]

    expect(repliesOfBot(logs, 'bot-a')).toEqual(['A 的回复', 'A 的第二条'])
    expect(repliesOfBot(logs, 'bot-b')).toEqual(['B 的回复'])
  })

  it('没有选 bot（null / 空字符串）时没有回复', () => {
    const logs = [log('bot-a', '[reply] A 的回复')]

    expect(repliesOfBot(logs, null)).toEqual([])
    expect(repliesOfBot(logs, '')).toEqual([])
  })

  it('[agent] 来源的行即使带前缀也不是回复（run-store 把 agent 记成 "[agent]"）', () => {
    const logs = [log('[agent]', '[reply] 不是 bot 说的'), log('bot-a', '[reply] 是 bot 说的')]

    expect(repliesOfBot(logs, 'bot-a')).toEqual(['是 bot 说的'])
  })
})