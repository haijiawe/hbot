# `apps/web` — HBot IDE 前端（M0 壳）

React 19 + TypeScript（strict）+ Vite 8 + zustand + Monaco。样式是普通 CSS，深色主题，无 Tailwind。

## 命令

```bash
cd apps/web
pnpm install
pnpm run dev        # 开发服务器 http://localhost:1420
pnpm run typecheck  # tsc --noEmit
pnpm run check      # tsc --noEmit && vite build  ← 本目录的闸门
pnpm run build      # 仅构建
```

`apps/web` 是独立 pnpm 包（仓根没有 `pnpm-workspace.yaml`），有自己的 `pnpm-lock.yaml`。

## 目录

```
src/
  app/       外壳：App、活动栏、状态栏、错误边界、全局快捷键 hook
  components/ 共享组件（分隔条）
  features/  explorer / editor / problems / agent / run / settings / command-palette
  lib/       ipc（命令入口 + mock）、layout（布局唯一真源）、keybindings、paths、format、monaco/
  stores/    workspace-store / agent-store / run-store（仅此三个）
  styles/    theme.css（变量与 reset）、app.css（外壳与各 feature）
```

## 几个约束（改代码前先看）

1. **布局数值只有一个来源**：`src/lib/layout.ts`。组件通过 `resolveLayout()` / `clamp*()` 取值，或读根元素上的 CSS 变量（`--sidebar-width` 等，由 `layoutStyleVars()` 输出）。不要在组件里写宽度像素值。
2. **命令只有一个入口**：`src/lib/ipc.ts` 的 `dispatch(command)`。非 Tauri 环境走 `src/lib/ipc-mock.ts` 的内存 mock；接入真实后端时提供 `window.__HBOT__.dispatch`，`src/lib/ipc.ts` 会自动改走它，其余代码不用动。
   - 这里的 `Command` 联合类型是 M0 的临时契约。真实契约在 `crates/hbot-rpc/src/dto/`（`command.rs` / `event.rs`），经 `ts-rs` 生成到 `src/generated/`；届时删除手写部分并改为 re-export，不要两处维护。
3. **只有三个 store**，且不允许一个 store 直接调另一个 store 的 setter：跨域协作走 `dispatch()` 命令，或后续接入的事件流（`bot.status` / `diagnostic.changed` / `agent.diff`）。
4. **重依赖懒加载**：Monaco（`features/editor/editor-surface.tsx`）与底部面板（`features/run/bottom-panel.tsx`）都是 `React.lazy`；Monaco worker 用 Vite `?worker` 本地打包，不取 CDN（桌面离线可用）。
5. **可访问性最低线**：按钮有 `aria-label`；`:focus-visible` 有可见轮廓；日志与消息列表 `aria-live="polite"`；错误用 `role="alert"`。

## M0 的范围与已知缺口

- 搜索页签（`Ctrl+Shift+F`）、调试、真实诊断、Agent 写入落盘都**不在 M0**：活动栏搜索按钮是禁用态，问题面板是占位空状态，Diff 卡片的接受/拒绝只改本地状态（不写盘）。
- 提取文本默认走 mock 数据（`src/lib/ipc-mock.ts` 里的示例项目），因为 Tauri 侧的 `dispatch` 还没接上。
- `check` 目前只做类型与构建。仓库文档里的 UI 闸门还包括 biome 与 vitest（`docs/05-quality-gates.md`），本次任务未要求，故未引入；补的时候在 `package.json` 加 `lint` / `test` 并把 `check` 串起来即可。
- Monaco 目前整包引入（`import * as monaco from 'monaco-editor'`，含全部语言与语言服务），换来的是 find/suggest 等编辑器贡献开箱可用。代价是 Monaco chunk 与 `ts.worker` 偏大；M1 可改为按需注册语言（只需 ts/js、json、markdown、yaml）。