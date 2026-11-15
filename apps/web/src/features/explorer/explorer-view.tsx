/** Explorer：侧栏的默认视图。 */

import { useWorkspaceStore } from '../../stores/workspace-store'
import { FileTree } from './file-tree'

export function ExplorerView() {
  const files = useWorkspaceStore((state) => state.files)
  const treeLoaded = useWorkspaceStore((state) => state.treeLoaded)
  const activePath = useWorkspaceStore((state) => state.activePath)
  const rootName = useWorkspaceStore((state) => state.rootName)
  const loadTree = useWorkspaceStore((state) => state.loadTree)
  const openFile = useWorkspaceStore((state) => state.openFile)

  return (
    <section className="explorer" aria-label="资源管理器">
      <header className="panel-header">
        <h2 className="panel-header__title" title={rootName}>
          {rootName}
        </h2>
        <button
          type="button"
          className="icon-button"
          onClick={() => void loadTree()}
          aria-label="刷新文件树"
          title="刷新文件树"
        >
          ↻
        </button>
      </header>

      {!treeLoaded ? (
        <p className="panel-empty" role="status">
          正在读取项目文件…
        </p>
      ) : files.length === 0 ? (
        <p className="panel-empty">没有可显示的文件。检查是否已打开项目。</p>
      ) : (
        <div className="panel-scroll">
          <FileTree nodes={files} activePath={activePath} onOpenFile={(path) => void openFile(path)} />
        </div>
      )}
    </section>
  )
}