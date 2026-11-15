/** 编辑区：标签栏 + 懒加载的 Monaco + 空状态。 */

import { lazy, Suspense } from 'react'
import { useWorkspaceStore } from '../../stores/workspace-store'
import { EditorTabs } from './editor-tabs'

// Monaco 是首屏最重的依赖：必须懒加载（AGENTS.md §6）。
const EditorSurface = lazy(() => import('./editor-surface'))

export function EditorArea() {
  const tabCount = useWorkspaceStore((state) => state.tabs.length)

  return (
    <section className="editor-area" aria-label="编辑器">
      <EditorTabs />
      <div className="editor-area__body">
        {tabCount === 0 ? (
          <div className="welcome" role="status">
            <h2 className="welcome__title">HBot IDE</h2>
            <p className="welcome__hint">从左侧资源管理器打开文件，或者：</p>
            <ul className="welcome__keys">
              <li>
                <kbd>Ctrl</kbd> + <kbd>P</kbd> 快速打开文件
              </li>
              <li>
                <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>P</kbd> 命令面板
              </li>
              <li>
                <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>A</kbd> 显示 / 隐藏 Agent 栏
              </li>
            </ul>
            <p className="welcome__note">
              M0 壳：文件读写与 Agent 回复来自 ipc mock，真实实现由 Rust 侧接管。
            </p>
          </div>
        ) : (
          <Suspense
            fallback={
              <p className="editor-loading" role="status">
                正在加载编辑器…
              </p>
            }
          >
            <EditorSurface />
          </Suspense>
        )}
      </div>
    </section>
  )
}