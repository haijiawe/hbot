#!/usr/bin/env node
// 校验 crates/hbot-desktop 的 Tauri 2 打包配置（结构 + 必需字段 + 图标实物 + 前端产物）。
//
// 为什么需要这个脚本：`cargo tauri build` 会下载 WiX / NSIS / AppImage 工具链，
// 在没装 tauri-cli、也不该联网的机器上跑不起来；但「配置写错了」是可以在本地零成本发现的。
// 本脚本只读文件、不联网、不写盘，用来替代 bundler 的解析步骤。
//
// 用法：
//   node scripts/check-tauri-conf.mjs                     # 严格模式（apps/web/dist 必须存在）
//   node scripts/check-tauri-conf.mjs --allow-missing-frontend-dist
//   node scripts/check-tauri-conf.mjs --json              # 机器可读摘要（给 CI / 其他脚本用）
//
// 退出码：0 = 全部通过；1 = 有错误（逐条打印）；2 = 参数错误。

import { readFileSync, existsSync, statSync, readdirSync } from 'node:fs'
import { dirname, join, resolve, relative, isAbsolute } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '..')
const crateDir = join(repoRoot, 'crates', 'hbot-desktop')
const confPath = join(crateDir, 'tauri.conf.json')

const argv = process.argv.slice(2)
const allowMissingDist = argv.includes('--allow-missing-frontend-dist')
const asJson = argv.includes('--json')
const knownFlags = new Set(['--allow-missing-frontend-dist', '--json', '--help', '-h'])
const unknown = argv.filter(a => !knownFlags.has(a))
if (argv.includes('--help') || argv.includes('-h')) {
  console.log('node scripts/check-tauri-conf.mjs [--allow-missing-frontend-dist] [--json]')
  process.exit(0)
}
if (unknown.length > 0) {
  console.error(`未知参数: ${unknown.join(' ')}`)
  process.exit(2)
}

const checks = []
const errors = []
const warnings = []

function ok(name, detail) {
  checks.push({ name, detail, level: 'ok' })
}
function fail(name, detail) {
  checks.push({ name, detail, level: 'error' })
  errors.push(`${name}${detail ? ` — ${detail}` : ''}`)
}
function warn(name, detail) {
  checks.push({ name, detail, level: 'warn' })
  warnings.push(`${name}${detail ? ` — ${detail}` : ''}`)
}
function check(cond, name, detail) {
  if (cond) ok(name, detail)
  else fail(name, detail)
  return cond
}

function readJson(file) {
  const raw = readFileSync(file, 'utf8')
  // 去掉 BOM，Tauri 的 serde_json 也不接受 BOM
  const text = raw.charCodeAt(0) === 0xfeff ? raw.slice(1) : raw
  try {
    return { value: JSON.parse(text), raw: text }
  } catch (e) {
    fail(`JSON 解析 ${relative(repoRoot, file)}`, e.message)
    return null
  }
}

