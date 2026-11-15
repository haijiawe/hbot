/**
 * Web 侧单测（`pnpm test`）。
 *
 * 只测**纯逻辑**：store 对核心事件的归约、Diff 统计这类不碰 DOM 的部分。
 * 所以 `environment: 'node'` —— 不需要 jsdom，也不需要浏览器。
 * 组件的渲染测试留给后续（要引入 jsdom 时在这里加第二个 environment）。
 */

import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
})