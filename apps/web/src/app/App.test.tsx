// @vitest-environment jsdom
/**
 * 外壳在「还没有打开项目」时的分支（`app/App.tsx`）。
 *
 * 这条是 M1 的可见症状修复：真桌面壳启动时 `projectRoot` 是 `null`，以前这里照常
 * 挂编辑区 —— 用户看到的是一块空白画布，不知道下一步该做什么。现在必须是一句
 * 能照着做的提示，而且**编辑区不许挂载**（它内部会懒加载 monaco）。
 */

import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useWorkspaceStore } from '../stores/workspace-store'
import { App } from './App'

// 只挡掉持久化那两跳：jsdom 里没有 IndexedDB，而这条测试测的是渲染分支，不是 journal。
vi.mock('../lib/session-persistence', () => ({
  startSessionPersistence: () => () => {},
  restoreSessionIfSameProject: async () => 'empty',
}))

beforeEach(() => {
  useWorkspaceStore.setState({
    projectRoot: null,
    projectDialog: null,
    files: [],
    treeLoaded: false,
    tabs: [],
    contents: {},
    saved: {},
    cursors: {},
    activePath: null,
    error: null,
    diagnostics: [],
  })
})

afterEach(cleanup)

describe('App：还没有打开项目', () => {
  it('主编辑区显示可照做的空状态提示，而不是空白画布或编辑器', () => {
    render(<App />)

    const empty = screen.getByLabelText('还没有打开项目')
    expect(empty.getAttribute('role')).toBe('status')
    // 提示里必须给出可执行的路（命令面板 + 打开项目），否则等于换了种方式说"没东西"。
    expect(empty.textContent).toContain('Shift')
    expect(empty.textContent).toContain('打开项目…')
    // 关键断言：不是一块空白画布 —— 编辑区组件没有挂载。
    expect(document.querySelector('.editor-area')).toBeNull()
  })
})