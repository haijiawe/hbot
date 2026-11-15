#!/usr/bin/env node
// 桌面端打包前置流水线：前端产物 → 配置校验 → release 二进制 → （可选）真跑 bundler。
//
// 默认**不**跑 bundler（`cargo tauri build` 会下载 WiX / NSIS / AppImage 工具链，
// 首次还要联网），只做到「二进制可产出、配置可解析」；真打包必须显式 --bundle。
//
// 用法：
//   node scripts/build-desktop.mjs                  # 前端 + 校验 + release 构建
//   node scripts/build-desktop.mjs --skip-web       # 复用已有 apps/web/dist
//   node scripts/build-desktop.mjs --target-dir D:\hbot-target\pkg
//   node scripts/build-desktop.mjs --bundle         # 额外真跑 cargo tauri build（需 tauri-cli）
//
// 退出码：0 = 成功；1 = 某一步失败（原样透传子进程退出码）。

import { spawnSync } from 'node:child_process'
import { existsSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '..')
const webDir = join(repoRoot, 'apps', 'web')
const distDir = join(webDir, 'dist')
const isWindows = process.platform === 'win32'

const argv = process.argv.slice(2)
const flags = {
  skipWeb: argv.includes('--skip-web'),
  bundle: argv.includes('--bundle'),
}
const targetDirArg = valueOf('--target-dir')
const targetDir = targetDirArg ? resolve(targetDirArg) : (process.env.CARGO_TARGET_DIR || join(repoRoot, 'target'))

function valueOf(name) {
  const i = argv.indexOf(name)
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : undefined
}

function step(title) {
  console.log(`\n=== ${title} ===`)
}

function run(cmd, args, opts = {}) {
  console.log(`$ ${cmd} ${args.join(' ')}`)
  // stdio: inherit —— 受限沙箱下孙进程拿不到管道 stdio，继承最稳，也保留原生颜色/进度
  const res = spawnSync(cmd, args, {
    cwd: opts.cwd ?? repoRoot,
    stdio: 'inherit',
    env: { ...process.env, ...(opts.env ?? {}) },
    shell: isWindows, // Windows 上 pnpm / cargo 都是 .cmd / .exe 包装
  })
  if (res.error) {
    console.error(`启动失败: ${res.error.message}`)
    process.exit(1)
  }
  if (res.status !== 0) {
    console.error(`失败: ${cmd} ${args.join(' ')} 退出码 ${res.status}`)
    process.exit(res.status ?? 1)
  }
}

// 1) 前端产物：Tauri 把 frontendDist 整个目录内嵌进二进制，缺了就打包失败
step('前端产物 (apps/web/dist)')
if (flags.skipWeb) {
  console.log('跳过（--skip-web）')
} else {
  const hasPnpm = spawnSync(isWindows ? 'pnpm.cmd' : 'pnpm', ['--version'], { stdio: 'ignore' }).status === 0
  if (hasPnpm) {
    run('pnpm', ['--dir', 'apps/web', 'run', 'build'])
  } else {
    console.log('未找到 pnpm，回退 npm')
    run('npm', ['--prefix', 'apps/web', 'run', 'build'])
  }
}
if (!existsSync(join(distDir, 'index.html'))) {
  console.error(`缺少 ${distDir}\\index.html —— 打包会让窗口白屏/失败，先跑 pnpm --dir apps/web build`)
  process.exit(1)
}
console.log(`dist 就绪: ${distDir}`)

// 2) 配置校验（纯本地、零依赖，替代 bundler 的解析步骤）
step('配置校验 (scripts/check-tauri-conf.mjs)')
run(process.execPath, [join(repoRoot, 'scripts', 'check-tauri-conf.mjs')])

// 3) release 二进制（build.rs 里的 tauri_build::build() 会用 serde 解析 tauri.conf.json）
step('release 构建')
if (process.env.CARGO_TARGET_DIR !== targetDir) {
  console.log(`CARGO_TARGET_DIR=${targetDir}`)
}
run('cargo', ['build', '-p', 'hbot-desktop', '--release'], {
  env: { CARGO_TARGET_DIR: targetDir },
})

const exePath = join(targetDir, 'release', isWindows ? 'hbot-desktop.exe' : 'hbot-desktop')
if (!existsSync(exePath)) {
  console.error(`构建完成但找不到产物: ${exePath}`)
  process.exit(1)
}
const bytes = statSync(exePath).size
console.log(`产物: ${exePath}`)
console.log(`体积: ${(bytes / 1024 / 1024).toFixed(2)} MiB (${bytes} 字节)`)

// 4) 可选：真跑 bundler
if (!flags.bundle) {
  console.log('\n未跑 bundler（默认行为）。真出安装包见 docs/spikes/S5-packaging.md：')
  console.log('  cargo tauri build --bundles msi,nsis        # Windows（需 cargo-tauri + WiX/NSIS 工具链）')
  console.log('  cargo tauri build --bundles deb,appimage    # Linux（需 libwebkit2gtk-4.1-dev 等系统依赖）')
  console.log('  cargo tauri build --bundles dmg             # macOS（需 Xcode CLT + .icns 图标）')
  console.log('加 --bundle 才会真跑（会下载工具链）。')
  process.exit(0)
}

step('bundler (cargo tauri build)')
run('cargo', ['tauri', 'build'], { env: { CARGO_TARGET_DIR: targetDir } })
console.log('\nbundler 完成，产物在 target/release/bundle/ 下。')