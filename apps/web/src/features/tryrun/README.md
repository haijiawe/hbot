# features/tryrun

> 本地试跑：选 bot → 启动 → 输入模拟消息 → 看回复。

## 职责

没有 QQ 账号也能验证 handler：`local` 平台的回复以 `[reply] ` 前缀写进 run log，本 feature 把那些行挑出来显示成对话。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `tryrun-panel.tsx` | bot 选择、启动、输入框、对话区 |
| `replies.ts` | 从日志行里提取回复（纯函数，便于测） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/tryrun-store.ts` | 当前 bot、会话、已发送的模拟消息 |
| `lib/ipc.ts` | `bot.conversations` / `bot.inject` / `bot.logs` |

## 约定

- 这是"新建项目就能用"的那条路径：项目创建时会自带一条 `local` 平台的 `bots.json` 条目（没有它 `bot.start` 会报 -32002）。
- 回复识别按前缀而不是按形状猜，改前缀等于改协议。

## 测试

`tryrun-panel.test.tsx` / `replies.test.ts`；端到端在 `crates/hbot-rpc/tests/quickstart_flow.rs`。
