# features/debug

> 调试：断点、日志点、调用栈与单步控制。

## 职责

把 `debugStore` 的断点状态映射成编辑器装饰与工具栏，并把单步命令发给 `hbot-rpc`。断点属于 **bot** 不属于 agent 会话（`AGENTS.md` 坑 31）。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `debug-panel.tsx` | 暂停态：调用栈、变量、继续 / 单步 |
| `debug-toolbar.tsx` | 顶部控制条：启动 / 停止 / 继续 / 单步 |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/debug-store.ts` | 断点、日志点、暂停帧（两块表分开存） |
| `lib/debug-format.ts` | 帧 / 变量格式化 |

## 约定

- 断点与日志点是**两张表**，UI 里单独加一个日志点不能清掉同文件断点（`AGENTS.md` 坑 51）。
- 条件断点是在 isolate 里求值的 JS，不是沙箱：UI 必须如实提示它能调宿主函数（坑 50）。
- 探针只在 debug 编译里存在；没有断点时不该 arm（4.64× 开销，坑 37）。

## 测试

`debug-panel.test.tsx` / `debug-toolbar.test.tsx`。
