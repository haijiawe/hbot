/**
 * Web 侧单测（`pnpm test`）。
 *
 * 默认 `environment: 'node'`：store 归约、Diff 统计这类不碰 DOM 的逻辑仍然跑在 node 下，
 * 比 jsdom 快得多，也不会因为缺 `ResizeObserver` 之类的浏览器 API 而挂。
 *
 * 组件测试**逐文件**声明环境，不在这里全局改：
 * ```ts
 * // @vitest-environment jsdom
 * ```
 * 一行注释就够（vitest 支持文件级 environment 覆盖），代价是只让真正需要 DOM 的文件付
 * jsdom 的启动成本；反过来若全局改成 jsdom，32 个 store 测试会全部跟着变慢。
 *
 * `include` 同时收 `.test.ts` 与 `.test.tsx`：组件测试用 tsx（JSX），漏掉一条后缀就会
 * 让新增的测试文件**静默不跑**（本仓踩过这个坑：`vitest.config.mts` 收窄过一次
 * include，结果 364 个测试文件被孤立，见 AGENTS.md §5 第 1 条）。
 */

import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
})