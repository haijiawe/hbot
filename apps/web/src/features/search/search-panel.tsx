/**
 * 全局搜索面板（底部面板的"搜索"页签）。
 *
 * 三层状态要分清，别混成一句"搜索中"：
 * - `searchId === null && running`：命令已发出，第一条 `file.search` 还没到（真机的 ID 随事件到达）；
 * - `searchId !== null`：至少收到过一条事件；契约里**没有完成事件**，所以 `running` 会保持
 *   true 直到取消 / 新搜索 —— 文案只承诺"会继续追加"；
 * - 其余：还没搜过。
 *
 * 错误（非法正则 / 非法 glob / 核心拒绝）只走这里的一行 `role="alert"`，结果区不显示假数据。
 */

import { useSearchStore } from '../../stores/search-store'
import { SearchForm } from './search-form'
import { SearchResults } from './search-results'

export function SearchPanel() {
  const query = useSearchStore((state) => state.query)
  const running = useSearchStore((state) => state.running)
  const searchId = useSearchStore((state) => state.searchId)
  const totalMatches = useSearchStore((state) => state.totalMatches)
  const error = useSearchStore((state) => state.error)

  const statusText =
    searchId === null
      ? running
        ? '正在搜索…'
        : '输入关键词后按回车开始搜索。'
      : `没有匹配「${query}」。`

  return (
    <section className="search" aria-label="全局搜索">
      <SearchForm />
      {error === null ? null : (
        <p className="inline-error search__error" role="alert">
          {error}
        </p>
      )}
      {totalMatches > 0 ? (
        <SearchResults />
      ) : (
        <p className="panel-empty search__status" role="status">
          {statusText}
        </p>
      )}
    </section>
  )
}