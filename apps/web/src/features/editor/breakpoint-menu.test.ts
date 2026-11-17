/**
 * 行槽位右键菜单的菜单项表：按"这一行有什么"动态变化。
 *
 * 这是 gutter 右键菜单唯一的可测面（真菜单挂在 Monaco 实例上，jsdom 里没有 glyph
 * margin），所以把判定抽成了纯函数。四条状态各钉一遍：只有断点、只有日志点、两样
 * 都有、以及"两样都没有时不该开菜单"。
 */

import { describe, expect, it } from 'vitest'
import { gutterMenuItems } from './breakpoint-menu'

const actions = (state: Parameters<typeof gutterMenuItems>[0]) =>
  gutterMenuItems(state).map((item) => item.action)

describe('断点/日志点的右键菜单项', () => {
  it('只有断点：编辑条件 / 清除条件 / 删除断点，再加一个"添加日志点"', () => {
    expect(actions({ hasBreakpoint: true, condition: null, logpoint: null })).toEqual([
      'edit-condition',
      'clear-condition',
      'remove-breakpoint',
      'add-logpoint',
    ])
  })

  it('只有日志点：出现编辑/删除日志点，没有断点项（这行不会停）', () => {
    expect(actions({ hasBreakpoint: false, condition: null, logpoint: 'count={count}' })).toEqual([
      'edit-logpoint',
      'remove-logpoint',
    ])
  })

  it('两样都有：两组都在（日志点组换成编辑/删除）', () => {
    expect(actions({ hasBreakpoint: true, condition: 'count > 3', logpoint: 'count={count}' })).toEqual([
      'edit-condition',
      'clear-condition',
      'remove-breakpoint',
      'edit-logpoint',
      'remove-logpoint',
    ])
  })

  it('没有条件时"清除条件"禁用（禁用不隐藏：让用户知道这行有条件这个概念）', () => {
    const items = gutterMenuItems({ hasBreakpoint: true, condition: null, logpoint: null })
    expect(items.find((item) => item.action === 'clear-condition')?.disabled).toBe(true)
    expect(
      gutterMenuItems({ hasBreakpoint: true, condition: 'x > 1', logpoint: null }).find(
        (item) => item.action === 'clear-condition',
      )?.disabled,
    ).toBe(false)
  })
})