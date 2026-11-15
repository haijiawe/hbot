/** 编辑器标签栏：多文件切换、脏标记、关闭。 */

import { basename } from '../../lib/paths'
import { useWorkspaceStore } from '../../stores/workspace-store'

export function EditorTabs() {
  const tabs = useWorkspaceStore((state) => state.tabs)
  const activePath = useWorkspaceStore((state) => state.activePath)
  const activateTab = useWorkspaceStore((state) => state.activateTab)
  const closeTab = useWorkspaceStore((state) => state.closeTab)

  if (tabs.length === 0) return null

  return (
    <div className="tabs" role="tablist" aria-label="打开的编辑器">
      {tabs.map((tab) => {
        const active = tab.path === activePath
        const name = basename(tab.path)
        return (
          <div key={tab.path} className={`tab${active ? ' tab--active' : ''}`} role="presentation">
            <button
              type="button"
              role="tab"
              aria-label={`文件标签 ${tab.path}${tab.dirty ? '（未保存）' : ''}`}
              aria-selected={active}
              className="tab__label"
              title={tab.path}
              onClick={() => activateTab(tab.path)}
            >
              {name}
              {tab.dirty ? (
                <>
                  <span className="tab__dot" aria-hidden="true" />
                  <span className="sr-only">（未保存）</span>
                </>
              ) : null}
            </button>
            <button
              type="button"
              className="tab__close"
              aria-label={`关闭 ${name}`}
              title={`关闭 ${name}`}
              onClick={() => closeTab(tab.path)}
            >
              ×
            </button>
          </div>
        )
      })}
    </div>
  )
}