// ── Tauri 2 的枚举取值（来源：tauri-utils 2.10.2 src/config.rs） ──────────────
// BundleType::Deserialize 接受 deb/rpm/appimage/msi/nsis/app/dmg（大小写不敏感）
const BUNDLE_TYPES = ['deb', 'rpm', 'appimage', 'msi', 'nsis', 'app', 'dmg']
// BundleConfig::category 的合法值（同文件 1662 行的 doc comment）
const CATEGORIES = [
  'Business', 'DeveloperTool', 'Education', 'Entertainment', 'Finance', 'Game',
  'ActionGame', 'AdventureGame', 'ArcadeGame', 'BoardGame', 'CardGame', 'CasinoGame',
  'DiceGame', 'EducationalGame', 'FamilyGame', 'KidsGame', 'MusicGame', 'PuzzleGame',
  'RacingGame', 'RolePlayingGame', 'SimulationGame', 'SportsGame', 'StrategyGame',
  'TriviaGame', 'WordGame', 'GraphicsAndDesign', 'HealthcareAndFitness', 'Lifestyle',
  'Medical', 'Music', 'News', 'Photography', 'Productivity', 'Reference',
  'SocialNetworking', 'Sports', 'Travel', 'Utility', 'Video', 'Weather',
]
// 平台 → 预期 targets（与 tauri.<platform>.conf.json 的覆盖值一一对应）
const EXPECTED_PLATFORM_TARGETS = {
  windows: ['nsis'],
  linux: ['deb', 'appimage'],
  macos: ['dmg'],
}
// bundle.icon 里必须出现的图标文件（缺一个就少一个平台的图标）
// 2026-11-16：补了 icon.icns —— macOS 的 .app/.dmg 只认 ICNS，png 顶不了。
// icon.icns 由 `cargo tauri icon crates/hbot-desktop/icons/icon-1024.png` 生成
// （输入图必须 ≥1024x1024，这是 tauri-cli 的硬要求）。
const REQUIRED_ICONS = [
  'icons/32x32.png',
  'icons/128x128.png',
  'icons/128x128@2x.png',
  'icons/icon.png',
  'icons/icon.ico',
  'icons/icon.icns',
]
// 生成整套图标的源图（不在 bundle.icon 里，只在仓库里留着以便重新生成）
const ICON_SOURCE = 'icons/icon-1024.png'
const ICON_SOURCE_MIN = 1024

// ── 1. 基础文件 ───────────────────────────────────────────────────────────────
if (!check(existsSync(confPath), 'tauri.conf.json 存在', relative(repoRoot, confPath))) {
  finish()
}
const parsed = readJson(confPath)
if (!parsed) finish()
const conf = parsed.value

check(typeof conf === 'object' && conf !== null && !Array.isArray(conf),
  'tauri.conf.json 顶层是对象')
check(typeof conf.$schema === 'string' && conf.$schema.includes('schema.tauri.app/config/2'),
  '$schema 指向 Tauri 2 配置 schema', conf.$schema)

// productName / version / identifier
check(typeof conf.productName === 'string' && conf.productName.trim() !== '',
  'productName 是非空字符串', String(conf.productName))
check(typeof conf.version === 'string' && /^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$/.test(conf.version),
  'version 是 semver 字符串', String(conf.version))
// identifier 在 0.1.0 发布前从 `com.hbot.app` 改成 `com.hbot.desktop`：
// bundler 对以 `.app` 结尾的 identifier 会告警（与 macOS 的 bundle 扩展名撞车）。
// 发布之后改它等于换应用身份（安装升级会变成两个应用），所以从这里开始冻结。
check(conf.identifier === 'com.hbot.desktop',
  'identifier 未被改动（冻结值 com.hbot.desktop）', String(conf.identifier))
check(!conf.identifier.endsWith('.app'),
  'identifier 不以 .app 结尾（避免与 macOS bundle 扩展名撞车）', String(conf.identifier))

// build.frontendDist
const frontendDist = conf.build?.frontendDist
check(typeof frontendDist === 'string' && frontendDist.trim() !== '',
  'build.frontendDist 是字符串', String(frontendDist))

// app.windows：打包不依赖窗口尺寸，但窗口必须是合法的 Tauri 2 形状
const windows = conf.app?.windows
check(Array.isArray(windows) && windows.length >= 1, 'app.windows 至少一个窗口')
if (Array.isArray(windows) && windows[0]) {
  const w = windows[0]
  check(typeof w.title === 'string' && w.title.length > 0, 'app.windows[0].title 非空', String(w.title))
  check(Number.isFinite(w.width) && Number.isFinite(w.height) && w.width > 0 && w.height > 0,
    'app.windows[0] 宽高为正数', `${w.width}x${w.height}`)
}
check(conf.app?.security !== undefined, 'app.security 段存在')

// ── 2. bundle 段 ─────────────────────────────────────────────────────────────
const bundle = conf.bundle
if (!check(bundle && typeof bundle === 'object', 'bundle 段存在')) finish()

