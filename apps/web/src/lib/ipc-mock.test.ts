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