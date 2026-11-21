import { describe, expect, it } from 'vitest'
import { limitsSummary, parseManifest } from './bot-manifest'

const CHAT = JSON.stringify({
  name: "chat-bot",
  entry: "src/main.ts",
  platform: { kind: "local" },
  capabilities: { state: { kv: true, history: true }, ai: true },
  limits: { handlerMs: 2000, heapMb: 32, quotaMb: 0 },
  permissions: { http: ["api.example.com"] },
})

describe('parseManifest', () => {
  it('把一份清单读成视图（能力行顺序固定）', () => {
    const parsed = parseManifest(CHAT)
    expect(parsed.ok).toBe(true)
    if (!parsed.ok) return
    const view = parsed.view
    expect(view.name).toBe("chat-bot")
    expect(view.entry).toBe("src/main.ts")
    expect(view.platform).toBe("local")
    // 12 行固定顺序：状态子能力在前，入站/设备在最后。
    expect(view.capabilities.map((row) => row.id)).toEqual([
      'state.kv',
      'state.history',
      'state.sql',
      'state.files',
      'state.session',
      'ai',
      'timer',
      'jobs',
      'send',
      'notify',
      'webhook',
      'device',
    ])
    expect(view.capabilities.filter((row) => row.declared).map((row) => row.id)).toEqual(['state.kv', 'state.history', 'ai'])
    expect(view.httpHosts).toEqual(["api.example.com"])
  })

  it('计划类能力把条数写进 detail', () => {
    const parsed = parseManifest(
      JSON.stringify({
        name: "p",
        capabilities: {
          timer: [{ name: "a" }, { name: "b" }],
          jobs: [{ name: "sync" }],
          webhook: [{ path: "/x" }],
          notify: { conversation: "local:default", kind: "private" },
          device: { sub: ["screenshot", "ocr"] },
        },
      }),
    )
    expect(parsed.ok).toBe(true)
    if (!parsed.ok) return
    const byId = new Map(parsed.view.capabilities.map((row) => [row.id, row]))
    expect(byId.get('timer')?.detail).toBe("2 条计划")
    expect(byId.get('jobs')?.detail).toBe("1 种类型")
    expect(byId.get('webhook')?.detail).toBe("1 条路由")
    expect(byId.get('notify')?.detail).toBe("local:default · private")
    expect(byId.get('device')?.detail).toBe("screenshot / ocr")
  })

  it('state.files 写成 true 或对象都算声明', () => {
    for (const files of [true, { quotaMb: 8 }]) {
      const parsed = parseManifest(JSON.stringify({ name: "p", capabilities: { state: { files } } }))
      expect(parsed.ok).toBe(true)
      if (!parsed.ok) return
      expect(parsed.view.capabilities.find((row) => row.id === 'state.files')?.declared).toBe(true)
    }
  })

  it('坏 JSON 与缺 name 都只给一句人话，不抛', () => {
    const broken = parseManifest('{ oops')
    expect(broken.ok).toBe(false)
    if (!broken.ok) expect(broken.reason).toContain('不是合法 JSON')
    const nameless = parseManifest('{"entry":"src/main.ts"}')
    expect(nameless.ok).toBe(false)
    if (!nameless.ok) expect(nameless.reason).toContain('缺少 name')
  })

  it('HTTP 源去重且保留原顺序', () => {
    const parsed = parseManifest(
      JSON.stringify({ name: "p", permissions: { http: ["b", "a", "b", 3] } }),
    )
    expect(parsed.ok).toBe(true)
    if (!parsed.ok) return
    expect(parsed.view.httpHosts).toEqual(['b', 'a'])
  })

  it('limitsSummary 只列写了的项，配额 0 说成不限', () => {
    const parsed = parseManifest(CHAT)
    expect(parsed.ok).toBe(true)
    if (!parsed.ok) return
    expect(limitsSummary(parsed.view)).toBe('handler 2000 ms · 堆 32 MB · 文件配额不限')
    const empty = parseManifest(JSON.stringify({ name: "p" }))
    expect(empty.ok).toBe(true)
    if (!empty.ok) return
    expect(limitsSummary(empty.view)).toBe('')
  })
})
