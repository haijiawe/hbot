# features/explorer

> 文件树与项目视图。

## 职责

极小的一层：把 `workspaceStore` 的目录树渲染成可展开的列表，点击即打开文件。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `explorer-view.tsx` | 视图容器（空状态、刷新、新建 / 重命名入口） |
| `file-tree.tsx` | 递归树节点与懒加载目录 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/workspace-store.ts` | 树、展开状态、当前文件 |
| `lib/ipc.ts` | `files.list` / `files.create` / `files.rename` / `files.remove` |

## 约定

- 树里不显示 `.git/` 与 `.hbot/`（路径守卫在 Rust 侧也拒）。
- 删除是"移进 `.hbot/trash`"，UI 文案不要说"永久删除"。

## 测试

没有独立测试文件：树的行为由 `workspace-store.tabs.test.ts` 与手工验收覆盖。