// 2.1 active
check(bundle.active === true, 'bundle.active === true（否则 tauri build 只出裸二进制）',
  JSON.stringify(bundle.active))

// 2.2 targets
const targets = bundle.targets
const isTargetsAll = targets === 'all'
const isTargetsArray = Array.isArray(targets) &&
  targets.length > 0 &&
  targets.every(t => typeof t === 'string' && BUNDLE_TYPES.includes(t.toLowerCase()))
check(isTargetsArray || isTargetsAll,
  'bundle.targets 是合法 target 数组或 "all"', JSON.stringify(targets))
if (isTargetsArray) {
  const lower = targets.map(t => t.toLowerCase())
  // 2026-11-16：Windows 只出 NSIS（per-user，不需要管理员）。MSI 默认 per-machine，
  // 与 NSIS 的 `installMode: currentUser` 是两套权限模型，同时发会让用户在同一台机器上
  // 装出两份互不相识的应用。要恢复 MSI 必须同时决定 per-machine 的升级/卸载策略。
  check(lower.includes('nsis'),
    'base config 的 targets 覆盖 Windows（nsis；msi 待定权限模型）', lower.join('+'))
  const dupes = lower.filter((t, i) => lower.indexOf(t) !== i)
  check(dupes.length === 0, 'bundle.targets 无重复项', dupes.join(','))
}

// 2.3 元数据字段
check(typeof bundle.publisher === 'string' && bundle.publisher.trim() !== '',
  'bundle.publisher 非空（MSI Manufacturer / deb Maintainer 回退）', String(bundle.publisher))
check(typeof bundle.copyright === 'string' && bundle.copyright.trim() !== '',
  'bundle.copyright 非空', String(bundle.copyright))
check(typeof bundle.license === 'string' && bundle.license.trim() !== '',
  'bundle.license 非空（deb / dmg 会写进包元数据）', String(bundle.license))

const category = bundle.category
const categoryOk = typeof category === 'string' &&
  CATEGORIES.some(c => c.toLowerCase() === category.toLowerCase())
check(categoryOk, `bundle.category 是 Tauri 2 合法分类之一`, String(category))

const shortDesc = bundle.shortDescription
if (check(typeof shortDesc === 'string' && shortDesc.trim() !== '',
  'bundle.shortDescription 非空', String(shortDesc))) {
  const len = [...shortDesc].length
  // deb 的 Synopsis 是单行短描述；过长会被打包器/桌面环境截断
  if (len > 80) warn('shortDescription 偏长', `${len} 字符（建议 ≤ 80，deb Synopsis 是单行）`)
  else ok('shortDescription 长度合适', `${len} 字符`)
  check(!/[\r\n]/.test(shortDesc), 'shortDescription 是单行')
}
const longDesc = bundle.longDescription
if (check(typeof longDesc === 'string' && longDesc.trim() !== '',
  'bundle.longDescription 非空', longDesc ? `${[...longDesc].length} 字符` : '缺失')) {
  check([...longDesc].length >= 40, 'longDescription 足够长（≥ 40 字符）', `${[...longDesc].length} 字符`)
}

// updater 产物需要签名私钥（TAURI_SIGNING_PRIVATE_KEY），当前没有 → 必须是 false，
// 否则 cargo tauri build 会在最后一步报缺 key。
check(bundle.createUpdaterArtifacts === false,
  'createUpdaterArtifacts === false（尚无更新签名私钥）', JSON.stringify(bundle.createUpdaterArtifacts))

// 2.4 平台子段
check(typeof bundle.windows?.nsis?.installMode === 'string' &&
  ['currentUser', 'perMachine', 'both'].includes(bundle.windows.nsis.installMode),
  'bundle.windows.nsis.installMode 合法', String(bundle.windows?.nsis?.installMode))
