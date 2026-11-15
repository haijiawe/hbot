/** 路径工具：项目内路径统一用 `/`，不依赖 node:path（渲染进程没有 Node）。 */

export function basename(path: string): string {
  const index = path.lastIndexOf('/')
  return index === -1 ? path : path.slice(index + 1)
}

export function dirname(path: string): string {
  const index = path.lastIndexOf('/')
  return index === -1 ? '' : path.slice(0, index)
}

export function extensionOf(path: string): string {
  const name = basename(path)
  const index = name.lastIndexOf('.')
  return index <= 0 ? '' : name.slice(index + 1).toLowerCase()
}

/** Monaco 的语言 id；未知扩展名回退 plaintext（不猜）。 */
export function languageForPath(path: string): string {
  switch (extensionOf(path)) {
    case 'ts':
      return 'typescript'
    case 'tsx':
      return 'typescript'
    case 'js':
      return 'javascript'
    case 'jsx':
      return 'javascript'
    case 'json':
      return 'json'
    case 'md':
      return 'markdown'
    case 'css':
      return 'css'
    case 'html':
      return 'html'
    case 'yaml':
    case 'yml':
      return 'yaml'
    default:
      return 'plaintext'
  }
}

/** 扁平化文件树，给快速打开用。 */
export function flattenFiles(nodes: readonly FileTreeNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    if (node.kind === 'file') out.push(node.path)
    else if (node.children) flattenFiles(node.children, out)
  }
  return out
}

interface FileTreeNode {
  path: string
  kind: 'file' | 'dir'
  children?: FileTreeNode[]
}

/** 模糊匹配：`q` 的字符按顺序出现在 `text` 中即命中；大小写不敏感。 */
export function fuzzyMatch(text: string, query: string): boolean {
  if (query.length === 0) return true
  const haystack = text.toLowerCase()
  const needle = query.toLowerCase()
  let cursor = 0
  for (const char of needle) {
    const found = haystack.indexOf(char, cursor)
    if (found === -1) return false
    cursor = found + 1
  }
  return true
}