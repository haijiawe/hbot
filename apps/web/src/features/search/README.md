# features/search

> 全局搜索与替换。

## 职责

搜索表单 + 结果列表；结果点击即跳到编辑器对应行列。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `search-panel.tsx` | 面板容器 |
| `search-form.tsx` | 查询、大小写 / 正则 / 全词开关 |
| `search-results.tsx` | 结果分组与跳转 |
| `open-search.ts` | 从快捷键 / 选中文本发起搜索 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/search-store.ts` | 查询、结果、截断标记 |
| `lib/ipc.ts` | `search.run` / `search.cancel` |

## 约定

- 结果被截断时必须显式告诉用户"还有更多"，不能假装完整。
- 二进制与非 UTF-8 文件在 Rust 侧就被跳过并计数，UI 只展示计数。

## 测试

`search-panel.test.tsx` 与 `stores/search-store.test.ts`。
