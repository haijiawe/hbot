// static reachability from an entry, resolving @aliases + relative
import fs from 'node:fs'
import path from 'node:path'

const ROOT = 'E:\\H Bot 制造'
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
const IMPORT_RE = /^[ \t]*(?:import|export)\s+(?:type\s+)?(?:[\s\S]*?\s+from\s+)?['"]([^'"]+)['"]/gm

function tryFile(abs) {
  for (const c of [abs, abs + '.ts', abs + '.tsx', path.join(abs, 'index.ts'), path.join(abs, 'index.tsx')]) {
    if (fs.existsSync(c) && fs.statSync(c).isFile()) return c
  }
  return null
}
function resolve(from, spec) {
  for (const [a, r] of ALIASES) if (spec.startsWith(a)) return tryFile(path.join(ROOT, r, spec.slice(a.length)))
  if (spec.startsWith('.')) return tryFile(path.resolve(path.dirname(from), spec))
  return null
}
function reach(entry) {
  const seen = new Set()
  const stack = [entry]
  const externals = new Set()
  while (stack.length) {
    const f = stack.pop()
    if (seen.has(f)) continue
    seen.add(f)
    const text = fs.readFileSync(f, 'utf8')
    IMPORT_RE.lastIndex = 0
    let m
    while ((m = IMPORT_RE.exec(text)) !== null) {
      const spec = m[1]
      const t = resolve(f, spec)
      if (t) stack.push(t)
      else externals.add(spec)
    }
  }
  return { files: [...seen].map(f => path.relative(ROOT, f).replace(/\\/g, '/')), externals: [...externals] }
}

for (const entry of process.argv.slice(2)) {
  const abs = path.join(ROOT, entry)
  const { files, externals } = reach(abs)
  const electron = []
  const native = []
  for (const f of files) {
    const text = fs.readFileSync(path.join(ROOT, f), 'utf8')
    if (/from\s+['"]electron['"]/.test(text) || /require\(\s*['"]electron['"]\s*\)/.test(text)) {
      const lines = text.split('\n')
      const ln = lines.findIndex(l => /['"]electron['"]/.test(l) && /import|require/.test(l)) + 1
      electron.push(`${f}:${ln}`)
    }
    if (/better-sqlite3|node-pty|ssh2/.test(text)) native.push(f)
  }
  console.log(`\n=== ${entry}: reachable ${files.length} files, ${externals.length} external specs`)
  console.log('ELECTRON_STATIC_IMPORTS ' + electron.length + ':')
  for (const e of electron) console.log('  ' + e)
  console.log('NATIVE_MENTIONS ' + native.length + ':')
  for (const n of native.slice(0, 40)) console.log('  ' + n)
  console.log('EXTERNALS ' + JSON.stringify(externals.sort()))
}