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
    sourcemap: true,
    // Monaco 是 lazy chunk，体积大是预期内的；首屏 JS 预算见 docs/05-quality-gates.md。
    chunkSizeWarningLimit: 4096,
  },
})