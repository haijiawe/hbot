/**
 * 命令面板 / 快速打开：共用一层 UI，两种模式。
 *
 * 命令表从 `lib/keybindings.ts` 取键位显示，动作只调用 store 或 `dispatch()`，
 * 不在这里直接碰 DOM 或磁盘。
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { formatKeybinding, keybindingById } from '../../lib/keybindings'
import { flattenFiles, fuzzyMatch } from '../../lib/paths'
import { useRunStore } from '../../stores/run-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

export type PaletteMode = 'commands' | 'files'

interface PaletteItem {
  id: string
  title: string
  hint?: string
  keys?: string
  disabled?: boolean
  /** 执行后不关闭面板（例如从命令切到快速打开）。 */
  keepOpen?: boolean
  run: () => void
}

interface CommandPaletteProps {
  mode: PaletteMode
  onClose: () => void
  onNotice: (message: string) => void
}

export function CommandPalette({ mode: initialMode, onClose, onNotice }: CommandPaletteProps) {
  const [mode, setMode] = useState<PaletteMode>(initialMode)
  const [query, setQuery] = useState('')
  const [index, setIndex] = useState(0)
  const inputRef = useRef<HTMLInputElement | null>(null)

  const files = useWorkspaceStore((state) => state.files)
  const tabs = useWorkspaceStore((state) => state.tabs)
  const agentCollapsed = useWorkspaceStore((state) => state.layout.agentCollapsed)
  const bottomPanelCollapsed = useWorkspaceStore((state) => state.layout.bottomPanelCollapsed)
  const openFile = useWorkspaceStore((state) => state.openFile)
  const saveActive = useWorkspaceStore((state) => state.saveActive)
  const saveAll = useWorkspaceStore((state) => state.saveAll)
  const toggleAgent = useWorkspaceStore((state) => state.toggleAgent)
  const toggleBottomPanel = useWorkspaceStore((state) => state.toggleBottomPanel)
  const setBottomPanelTab = useWorkspaceStore((state) => state.setBottomPanelTab)
  const setSidebarView = useWorkspaceStore((state) => state.setSidebarView)
  const start = useRunStore((state) => state.start)
  const stop = useRunStore((state) => state.stop)

  useEffect(() => {
    inputRef.current?.focus()
  }, [])

  const commands = useMemo<PaletteItem[]>(() => {
    const keys = (id: Parameters<typeof keybindingById>[0]): string =>
      formatKeybinding(keybindingById(id))

    return [
      {
        id: 'file.quick-open',
        title: '快速打开文件…',
        keys: keys('quick-open'),
        keepOpen: true,
        run: () => setMode('files'),
      },
      {
        id: 'file.save',
        title: '保存当前文件',
        keys: keys('save'),
        run: () => void saveActive(),
      },
      {
        id: 'file.save-all',
        title: '全部保存',
        keys: keys('save-all'),
        run: () => void saveAll(),
      },
      {
        id: 'run.start',
        title: '启动 bot',
        keys: keys('run-bot'),
        run: () => void start(),
      },
      {
        id: 'run.stop',
        title: '停止 bot',
        keys: keys('stop-bot'),
        run: () => void stop(),
      },
      {
        id: 'view.agent',
        title: agentCollapsed ? '显示 Agent 栏' : '隐藏 Agent 栏',
        keys: keys('toggle-agent'),
        run: () => toggleAgent(),
      },
      {
        id: 'view.bottom-panel',
        title: bottomPanelCollapsed ? '显示底部面板' : '隐藏底部面板',
        keys: keys('toggle-bottom-panel'),
        run: () => toggleBottomPanel(),
      },
      {
        id: 'view.problems',
        title: '打开问题面板',
        keys: keys('problems-panel'),
        run: () => setBottomPanelTab('problems'),
      },
      {
        id: 'view.settings',
        title: '打开设置',
        run: () => setSidebarView('settings'),
      },
      {
        id: 'search.global',
        title: '全局搜索',
        keys: keys('global-search'),
        hint: 'M3 提供',
        disabled: true,
        run: () => onNotice('全局搜索在 M3 提供'),
      },
    ]
  }, [
    agentCollapsed,
    bottomPanelCollapsed,
    onNotice,
    saveActive,
    saveAll,
    setBottomPanelTab,
    setSidebarView,
    start,
    stop,
    toggleAgent,
    toggleBottomPanel,
  ])

  const fileItems = useMemo<PaletteItem[]>(() => {
    const openPaths = new Set(tabs.map((tab) => tab.path))
    return flattenFiles(files).map((path) => ({
      id: `file:${path}`,
      title: path,
      hint: openPaths.has(path) ? '已打开' : undefined,
      run: () => void openFile(path),
    }))
  }, [files, tabs, openFile])

  const items = useMemo(() => {
    const source = mode === 'commands' ? commands : fileItems
    return source.filter((item) => fuzzyMatch(item.title, query))
  }, [commands, fileItems, mode, query])

  useEffect(() => {
    setIndex(0)
  }, [mode, query])

  function execute(item: PaletteItem | undefined): void {
    if (!item || item.disabled) return
    item.run()
    if (!item.keepOpen) onClose()
  }

  const activeId = items.length > 0 ? `palette-option-${index}` : undefined
  const title = mode === 'commands' ? '命令面板' : '快速打开文件'

  return (
    <div className="palette-backdrop" onMouseDown={onClose}>
      <div
        className="palette"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="palette__search">
          <span className="palette__prompt" aria-hidden="true">
            {mode === 'commands' ? '>' : '⌕'}
          </span>
          <input
            ref={inputRef}
            className="palette__input"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'ArrowDown') {
                event.preventDefault()
                setIndex((current) => (items.length === 0 ? 0 : (current + 1) % items.length))
              } else if (event.key === 'ArrowUp') {
                event.preventDefault()
                setIndex((current) =>
                  items.length === 0 ? 0 : (current - 1 + items.length) % items.length,
                )
              } else if (event.key === 'Enter') {
                event.preventDefault()
                execute(items[index])
              } else if (event.key === 'Escape') {
                event.preventDefault()
                if (mode === 'files' && initialMode === 'commands') setMode('commands')
                else onClose()
              } else if (event.key === 'Backspace' && query.length === 0 && mode === 'files') {
                event.preventDefault()
                setMode('commands')
              }
            }}
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-autocomplete="list"
            aria-activedescendant={activeId}
            aria-label={mode === 'commands' ? '输入命令' : '输入文件名'}
            placeholder={mode === 'commands' ? '输入命令…' : '按路径模糊搜索…'}
            autoComplete="off"
            spellCheck={false}
          />
        </div>

        {items.length === 0 ? (
          <p className="panel-empty" role="status">
            没有匹配项。
          </p>
        ) : (
          <ul className="palette__list" id="palette-list" role="listbox" aria-label="结果">
            {items.map((item, itemIndex) => (
              <li
                key={item.id}
                id={`palette-option-${itemIndex}`}
                role="option"
                aria-selected={itemIndex === index}
                aria-disabled={item.disabled}
                className={`palette__item${itemIndex === index ? ' palette__item--active' : ''}${
                  item.disabled ? ' palette__item--disabled' : ''
                }`}
                onMouseEnter={() => setIndex(itemIndex)}
                onClick={() => execute(item)}
              >
                <span className="palette__title">{item.title}</span>
                {item.hint ? <span className="palette__hint">{item.hint}</span> : null}
                {item.keys ? <kbd className="palette__keys">{item.keys}</kbd> : null}
              </li>
            ))}
          </ul>
        )}

        <footer className="palette__footer">
          <span>↑↓ 选择</span>
          <span>Enter 执行</span>
          <span>Esc {mode === 'files' && initialMode === 'commands' ? '返回命令' : '关闭'}</span>
        </footer>
      </div>
    </div>
  )
}