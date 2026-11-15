/**
 * 搜索结果树：按文件分组，文件顺序与组内行序都用后端给的顺序（`store.fileOrder`）。
 *
 * 点击某条结果 → `workspace-store.openAt(path, line)`：打开文件 + 发一个跳行意图，
 * 由 `features/editor/editor-surface.tsx` 消费（那里才有 Monaco）。
 */

import { basename, dirname } from '../../lib/paths'
import { useSearchStore } from '../../stores/search-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

export function SearchResults() {
  const fileOrder = useSearchStore((state) => state.fileOrder)
  const resultsByFile = useSearchStore((state) => state.resultsByFile)
  const totalMatches = useSearchStore((state) => state.totalMatches)
  const running = useSearchStore((state) => state.running)
  const clear = useSearchStore((state) => state.clear)
  const openAt = useWorkspaceStore((state) => state.openAt)

  return (
    <div className="search-results">
      <div className="search-results__header">
        <span className="search-results__count">
          {`${totalMatches} 处匹配 · ${fileOrder.length} 个文件`}
        </span>
        {running ? (
          // 契约里没有"搜索完成"事件，所以只能说"还在追加"，不能假装跑完了。
          <span className="search-results__hint">结果会继续追加</span>
        ) : null}
        <button
          type="button"
          className="icon-button"
          aria-label="清空搜索结果"
          title="清空搜索结果"
          onClick={clear}
        >
          清
        </button>
      </div>

      <ul className="search-results__list">
        {fileOrder.map((path) => {
          const matches = resultsByFile[path] ?? []
          return (
            <li key={path} className="search-file">
              <div className="search-file__path" title={path}>
                <span className="search-file__name">{basename(path)}</span>
                <span className="search-file__dir">{dirname(path)}</span>
                <span className="panel-tab__count" aria-label={`${matches.length} 处匹配`}>
                  {matches.length}
                </span>
              </div>
              <ul className="search-file__matches">
                {matches.map((match, index) => (
                  <li key={`${match.line}:${index}`}>
                    <button
                      type="button"
                      className="search-match"
                      title={`${path}:${match.line}`}
                      onClick={() => void openAt(match.path, match.line)}
                    >
                      <span className="search-match__line">{match.line}</span>
                      <span className="search-match__text">
                        {match.text.trim().length === 0 ? '（空行）' : match.text}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            </li>
          )
        })}
      </ul>
    </div>
  )
}