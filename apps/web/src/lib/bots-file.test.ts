import { describe, expect, it } from 'vitest'
import { parseChannels } from './bots-file'

describe('parseChannels', () => {
  it('读出一行一条渠道，platform 既接受字符串也接受对象', () => {
    const rows = parseChannels(
      JSON.stringify({
        bots: [
          { id: "11111111-1111-4111-8111-111111111111", name: "本地试跑", platform: "local" },
          {
            id: "22222222-2222-4222-8222-222222222222",
            name: "群机器人",
            platform: { kind: "onebot11" },
            ws: "ws://127.0.0.1:3001",
          },
        ],
      }),
    )
    expect(rows).toEqual([
      {
        id: '11111111-1111-4111-8111-111111111111',
        name: '本地试跑',
        platform: 'local',
        ws: '',
      },
      {
        id: '22222222-2222-4222-8222-222222222222',
        name: '群机器人',
        platform: 'onebot11',
        ws: 'ws://127.0.0.1:3001',
      },
    ])
  })

  it('凭据字段一律不出现在结果里', () => {
    const rows = parseChannels(
      JSON.stringify({
        bots: [{ id: "x", name: "n", platform: "onebot11", token: "secret-token", clientSecret: "s3cret" }],
      }),
    )
    expect(rows).toHaveLength(1)
    expect(JSON.stringify(rows)).not.toContain('secret-token')
    expect(JSON.stringify(rows)).not.toContain('s3cret')
  })

  it('坏 JSON / 顶层不是对象 / 没有 bots 数组都返回空数组', () => {
    expect(parseChannels('{ oops')).toEqual([])
    expect(parseChannels('[]')).toEqual([])
    expect(parseChannels(JSON.stringify({ bots: "nope" }))).toEqual([])
    expect(parseChannels(JSON.stringify({}))).toEqual([])
  })

  it('数组里的坏条目跳过，好的保留', () => {
    const rows = parseChannels(JSON.stringify({ bots: [null, 3, { id: "ok" }] }))
    expect(rows).toEqual([{ id: 'ok', name: '', platform: '', ws: '' }])
  })
})
