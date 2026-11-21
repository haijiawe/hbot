import { describe, expect, it } from 'vitest'
import type { FileNode } from './ipc'
import { DOC_EXTENSIONS, isEditableDoc, pickDocs } from './doc-list'

function file(path: string): FileNode {
  const name = path.split('/').pop() ?? path
  return { name, path, kind: 'file' }
}

function dir(path: string, children: FileNode[]): FileNode {
  const name = path.split('/').pop() ?? path
  return { name, path, kind: 'dir', children }
}

describe('pickDocs', () => {
  it('只收文档扩展名，跳过源码', () => {
    const docs = pickDocs([file('README.md'), file('hbot.json'), file('src/main.ts')])
    expect(docs.map((doc) => doc.path)).toEqual(['README.md', 'hbot.json'])
  })

  it('递归进目录并记录所在目录', () => {
    const docs = pickDocs([dir('docs', [file('docs/07-runtime.md')])])
    expect(docs).toEqual([{ path: 'docs/07-runtime.md', name: '07-runtime.md', dir: 'docs' }])
  })

  it('根目录的 dir 是空串', () => {
    expect(pickDocs([file('AGENTS.md')])[0]?.dir).toBe('')
  })

  it('跳过 node_modules / .hbot / target 等依赖目录', () => {
    const docs = pickDocs([
      dir('node_modules', [file('node_modules/pkg/README.md')]),
      dir('.hbot', [file('.hbot/x.md')]),
      dir('target', [file('target/y.md')]),
    ])
    expect(docs).toEqual([])
  })

  it('limit 截断（安全阀）', () => {
    const nodes = Array.from({ length: 5 }, (_, index) => file('doc' + index + '.md'))
    expect(pickDocs(nodes, 3)).toHaveLength(3)
  })

  it('大小写不敏感', () => {
    expect(pickDocs([file('NOTES.MD')])).toHaveLength(1)
  })
})

describe('isEditableDoc', () => {
  it('文档扩展名可写，源码不可写', () => {
    expect(isEditableDoc('docs/a.md')).toBe(true)
    expect(isEditableDoc('hbot.json')).toBe(true)
    expect(isEditableDoc('src/main.ts')).toBe(false)
  })
})

describe('DOC_EXTENSIONS', () => {
  it('全是带点的小写扩展名', () => {
    for (const ext of DOC_EXTENSIONS) {
      expect(ext.startsWith('.')).toBe(true)
      expect(ext).toBe(ext.toLowerCase())
    }
  })
})
