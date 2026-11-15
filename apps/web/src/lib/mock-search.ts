/**
 * mock 后端的搜索实现（`ipc-mock.ts` 的 `search.start` 用它合成 `file.search`）。
 *
 * 真语义在 `crates/hbot-workspace/src/search.rs`：`ignore::overrides` 的 glob（`*.ts`
 * 只搜 TS、`!vendor/**` 排除）、大小写**恒不敏感**（核心把 `case_sensitive` 写死为
 * `false`）、命中按 `SearchHit{path, line_number, line}` 返回 —— 所以这里 `column` 恒 1、
 * `text` 是整行原文，与 `crates/hbot-rpc/src/app/commands/files.rs` 的映射一致。
 *
 * 这是**近似**，刻意保留：真机的 glob 还认 `[abc]` 之类，mock 项目里用不到；
 * 非法正则照核心一样抛错（错误会显示在搜索面板的错误行里）。
 */

import type { SearchMatch } from '../generated'
import { basename } from './paths'

/** 命中上限：真机是 `DEFAULT_MAX_RESULTS`（1000），mock 项目小，够用即可。 */
const MAX_SEARCH_MATCHES = 200

/** glob → 正则（`**` 跨目录，`*` / `?` 不跨）。 */
function globToRegExp(glob: string): RegExp {
  let source = ''
  for (let index = 0; index < glob.length; index += 1) {
    const char = glob[index] ?? ''
    if (char === '*') {
      if (glob[index + 1] === '*') {
        if (glob[index + 2] === '/') {
          source += '(?:.*/)?'
          index += 2
        } else {
          source += '.*'
          index += 1
        }
      } else {
        source += '[^/]*'
      }
      continue
    }
    if (char === '?') {
      source += '[^/]'
      continue
    }
    source += char.replace(/[.+^${}()|[\]\\]/g, '\\$&')
  }
  return new RegExp(`^${source}$`)
}

/** 编译一条 glob；不含 `/` 的模式只比对文件名（`*.ts` 这种最常见的写法）。 */
function compileGlob(glob: string): (path: string) => boolean {
  const pattern = glob.startsWith('/') ? glob.slice(1) : glob
  const againstBasename = !pattern.includes('/')
  const regExp = globToRegExp(pattern)
  return (path) => regExp.test(againstBasename ? basename(path) : path)
}

/** 按行扫一个路径 → 内容表，保持 map 的插入顺序（= 真机的遍历顺序）。 */
export function searchMatches(
  files: ReadonlyMap<string, string>,
  query: string,
  regex: boolean,
  globs: readonly string[],
): SearchMatch[] {
  const regExp = regex ? new RegExp(query, 'i') : null
  const needle = query.toLowerCase()
  const includes = globs.filter((glob) => !glob.startsWith('!')).map(compileGlob)
  const excludes = globs
    .filter((glob) => glob.startsWith('!'))
    .map((glob) => compileGlob(glob.slice(1)))

  const matches: SearchMatch[] = []
  for (const [path, content] of files) {
    if (includes.length > 0 && !includes.some((match) => match(path))) continue
    if (excludes.some((match) => match(path))) continue

    const lines = content.split('\n')
    for (let index = 0; index < lines.length; index += 1) {
      const line = lines[index] ?? ''
      const hit = regExp === null ? line.toLowerCase().includes(needle) : regExp.test(line)
      if (!hit) continue
      if (matches.length >= MAX_SEARCH_MATCHES) return matches
      matches.push({ path, line: index + 1, column: 1, text: line })
    }
  }

  return matches
}