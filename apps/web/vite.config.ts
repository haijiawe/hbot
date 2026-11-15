import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Tauri 打包后通过自定义协议加载，资源用相对路径；dev server 同样可用。
export default defineConfig({
  base: './',
  plugins: [react()],
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: 'es2022',
    // 不产出 .map：`tauri build` 会把整个 `dist/` 内嵌进安装包，实测 58.16 MiB
    // 里有 44.20 MiB 是 sourcemap（见 docs/spikes/S5-packaging.md）。dev server
    // 自己的 sourcemap 是内存里的，不受这里影响，调试体验不降级。
    sourcemap: false,
    // Monaco 是 lazy chunk，体积大是预期内的；首屏 JS 预算见 docs/05-quality-gates.md。
    chunkSizeWarningLimit: 4096,
  },
})