check(bundle.linux?.deb?.section === undefined || typeof bundle.linux.deb.section === 'string',
  'bundle.linux.deb.section 是字符串', String(bundle.linux?.deb?.section))
const minSysVer = bundle.macOS?.minimumSystemVersion
check(typeof minSysVer === 'string' && /^\d+\.\d+(\.\d+)?$/.test(minSysVer),
  'bundle.macOS.minimumSystemVersion 形如 10.15', String(minSysVer))

// 2.5 icon 数组 + 实物
const icons = bundle.icon
if (check(Array.isArray(icons) && icons.length > 0, 'bundle.icon 是非空数组',
  Array.isArray(icons) ? `${icons.length} 项` : String(icons))) {
  const missing = REQUIRED_ICONS.filter(p => !icons.includes(p))
  check(missing.length === 0, `bundle.icon 列全 ${REQUIRED_ICONS.length} 个图标文件`,
    missing.length ? `缺: ${missing.join(', ')}` : REQUIRED_ICONS.join(', '))

  for (const iconPath of icons) {
    const abs = resolve(crateDir, iconPath)
    const rel = relative(crateDir, abs)
    if (rel.startsWith('..') || isAbsolute(rel)) {
      fail(`图标 ${iconPath} 必须位于 crate 目录内`, rel)
      continue
    }
    if (!existsSync(abs)) {
      fail(`图标 ${iconPath} 存在`, abs)
      continue
    }
    const info = probeIcon(abs, iconPath)
    if (info.error) fail(`图标 ${iconPath} 格式`, info.error)
    else ok(`图标 ${iconPath} 格式`, info.detail)
  }

  // macOS 需要 .icns 才能出 .app / .dmg 图标
  const hasIcns = icons.some(p => p.toLowerCase().endsWith('.icns'))
  check(hasIcns, 'macOS .icns 图标就位（app/dmg 只认 ICNS）')

  // 源图：整套图标（含 .icns）都是 `cargo tauri icon <源图>` 一次生成的，
  // 源图低于 1024x1024 时 tauri-cli 直接拒绝，等于以后再也改不出图标。
  const sourceAbs = resolve(crateDir, ICON_SOURCE)
  if (check(existsSync(sourceAbs), `图标源图存在（${ICON_SOURCE}）`, relative(crateDir, sourceAbs))) {
    const src = probeIcon(sourceAbs, ICON_SOURCE)
    if (src.error) {
      fail(`图标源图格式（${ICON_SOURCE}）`, src.error)
    } else {
      const dims = src.detail.match(/^(\d+)x(\d+)/)
      const okSize = dims && Number(dims[1]) >= ICON_SOURCE_MIN && dims[1] === dims[2]
      check(okSize, `图标源图是正方形且 ≥${ICON_SOURCE_MIN}（否则生成不了 .icns）`, src.detail)
    }
  }
}

// ── 3. frontendDist 与前端产物 ───────────────────────────────────────────────
if (typeof frontendDist === 'string' && frontendDist.trim() !== '') {
  const distDir = resolve(crateDir, frontendDist)
  const distRel = relative(repoRoot, distDir)
  const exists = existsSync(distDir) && statSync(distDir).isDirectory()
  if (!exists) {
    const msg = `缺少前端产物 ${distRel}（先跑 pnpm --dir apps/web build，或 pnpm --dir apps/web run check）`
    if (allowMissingDist) warn('frontendDist 目录存在', msg)
    else fail('frontendDist 目录存在', msg)
  } else {
    ok('frontendDist 目录存在', distRel)
    const indexHtml = join(distDir, 'index.html')
    if (check(existsSync(indexHtml), 'frontendDist/index.html 存在', relative(repoRoot, indexHtml))) {
      const html = readFileSync(indexHtml, 'utf8')
      check(/<script[^>]+src=/.test(html) || /<script[^>]*type="module"/.test(html),
        'index.html 引用了打包后的 JS', `${html.length} 字节`)
      check(!/\/src\/main\.tsx/.test(html), 'index.html 不是 dev 入口（没有 /src/main.tsx）')
    }
    const assetsDir = join(distDir, 'assets')
    if (check(existsSync(assetsDir), 'frontendDist/assets 存在', 'vite 产物目录')) {
      const entries = readdirCount(assetsDir)
      check(entries > 0, 'assets 目录非空', `${entries} 个文件`)
    }
    // 体积提示：整包 < 70 MB 的预算主要由前端产物 + 二进制决定
    const distBytes = dirBytes(distDir)
    ok('前端产物体积', `${mib(distBytes)}（会被内嵌进二进制）`)
  }
}

