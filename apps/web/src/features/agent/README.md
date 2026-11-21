# features/agent

> Agent 面板：对话、工具调用、审批卡与 Diff 卡。

## 职责

把 `agentStore` 的状态渲染出来，并把用户的"接受 / 拒绝"翻译成命令。Agent 的决策在 Rust 侧（`hbot-agent`），这里只有展示与确认。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `agent-panel.tsx` | 面板主体：消息流、工具行、结构化错误 |
| `approval-card.tsx` | 审批卡：展示 `(tool, 规范化参数, 沙箱档)` 并回传决定 |
| `diff-card.tsx` | Diff 卡：逐块接受 / 拒绝 |
| `accept-edit.ts` | 把"接受这一块"落成一次 `agent.editApply` |
| `edit-undo.test.ts` / `edit-undo.ts` | 撤销语义（每个 edit 一个撤销点） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/agent-store.ts` | 消息、待审编辑、审批队列（唯一写入者） |
| `lib/agent-requests.ts` | 组装 `agent.*` 命令 |
| `lib/edit-apply.ts` | Diff 应用到编辑器缓冲区（未保存时拒绝自动应用） |

## 约定

- 不直接改 `workspaceStore`：跨域一律走命令或事件（`AGENTS.md` §6）。
- 审批与 Diff 都不能"全部接受"式批量放行 —— 规则键含参数，逐条确认。
- 每个卡片都能在"空 / 加载 / 错误"三态下不白屏。

## 测试

`approval-card.test.tsx` / `diff-card.test.tsx` / `edit-undo.test.ts`。组件测试靠文件首行 `// @vitest-environment jsdom` 挂载。
