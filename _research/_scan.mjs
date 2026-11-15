// read-only dependency scan of E:\H Bot 制造\src  (v2: resolves relative imports too)
import fs from 'node:fs'
import path from 'node:path'

const ROOT = 'E:\\H Bot 制造'
const SRC = path.join(ROOT, 'src')
const ALIASES = [
  ['@shared/', 'src/shared/'],
  ['@main/', 'src/main/'],
  ['@renderer/', 'src/renderer/src/'],
  ['@logic/', 'src/shared/nodes/builtin/logic/'],
  ['@actions/', 'src/shared/nodes/builtin/actions/'],
  ['@triggers/', 'src/shared/nodes/builtin/triggers/'],
  ['@data/', 'src/shared/nodes/builtin/data/'],
  ['@ai/', 'src/shared/nodes/builtin/ai/'],
  ['@interface/', 'src/shared/nodes/builtin/interface/'],
]
const NODE_BUILTINS = new Set(['fs', 'path', 'os', 'crypto', 'child_process', 'net', 'http', 'https', 'tls', 'url', 'util', 'stream', 'events', 'zlib', 'worker_threads', 'vm', 'buffer', 'process', 'readline', 'dns', 'tty', 'assert', 'querystring', 'string_decoder', 'timers', 'perf_hooks', 'async_hooks', 'module', 'v8', 'inspector', 'cluster'])

function collect(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, e.name)
    if (e.isDirectory()) {
      if (e.name === 'node_modules' || e.name === '__tests__') continue
      collect(full, out)
    } else if (/\.(ts|tsx)$/.test(e.name) && !/\.d\.ts$/.test(e.name)) out.push(full)
  }
  return out
}
const files = collect(SRC)
const rel = f => path.relative(ROOT, f).replace(/\\/g, '/')
const boundaryOf = r => r.startsWith('src/main') ? 'main' : r.startsWith('src/renderer') ? 'renderer' : r.startsWith('src/shared') ? 'shared' : 'preload'
const byBoundary = {}
for (const f of files) { const b = boundaryOf(rel(f)); byBoundary[b] = (byBoundary[b] ?? 0) + 1 }
console.log('FILE_COUNTS ' + JSON.stringify(byBoundary) + ' total=' + files.length)

const LINE_IMPORT_RE = /^[ \t]*(?:import|export)\s+(?:type\s+)?(?:[\s\S]*?\s+from\s+)?['"]([^'"]+)['"]/gm
const DYN_RE = /\b(?:import|require)\s*\(\s*['"]([^'"]+)['"]/g

function resolveSpec(fromFile, spec) {
  for (const [alias, real] of ALIASES) {
    if (spec.startsWith(alias)) return { abs: path.join(ROOT, real, spec.slice(alias.length)), alias: true }
  }
  if (spec.startsWith('.')) return { abs: path.resolve(path.dirname(fromFile), spec), alias: false }
  return null
}
function toRel(abs) {
  const t = path.relative(ROOT, abs).replace(/\\/g, '/')
  if (!t.startsWith('src/')) return null
  return t
}

const violations = { sharedToMain: [], sharedToRenderer: [], rendererToMain: [], rendererNode: [], mainToRenderer: [], preloadToMain: [], preloadToRenderer: [] }
const rendererNodeModules = new Map()
const edgeCounts = new Map()

for (const f of files) {
  const text = fs.readFileSync(f, 'utf8')
  const r = rel(f)
  const boundary = boundaryOf(r)
  const specs = []
  for (const re of [LINE_IMPORT_RE, DYN_RE]) {
    re.lastIndex = 0
    let m
    while ((m = re.exec(text)) !== null) specs.push({ spec: m[1], line: text.slice(0, m.index).split('\n').length })
  }
  for (const { spec, line } of specs) {
    const res = resolveSpec(f, spec)
    if (res) {
      const t = toRel(res.abs)
      if (t) {
        const tb = boundaryOf(t)
        edgeCounts.set(boundary + '->' + tb, (edgeCounts.get(boundary + '->' + tb) ?? 0) + 1)
        if (boundary === 'shared' && tb === 'main') violations.sharedToMain.push(`${r}:${line} -> ${spec}`)
        if (boundary === 'shared' && tb === 'renderer') violations.sharedToRenderer.push(`${r}:${line} -> ${spec}`)
        if (boundary === 'renderer' && tb === 'main') violations.rendererToMain.push(`${r}:${line} -> ${spec}`)
        if (boundary === 'main' && tb === 'renderer') violations.mainToRenderer.push(`${r}:${line} -> ${spec}`)
        if (boundary === 'preload' && tb === 'main') violations.preloadToMain.push(`${r}:${line} -> ${spec}`)
      }
      continue
    }
    if (boundary === 'renderer') {
      const bare = spec.replace(/^node:/, '').split('/')[0]
      if (spec.startsWith('node:') || NODE_BUILTINS.has(bare)) {
        violations.rendererNode.push(`${r}:${line} -> ${spec}`)
        rendererNodeModules.set(bare, (rendererNodeModules.get(bare) ?? 0) + 1)
      }
    }
  }
}
console.log('\nBOUNDARY_EDGE_COUNTS ' + JSON.stringify(Object.fromEntries([...edgeCounts].sort())))
for (const [k, v] of Object.entries(violations)) {
  console.log(`\n## ${k}: ${v.length}`)
  for (const x of v.slice(0, 80)) console.log('  ' + x)
}
console.log('\nRENDERER_NODE_MODULES ' + JSON.stringify(Object.fromEntries([...rendererNodeModules].sort((a, b) => b[1] - a[1]))))

// services internal coupling
const svcEdges = new Map()
for (const f of files) {
  const r = rel(f)
  if (!r.startsWith('src/main/services/')) continue
  const fromDir = r.split('/').slice(0, 4).join('/')
  const text = fs.readFileSync(f, 'utf8')
  for (const re of [LINE_IMPORT_RE, DYN_RE]) {
    re.lastIndex = 0
    let m
    while ((m = re.exec(text)) !== null) {
      const res = resolveSpec(f, m[1])
      if (!res) continue
      const t = toRel(res.abs)
      if (!t || !t.startsWith('src/main/services/')) continue
      const toDir = t.split('/').slice(0, 4).join('/')
      if (fromDir === toDir) continue
      const key = fromDir.replace('src/main/services/', '') + ' -> ' + toDir.replace('src/main/services/', '')
      if (!svcEdges.has(key)) svcEdges.set(key, { n: 0, sample: `${r}:${text.slice(0, m.index).split('\n').length}` })
      svcEdges.get(key).n++
    }
  }
}
const pairs = [...svcEdges.entries()].sort((a, b) => b[1].n - a[1].n)
const dirNames = new Set([...pairs].flatMap(([k]) => k.split(' -> ')))
let mutual = []
for (const [k] of pairs) { const [a, b] = k.split(' -> '); if (svcEdges.has(b + ' -> ' + a)) mutual.push(k) }
console.log('\nSERVICE_DIR_EDGES total=' + pairs.length + ' dirs=' + dirNames.size)
console.log('TOP25:')
for (const [k, v] of pairs.slice(0, 25)) console.log(`  ${v.n}\t${k}\t(${v.sample})`)
console.log('\nMUTUAL_DIR_PAIRS ' + mutual.length + ':')
for (const k of mutual) console.log('  ' + k + '  vs  ' + k.split(' -> ').reverse().join(' -> '))