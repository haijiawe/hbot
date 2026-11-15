// IPC 层不许出现 `any`：契约边界只有一份（`src/generated/`），
// 手写适配层一旦用 any 兜住，类型就再也说明不了"wire 与 UI 的差异在哪"。
//
// 为什么不用 TypeScript 编译器 API：本仓 `typescript@7` 是原生端口，包入口只导出
// 版本号（`lib/version.cjs`），`ts.createSourceFile` 已不存在。所以这里自己做一遍
// 扫描：先抹掉注释与字符串/模板内容（保留换行以便报行号），再找 `any` 这个词，
// 因此解释性注释里写 any 不算债，`as any` / `: any` / `<any>` 都算。
//
// 用法：node scripts/check-ipc-no-any.mjs

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, '..')

/** 受检文件（相对 apps/web）：IPC 的全部入口与适配层。 */
const FILES = ['src/lib/ipc.ts', 'src/lib/ipc-tauri.ts']

/**
 * 把注释、字符串与模板字面量的内容抹成空格（换行保留），这样后续的整词搜索
 * 只命中真正的代码 token。
 *
 * 不需要完整的 JS 词法分析：这里只关心 `//`、`/*` 有没有被字符串里的内容骗到，
 * 以及字符串里的 "any" 不算数。`/` 后面不是 `/` 或 `*` 时按普通字符处理。
 *
 * @param {string} text
 * @returns {string}
 */
function blankOutLiterals(text) {
  const out = []
  let index = 0
  let mode = 'code' // code | line-comment | block-comment | string | template
  let quote = ''

  while (index < text.length) {
    const char = text[index]
    const next = text[index + 1]

    if (mode === 'code') {
      if (char === '/' && next === '/') {
        mode = 'line-comment'
        out.push(' ', ' ')
        index += 2
        continue
      }
      if (char === '/' && next === '*') {
        mode = 'block-comment'
        out.push(' ', ' ')
        index += 2
        continue
      }
      if (char === "'" || char === '"') {
        mode = 'string'
        quote = char
        out.push(' ')
        index += 1
        continue
      }
      if (char === '`') {
        mode = 'template'
        out.push(' ')
        index += 1
        continue
      }
      out.push(char)
      index += 1
      continue
    }

    if (mode === 'line-comment') {
      if (char === '\n') {
        mode = 'code'
        out.push('\n')
      } else {
        out.push(' ')
      }
      index += 1
      continue
    }

    if (mode === 'block-comment') {
      if (char === '*' && next === '/') {
        mode = 'code'
        out.push(' ', ' ')
        index += 2
        continue
      }
      out.push(char === '\n' ? '\n' : ' ')
      index += 1
      continue
    }

    // 字符串 / 模板：内容抹空格，转义跳过两个字符。
    if (char === '\\') {
      out.push(' ', ' ')
      index += 2
      continue
    }
    if ((mode === 'string' && char === quote) || (mode === 'template' && char === '`')) {
      mode = 'code'
      out.push(' ')
      index += 1
      continue
    }
    out.push(char === '\n' ? '\n' : ' ')
    index += 1
  }

  return out.join('')
}

/** @type {string[]} */
const hits = []
const ANY_PATTERN = /\bany\b/g

for (const relative of FILES) {
  const file = resolve(root, relative)
  const code = blankOutLiterals(readFileSync(file, 'utf8'))
  for (const match of code.matchAll(ANY_PATTERN)) {
    const line = code.slice(0, match.index).split('\n').length
    hits.push(`${relative}:${line}`)
  }
}

if (hits.length > 0) {
  console.error(`IPC 层出现 \`any\`（共 ${hits.length} 处）：`)
  for (const hit of hits) console.error(`  ${hit}`)
  console.error('改成 unknown + 类型守卫，或补最小接口；生成的 DTO 在 src/generated/。')
  process.exit(1)
}

console.log(`IPC 层无 \`any\`：${FILES.join(', ')}`)