// ── 4. 平台覆盖配置（tauri.<platform>.conf.json） ──────────────────────────────
// tauri-cli 在 build 时把平台覆盖文件合并到 base config 上；这是 Tauri 2 表达
// 「三平台各自 targets」的唯一官方方式（base 里只有一个 targets 字段）。
for (const [platform, expected] of Object.entries(EXPECTED_PLATFORM_TARGETS)) {
  const overlayPath = join(crateDir, `tauri.${platform}.conf.json`)
  if (!check(existsSync(overlayPath), `平台覆盖存在 tauri.${platform}.conf.json`,
    relative(repoRoot, overlayPath))) continue
  const overlay = readJson(overlayPath)
  if (!overlay) continue
  const oTargets = overlay.value?.bundle?.targets
  const normalized = Array.isArray(oTargets) ? oTargets.map(t => String(t).toLowerCase()) : []
  const sameSet = normalized.length === expected.length &&
    expected.every(t => normalized.includes(t))
  check(sameSet, `tauri.${platform}.conf.json 的 targets = ${expected.join('+')}`,
    JSON.stringify(oTargets))
  const strayKeys = Object.keys(overlay.value).filter(k => !['$schema', 'bundle'].includes(k))
  check(strayKeys.length === 0,
    `tauri.${platform}.conf.json 只覆盖必要字段`, strayKeys.join(', ') || 'bundle')
}

// ── 5. 打包前置：Cargo.toml / build.rs ──────────────────────────────────────
const cargoToml = readFileSync(join(crateDir, 'Cargo.toml'), 'utf8')
check(/tauri\s*=\s*\{[^}]*version\s*=\s*"2/.test(cargoToml),
  'Cargo.toml 依赖 tauri 2')
check(/tauri-build\s*=\s*\{[^}]*version\s*=\s*"2/.test(cargoToml),
  'Cargo.toml 把 tauri-build 放在 build-dependencies')
const buildRs = readFileSync(join(crateDir, 'build.rs'), 'utf8')
check(/tauri_build::build\(\)/.test(buildRs), 'build.rs 调用 tauri_build::build()',
  '这一步会在编译期用 serde 解析 tauri.conf.json（含 deny_unknown_fields）')

// version 与 workspace 版本一致，否则安装包版本和 crate 版本会漂移
const rootCargo = readFileSync(join(repoRoot, 'Cargo.toml'), 'utf8')
const wsVersion = rootCargo.match(/\[workspace\.package\][\s\S]*?^\s*version\s*=\s*"([^"]+)"/m)?.[1]
check(wsVersion !== undefined && wsVersion === conf.version,
  'tauri.conf.json 的 version 与 workspace 版本一致',
  `config=${conf.version} workspace=${wsVersion}`)

finish()

