/**
 * 搜索输入面板：关键词 / 正则开关 / glob 过滤 + 搜索与取消按钮。
 *
 * 命令经 `search-store.start()` 走 `dispatch('search.start')`，拿不到结果（`data: null`）；
 * 命中由 `file.search` 事件推回 store，面板只读 store，不直接碰 IPC 传输层。
 */

import { useEffect, useRef } from 'react'
import { parseGlobs, useSearchStore } from '../../stores/search-store'

export function SearchForm() {
  const query = useSearchStore((state) => state.query)
  const regex = useSearchStore((state) => state.regex)
  const globs = useSearchStore((state) => state.globs)
  const running = useSearchStore((state) => state.running)
  const focusSeq = useSearchStore((state) => state.focusSeq)
  const setQuery = useSearchStore((state) => state.setQuery)
  const setRegex = useSearchStore((state) => state.setRegex)
  const setGlobs = useSearchStore((state) => state.setGlobs)
  const start = useSearchStore((state) => state.start)
  const cancel = useSearchStore((state) => state.cancel)
  const inputRef = useRef<HTMLInputElement | null>(null)

  // 快捷键 / 活动栏 / 命令面板打开面板时都要能直接打字（`focusSeq` 每次 +1）。
  useEffect(() => {
    if (focusSeq === 0) return
    const input = inputRef.current
    if (!input) return
    input.focus()
    input.select()
  }, [focusSeq])

  return (
    <form
      className="search-form"
      role="search"
      onSubmit={(event) => {
        event.preventDefault()
        void start({ query, regex, globs: parseGlobs(globs) })
      }}
    >
      <label className="search-form__field search-form__field--grow">
        <span className="search-form__label">搜索</span>
        <input
          ref={inputRef}
          className="search-form__input"
          type="text"
          value={query}
          placeholder="输入关键词，回车搜索"
          aria-label="搜索内容"
          spellCheck={false}
          autoComplete="off"
          onChange={(event) => setQuery(event.target.value)}
        />
      </label>

      <label className="search-form__check">
        <input
          type="checkbox"
          checked={regex}
          aria-label="按正则表达式搜索"
          onChange={(event) => setRegex(event.target.checked)}
        />
        <span>正则</span>
      </label>

      <label className="search-form__field search-form__field--globs">
        <span className="search-form__label">文件</span>
        <input
          className="search-form__input"
          type="text"
          value={globs}
          placeholder="*.ts, !vendor/**"
          aria-label="文件过滤（glob，逗号分隔）"
          spellCheck={false}
          autoComplete="off"
          onChange={(event) => setGlobs(event.target.value)}
        />
      </label>

      <div className="search-form__actions">
        <button type="submit" className="button button--primary">
          搜索
        </button>
        <button type="button" className="button" disabled={!running} onClick={() => void cancel()}>
          取消
        </button>
      </div>
    </form>
  )
}