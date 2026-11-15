import { describe, expect, it } from 'vitest'
import { UNSERIALIZABLE, formatDebugValue } from './debug-format'

describe('formatDebugValue', () => {
  it('字符串输出成带引号的 JSON 字符串', () => {
    expect(formatDebugValue('hi')).toBe('"hi"')
  })

  it('数字 / 布尔 / null 输出 JSON 字面量', () => {
    expect(formatDebugValue(3)).toBe('3')
    expect(formatDebugValue(false)).toBe('false')
    expect(formatDebugValue(null)).toBe('null')
  })

  it('对象与数组输出紧凑 JSON', () => {
    expect(formatDebugValue({ text: 'hi', count: 2 })).toBe('{"text":"hi","count":2}')
    expect(formatDebugValue([1, 2])).toBe('[1,2]')
  })

  it('序列化不了的值统一显示 <unserializable>', () => {
    expect(formatDebugValue(undefined)).toBe(UNSERIALIZABLE)
    expect(formatDebugValue(() => 1)).toBe(UNSERIALIZABLE)
    // 循环引用会让 JSON.stringify 抛，不能让异常冒进渲染。
    const cyclic: Record<string, unknown> = {}
    cyclic.self = cyclic
    expect(formatDebugValue(cyclic)).toBe(UNSERIALIZABLE)
    expect(formatDebugValue(BigInt(1))).toBe(UNSERIALIZABLE)
  })
})