// ── 实现 ────────────────────────────────────────────────────────────────────
function probeIcon(abs, iconPath) {
  const buf = readFileSync(abs)
  const lower = iconPath.toLowerCase()
  if (lower.endsWith('.png')) {
    // PNG 签名 + IHDR 宽高（字节 16..24）
    const sig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
    if (buf.length < 24 || !sig.every((b, i) => buf[i] === b)) {
      return { error: `${buf.length} 字节，不是 PNG（签名不符）` }
    }
    const width = buf.readUInt32BE(16)
    const height = buf.readUInt32BE(20)
    const declared = declaredPngSize(lower)
    if (declared && (declared.w !== width || declared.h !== height)) {
      return { error: `文件名声明 ${declared.w}x${declared.h}，实际 ${width}x${height}` }
    }
    return { detail: `${width}x${height}, ${buf.length} 字节` }
  }
  if (lower.endsWith('.ico')) {
    // ICO: reserved(2)=0, type(2)=1, count(2)>0
    if (buf.length < 6 || buf.readUInt16LE(0) !== 0 || buf.readUInt16LE(2) !== 1) {
      return { error: `${buf.length} 字节，不是 ICO（头 6 字节不符）` }
    }
    const count = buf.readUInt16LE(4)
    if (count === 0) return { error: 'ICO 内没有图像' }
    // 列出条目尺寸（0 表示 256）
    const sizes = []
    for (let i = 0; i < count; i++) {
      const off = 6 + i * 16
      if (off + 2 > buf.length) break
      const w = buf[off] === 0 ? 256 : buf[off]
      const h = buf[off + 1] === 0 ? 256 : buf[off + 1]
      sizes.push(`${w}x${h}`)
    }
    return { detail: `${count} 张图 [${sizes.join(', ')}], ${buf.length} 字节` }
  }
  if (lower.endsWith('.icns')) {
    // ICNS: magic "icns" + 大端总长度（长度含 8 字节头自身）
    if (buf.length < 8 || buf.toString('latin1', 0, 4) !== 'icns') {
      return { error: `${buf.length} 字节，不是 ICNS（magic 不符）` }
    }
    const declaredLen = buf.readUInt32BE(4)
    if (declaredLen !== buf.length) return { error: `头部声明 ${declaredLen} 字节，实际 ${buf.length}` }
    return { detail: `${buf.length} 字节（ICNS 头一致）` }
  }
  return { detail: `${buf.length} 字节（未校验格式）` }
}

function declaredPngSize(lower) {
  if (lower.endsWith('32x32.png')) return { w: 32, h: 32 }
  if (lower.endsWith('128x128@2x.png')) return { w: 256, h: 256 }
  if (lower.endsWith('128x128.png')) return { w: 128, h: 128 }
  return null
}

function readdirCount(dir) {
  try {
    return readdirSync(dir).length
  } catch {
    return 0
  }
}

function dirBytes(dir) {
  let total = 0
  const walk = d => {
    for (const entry of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, entry.name)
      if (entry.isDirectory()) walk(p)
      else if (entry.isFile()) total += statSync(p).size
    }
  }
  walk(dir)
  return total
}

function mib(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(2)} MiB`
}

function finish() {
  const failed = errors.length > 0
  if (asJson) {
    console.log(JSON.stringify({
      ok: !failed,
      checks: checks.map(c => ({ level: c.level, name: c.name, detail: c.detail })),
      errors,
      warnings,
      conf: relative(repoRoot, confPath),
    }, null, 2))
  } else {
    for (const c of checks) {
      const tag = c.level === 'ok' ? 'ok  ' : c.level === 'warn' ? 'warn' : 'FAIL'
      console.log(`${tag} ${c.name}${c.detail ? `: ${c.detail}` : ''}`)
    }
    console.log('')
    if (warnings.length > 0) {
      console.log(`警告 ${warnings.length} 条：`)
      for (const w of warnings) console.log(`  - ${w}`)
    }
    if (failed) {
      console.log(`失败 ${errors.length} 条：`)
      for (const e of errors) console.log(`  - ${e}`)
      console.log('\n结论：tauri.conf.json 不满足打包前置条件')
    } else {
      console.log(`结论：通过 ${checks.filter(c => c.level === 'ok').length} 项检查` +
        `（警告 ${warnings.length} 条）— 配置可被 Tauri 2 解析`)
    }
  }
  process.exit(failed ? 1 : 0)
}