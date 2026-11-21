/**
 * 文档列表的纯函数：从文件树里挑出"能当文档看"的文件。
 *
 * 判据刻意只有扩展名与目录黑名单（不猜语义）：项目根与 `docs/` 下的 Markdown / 纯文本 /
 * JSON 是用户真正会改的东西，`src/` 里的 .ts 属于编辑器的事，这里不抢。
 */

import type { FileNode } from '../lib/ipc'

/** 认作文档的扩展名（小写、含点）。 */
export const DOC_EXTENSIONS: readonly string[] = ['.md', '.markdown', '.txt', '.json']

/** 整棵子树都不看的目录（与文件树同一份口径，避免把依赖树的 README 列进来）。 */
const SKIP_DIRS: readonly string[] = ['node_modules', '.hbot', '.git', 'target', 'dist']

export interface DocEntry {
  /** 相对项目根的路径，直接喂 `fs.read` / `fs.write`。 */
  path: string
  /** 列表里显示的名字（文件名，不含目录）。 */
  name: string
  /** 所在目录（根目录为空串），用来分组显示。 */
  dir: string
}

function isDoc(name: string): boolean {
  const lower = name.toLowerCase()
  return DOC_EXTENSIONS.some((ext) => lower.endsWith(ext))
}

/**
 * 深度优先收集文档，保持文件树顺序（根文件在前、目录内按名字）。
 *
 * `limit` 是给面板的安全阀：项目里有几百个 JSON 时不该把整棵树铺到侧栏。
 */
export function pickDocs(nodes: readonly FileNode[], limit = 200): DocEntry[] {
  const out: DocEntry[] = []
  const walk = (list: readonly FileNode[], dir: string) => {
    for (const node of list) {
      if (out.length >= limit) return
      if (node.kind === 'dir') {
        if (SKIP_DIRS.includes(node.name)) continue
        walk(node.children ?? [], dir.length === 0 ? node.name : dir + '/' + node.name)
        continue
      }
      if (!isDoc(node.name)) continue
      out.push({ path: node.path, name: node.name, dir })
    }
  }
  walk(nodes, '')
  return out
}

/** 一份文档是否可写：JSON 与 md 都是文本，二进制扩展名进不来（见 `DOC_EXTENSIONS`）。 */
export function isEditableDoc(path: string): boolean {
  const name = path.split('/').pop() ?? path
  return isDoc(name)
}
