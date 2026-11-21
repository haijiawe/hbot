# features/editor

> 代码编辑器：Monaco 表面、标签、断点装饰、内联编辑弹层。

## 职责

编辑区是 `workspaceStore` 的唯一渲染方：打开的文件、脏标记、光标、滚动位置都从这里出去。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `editor-surface.tsx` | Monaco 实例（**动态 import**，首屏不静态引入）与模型桥 |
| `editor-tabs.tsx` | 标签条（脏标记、关闭确认） |
| `editor-area.tsx` | 布局容器（空状态 / 标签 + 表面） |
| `breakpoint-decorations.ts` | 断点 / 日志点装饰（glyph margin） |
| `breakpoint-menu.ts` | 右键菜单：加条件、加日志点 |
| `inline-edit-popover.tsx` | 内联编辑（Agent 建议的一段替换） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/workspace-store.ts` | 打开的文件、内容、脏标记、标签顺序 |
| `lib/editor-model-bridge.ts` | Monaco model 与 store 的同步 |
| `lib/inline-edit.ts` | 内联编辑的请求与接受 |

## 约定

- Monaco 只能动态 import；静态引入会把首屏预算打穿（`AGENTS.md` §6）。
- 用户有未保存缓冲区时，Agent 的编辑**不得**静默覆盖（`AGENTS.md` §0.10）。
- 装饰的坐标换算是 0 基 / 1 基最容易错的地方，改这里先跑 `breakpoint-decorations.test.ts`。

## 测试

`breakpoint-decorations.test.ts` / `breakpoint-menu.test.ts` / `inline-edit-popover.test.tsx`。
