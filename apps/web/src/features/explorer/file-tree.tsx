/** 文件树：目录可折叠，文件可打开；纯展示 + 回调，不自己读盘。 */

import { useState } from 'react'
import { basename } from '../../lib/paths'
import type { FileNode } from '../../lib/ipc'

interface FileTreeProps {
  nodes: FileNode[]
  activePath: string | null
  onOpenFile: (path: string) => void
}

export function FileTree({ nodes, activePath, onOpenFile }: FileTreeProps) {
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set(['src']))

  function toggle(path: string): void {
    setExpanded((previous) => {
      const next = new Set(previous)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })
  }

  return (
    <ul className="tree" role="tree" aria-label="项目文件">
      {nodes.map((node) => (
        <TreeItem
          key={node.path}
          node={node}
          depth={0}
          expanded={expanded}
          activePath={activePath}
          onToggle={toggle}
          onOpenFile={onOpenFile}
        />
      ))}
    </ul>
  )
}

interface TreeItemProps {
  node: FileNode
  depth: number
  expanded: ReadonlySet<string>
  activePath: string | null
  onToggle: (path: string) => void
  onOpenFile: (path: string) => void
}

function TreeItem({ node, depth, expanded, activePath, onToggle, onOpenFile }: TreeItemProps) {
  const isDir = node.kind === 'dir'
  const isOpen = expanded.has(node.path)
  const isActive = !isDir && node.path === activePath

  return (
    <li className="tree__item" role="treeitem" aria-selected={isActive} aria-expanded={isDir ? isOpen : undefined}>
      <button
        type="button"
        className={`tree__row${isActive ? ' tree__row--active' : ''}`}
        style={{ paddingLeft: `${(depth + 1) * 12}px` }}
        onClick={() => (isDir ? onToggle(node.path) : onOpenFile(node.path))}
        aria-label={`${isDir ? (isOpen ? '折叠目录' : '展开目录') : '打开文件'} ${node.path}`}
      >
        <span className="tree__caret" aria-hidden="true">
          {isDir ? (isOpen ? '▾' : '▸') : ''}
        </span>
        <span className="tree__name">{isDir ? node.name : basename(node.path)}</span>
      </button>
      {isDir && isOpen && node.children ? (
        <ul role="group">
          {node.children.map((child) => (
            <TreeItem
              key={child.path}
              node={child}
              depth={depth + 1}
              expanded={expanded}
              activePath={activePath}
              onToggle={onToggle}
              onOpenFile={onOpenFile}
            />
          ))}
        </ul>
      ) : null}
    </li>
  )
}