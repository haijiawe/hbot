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
          // 空状态照 Mino 的 topbar + content 形态：eyebrow 小标题、大标题、说明、卡片。
          <div className="wb-page wb-page--center" role="status">
            <div className="wb-hero">
              <span className="wb-eyebrow">Editor</span>
              <h2 className="wb-title">还没有打开文件</h2>
              <p className="wb-sub">从左侧资源管理器选一个文件，或者用下面的快捷键。</p>
              <div className="wb-card">
                <div className="wb-card__title">快捷键</div>
                <ul className="wb-keys">
                  <li>
                    <span className="wb-kbd">Ctrl P</span>快速打开文件
                  </li>
                  <li>
                    <span className="wb-kbd">Ctrl ⇧ P</span>命令面板
                  </li>
                  <li>
                    <span className="wb-kbd">Ctrl ⇧ A</span>显示 / 隐藏 Agent 栏
                  </li>
                </ul>
              </div>
              <p className="wb-note">
                打开文件后会在这里看到代码；保存即写盘，热重载由核心负责。
              </p>
            </div>
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
