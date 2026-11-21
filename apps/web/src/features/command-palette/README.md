# features/command-palette

> 命令面板（Ctrl+Shift+P）与项目对话框（新建 / 打开）。

## 职责

所有"没有按钮的入口"都在这里：命令面板列出可执行命令，项目对话框负责新建项目与按绝对路径打开项目。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `command-palette.tsx` | 面板本体：过滤、键盘导航、执行 |
| `project-dialog.tsx` | 新建 / 打开项目（路径是文本输入，没有文件选择器） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `lib/ipc.ts` 的 `dispatch` | `project.create` / `project.open` |
| `lib/project-actions.ts` | 新建 / 打开的副作用（写 store、开标签、提示） |
| `lib/keybindings.ts` | 快捷键唯一真源（组件不写死键名） |

## 约定

- 命令列表与快捷键都从 `lib/keybindings.ts` 取，不在组件里复制。
- "五分钟上手"路径依赖本 feature：Ctrl+Shift+P → 新建项目 → 试跑页签（见 `docs/quickstart-desktop.md`）。

## 测试

`command-palette.test.tsx` / `project-dialog.test.tsx`。
